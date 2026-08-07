//! Model-first RoutePlan (T05).
//!
//! Replaces the flat candidate queue of the legacy `Dispatcher` with a grouped,
//! model-first plan:
//!
//!   model candidates → native protocol group (G1) → in-group priority tier →
//!   same-tier weight sampling → conversion group (G2) when the native group has
//!   no candidate or only degradation-permitted failures.
//!
//! Single facade: [`authorize_and_plan`] — the ONLY entry point used by every
//! public endpoint and both stream/non-stream paths (design 6.0.1 / 11.3).
//!
//! Group matrix (design 6.0.1 table):
//!   * Chat      G1 = OpenAI `chat_completions`;  G2 = Anthropic `messages` codec.
//!   * Responses G1 = OpenAI native `responses`;   G2 = explicit `responses_via_chat_v1`.
//!   * Messages  G1 = Anthropic `messages`;        G2 = OpenAI `chat_completions` codec.
//!   * CountTokens = Anthropic `count_tokens` only.
//!   * Embeddings  = OpenAI `embeddings` only.
//!
//! Native Ollama `/api/chat` is NOT in the current matrix (T06).

use crate::core::channel_identity::{
    resolve_channel_identity, ChannelIdentity, ChannelIdentityRow,
};
use crate::core::feature_flags::FeatureFlags;
use crate::db::models::{ApiKey, Channel};
use rand::Rng;
use serde::Serialize;
use serde_json::{json, Value};

/// Default per-group attempt budget (T00 decision 4).
pub const DEFAULT_MAX_ATTEMPTS_PER_GROUP: usize = 3;
/// Default whole-request attempt budget (T00 decision 4).
pub const DEFAULT_MAX_ATTEMPTS_TOTAL: usize = 6;

/// The downstream endpoint being routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum EndpointKind {
    ChatCompletions,
    Responses,
    Messages,
    CountTokens,
    Embeddings,
}

impl EndpointKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EndpointKind::ChatCompletions => "chat_completions",
            EndpointKind::Responses => "responses",
            EndpointKind::Messages => "messages",
            EndpointKind::CountTokens => "count_tokens",
            EndpointKind::Embeddings => "embeddings",
        }
    }

    /// Map a gate `DownstreamProtocol` to a routable endpoint kind.
    /// `Completions` / `Images` / `Audio` are NOT routed by the model-first plan
    /// (they keep their existing handlers).
    pub fn from_downstream_protocol(
        protocol: crate::security::gate::DownstreamProtocol,
    ) -> Option<EndpointKind> {
        use crate::security::gate::DownstreamProtocol::*;
        match protocol {
            ChatCompletions => Some(EndpointKind::ChatCompletions),
            Responses => Some(EndpointKind::Responses),
            Messages => Some(EndpointKind::Messages),
            CountTokens => Some(EndpointKind::CountTokens),
            Embeddings => Some(EndpointKind::Embeddings),
            Completions | Images | Audio => None,
        }
    }
}

/// Upstream wire protocol of a route group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum UpstreamProtocol {
    OpenAI,
    Anthropic,
    Ollama,
}

impl UpstreamProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpstreamProtocol::OpenAI => "openai",
            UpstreamProtocol::Anthropic => "anthropic",
            UpstreamProtocol::Ollama => "ollama",
        }
    }
}

/// Whether a group is the native (passthrough) tier or an explicit conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GroupTier {
    Native,
    Conversion,
}

impl GroupTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            GroupTier::Native => "native",
            GroupTier::Conversion => "conversion",
        }
    }
}

/// One channel that survived model matching and endpoint-capability filtering.
#[derive(Debug, Clone)]
pub struct RouteGroupCandidate {
    pub channel: Channel,
    pub identity: ChannelIdentity,
    pub tier: GroupTier,
    pub upstream_protocol: UpstreamProtocol,
    pub upstream_endpoint: String,
}

/// A named bucket of candidates sharing one upstream protocol/endpoint and an
/// independent retry budget.
#[derive(Debug, Clone)]
pub struct RouteGroup {
    pub id: String,
    pub tier: GroupTier,
    pub downstream: EndpointKind,
    pub upstream_protocol: UpstreamProtocol,
    pub upstream_endpoint: String,
    pub candidates: Vec<RouteGroupCandidate>,
    /// Effective per-group attempt budget (≤ candidate count).
    pub max_attempts: usize,
}

/// The full routing plan for one request.  `groups` is already ordered
/// native-first; conversion priority/weight can never leapfrog the native group.
#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub endpoint: EndpointKind,
    /// The downstream requested model (mapping source name).
    pub model: String,
    pub groups: Vec<RouteGroup>,
    /// Whole-request attempt budget.
    pub max_attempts_total: usize,
    pub flags: FeatureFlags,
    /// Channels dropped for identity/config problems (logged, never routed).
    pub config_errors: Vec<String>,
    /// Responses requests carrying `background`/`store` disable automatic retry.
    pub non_idempotent: bool,
}

/// Authorization / planning failure.  These all fail closed BEFORE any upstream
/// access (design 11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    KeyDisabled,
    KeyExpired,
    QuotaExceeded,
    ModelNotAllowed(String),
    NoChannels,
    NoCandidateForModel(String),
    /// Model candidates exist but none supports this endpoint.
    /// Carries the endpoint so the HTTP status follows design 6.3:
    /// 503 for chat_completions/responses/messages (gateway-wide unavailability),
    /// 501 for count_tokens/embeddings (capability not offered).
    NoEndpointSupported(EndpointKind, String),
}

impl PlanError {
    pub fn http_status(&self) -> u16 {
        match self {
            PlanError::KeyDisabled => 401,
            PlanError::KeyExpired => 401,
            PlanError::QuotaExceeded => 429,
            PlanError::ModelNotAllowed(_) => 403,
            PlanError::NoChannels => 503,
            PlanError::NoCandidateForModel(_) => 503,
            PlanError::NoEndpointSupported(endpoint, _) => match endpoint {
                // Design 6.3: Chat/Responses/Messages unavailable → 503.
                EndpointKind::ChatCompletions
                | EndpointKind::Responses
                | EndpointKind::Messages => 503,
                // Design 6.3: CountTokens keeps its current 501 semantics;
                // Embeddings likewise (no codec/conversion path exists).
                EndpointKind::CountTokens | EndpointKind::Embeddings => 501,
            },
        }
    }

    pub fn message(&self) -> String {
        match self {
            PlanError::KeyDisabled => "API key is disabled".to_string(),
            PlanError::KeyExpired => "API key has expired".to_string(),
            PlanError::QuotaExceeded => "Quota exceeded".to_string(),
            PlanError::ModelNotAllowed(m) => {
                format!("Model '{}' is not allowed for this API key", m)
            }
            PlanError::NoChannels => "No available channels".to_string(),
            PlanError::NoCandidateForModel(m) => {
                format!("No channel available for model: {}", m)
            }
            PlanError::NoEndpointSupported(_endpoint, m) => {
                format!("No channel supports this endpoint for model: {}", m)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Weighted ordering (shared with the legacy Dispatcher so semantics stay exact)
// ---------------------------------------------------------------------------

/// Anything with a `priority` (tier) and `weight` (same-tier sampling).
pub trait HasPriorityWeight {
    fn priority(&self) -> i64;
    fn weight(&self) -> i64;
}

impl HasPriorityWeight for Channel {
    fn priority(&self) -> i64 {
        self.priority
    }
    fn weight(&self) -> i64 {
        self.weight
    }
}

impl HasPriorityWeight for RouteGroupCandidate {
    fn priority(&self) -> i64 {
        self.channel.priority
    }
    fn weight(&self) -> i64 {
        self.channel.weight
    }
}

/// Order candidates by priority descending; within a priority tier, sample by
/// weight WITHOUT replacement (same semantics as the legacy Dispatcher).
pub fn order_by_priority_weight<T, R>(mut candidates: Vec<T>, rng: &mut R) -> Vec<T>
where
    T: HasPriorityWeight + Clone,
    R: Rng + ?Sized,
{
    if candidates.is_empty() {
        return candidates;
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.priority()));
    let mut ordered = Vec::with_capacity(candidates.len());
    let mut start = 0;
    while start < candidates.len() {
        let priority = candidates[start].priority();
        let mut end = start;
        while end < candidates.len() && candidates[end].priority() == priority {
            end += 1;
        }
        let mut group = candidates[start..end].to_vec();
        while !group.is_empty() {
            let total_weight: i64 = group.iter().map(|c| c.weight().max(0)).sum();
            let index = if total_weight > 0 {
                let mut point = rng.random_range(0..total_weight);
                let mut selected = 0;
                for (idx, c) in group.iter().enumerate() {
                    point -= c.weight().max(0);
                    if point < 0 {
                        selected = idx;
                        break;
                    }
                }
                selected
            } else {
                0
            };
            ordered.push(group.remove(index));
        }
        start = end;
    }
    ordered
}

// ---------------------------------------------------------------------------
// Model mapping resolution (sampled EXACTLY ONCE per attempt)
// ---------------------------------------------------------------------------

/// Resolve the upstream model from a channel's `model_mapping`.
///
/// * string value  → used verbatim;
/// * array value   → sampled WITHOUT replacement exactly once per attempt;
/// * no mapping    → the requested model.
///
/// The returned model is the single source of truth for the attempt body, logs
/// and stats (design 11.4: "解析后的 upstream_model 同时传给适配器、日志和统计").
pub fn resolve_upstream_model<R: Rng + ?Sized>(
    mapping: &Value,
    model: &str,
    rng: &mut R,
) -> String {
    if let Some(mapped) = mapping.get(model) {
        if let Some(s) = mapped.as_str() {
            return s.to_string();
        }
        if let Some(arr) = mapped.as_array() {
            let models: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if !models.is_empty() {
                let idx = rng.random_range(0..models.len());
                return models[idx].clone();
            }
        }
    }
    model.to_string()
}

// ---------------------------------------------------------------------------
// Authorization (design 11.3)
// ---------------------------------------------------------------------------

/// Authorize a request before any candidate construction.  Empty allowed arrays
/// mean "no restriction" (T00 decision 3).
///
/// `model` is the downstream request model / mapping source name.
pub fn authorize_request(api_key: &ApiKey, model: &str) -> Result<(), PlanError> {
    if api_key.status != 1 {
        return Err(PlanError::KeyDisabled);
    }
    if let Some(expires) = api_key.expires_at.as_deref() {
        if !expires.trim().is_empty() && is_expired(expires) {
            return Err(PlanError::KeyExpired);
        }
    }
    if api_key.quota_limit > 0 && api_key.quota_used >= api_key.quota_limit {
        return Err(PlanError::QuotaExceeded);
    }
    let allowed: Vec<String> = serde_json::from_str(&api_key.allowed_models).unwrap_or_default();
    if !allowed.is_empty() && !allowed.iter().any(|m| m == model) {
        return Err(PlanError::ModelNotAllowed(model.to_string()));
    }
    Ok(())
}

fn is_expired(iso: &str) -> bool {
    use chrono::{DateTime, NaiveDateTime, Utc};
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&Utc) < Utc::now();
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
        return dt < Utc::now().naive_utc();
    }
    if let Ok(dt) = chrono::NaiveDate::parse_from_str(iso, "%Y-%m-%d") {
        return dt
            .and_hms_opt(23, 59, 59)
            .unwrap_or(dt.and_hms_opt(0, 0, 0).unwrap())
            < Utc::now().naive_utc();
    }
    // Unparseable expiry is treated as "not expired" (fail open on format,
    // fail closed on the check itself).
    false
}

// ---------------------------------------------------------------------------
// Model candidates (design 6.0.1 / 11.3)
// ---------------------------------------------------------------------------

/// Keep only channels that are enabled, allowed by the API key, and match the
/// requested model (either `models` contains it, `model_mapping` has it as a
/// source name, or legacy `models=[]` wildcard).
pub fn resolve_model_candidates<'a>(
    channels: &'a [Channel],
    model: &str,
    api_key: &ApiKey,
) -> Vec<&'a Channel> {
    let allowed_channels: Vec<String> =
        serde_json::from_str(&api_key.allowed_channels).unwrap_or_default();
    channels
        .iter()
        .filter(|c| c.status == 1)
        // allowed_channels filter happens BEFORE model matching (design 11.3).
        .filter(|c| allowed_channels.is_empty() || allowed_channels.contains(&c.id))
        .filter(|c| channel_accepts_model(c, model))
        .collect()
}

fn channel_accepts_model(channel: &Channel, model: &str) -> bool {
    let models: Vec<String> = serde_json::from_str(&channel.models).unwrap_or_default();
    if models.is_empty() {
        // T00 decision 3: empty models = wildcard (accepts any request model).
        return true;
    }
    if models.iter().any(|m| m == model) {
        return true;
    }
    // Mapping source names also count as hits.
    let mapping: Value = serde_json::from_str(&channel.model_mapping).unwrap_or_default();
    mapping
        .as_object()
        .map(|o| o.contains_key(model))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Group building
// ---------------------------------------------------------------------------

/// True for Responses requests that carry a remote side effect and must not be
/// retried automatically (T00 decision 5).
///
/// Only a TRUTHY `background: true` / `store: true` disables retry: a present
/// but false `store: false` is not a remote side effect.  T00's broader "具有
/// 远端副作用的" beyond background/store is not generically detectable from the
/// request body — documented limitation (the known non-idempotent knobs are
/// background and store).
pub fn is_non_idempotent_responses(endpoint: EndpointKind, body: &Value) -> bool {
    if endpoint != EndpointKind::Responses {
        return false;
    }
    body.get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || body.get("store").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Whether the channel's legacy config records the Responses→Chat debt
/// (`config.legacy_capabilities=["responses_via_chat_v1"]`, design 11.2).
pub fn has_responses_debt(channel: &Channel) -> bool {
    let config: Value = serde_json::from_str(&channel.config).unwrap_or_default();
    config
        .get("legacy_capabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|s| s.as_str() == Some("responses_via_chat_v1"))
        })
        .unwrap_or(false)
}

/// Classify a channel into (tier, upstream protocol, upstream endpoint) for the
/// given downstream endpoint, or `None` if the channel cannot serve it.
fn classify_channel(
    endpoint: EndpointKind,
    id: &ChannelIdentity,
    channel: &Channel,
    flags: &FeatureFlags,
) -> Option<(GroupTier, UpstreamProtocol, String)> {
    let has = |ep: &str| id.native_endpoints.iter().any(|e| e == ep);
    match endpoint {
        EndpointKind::ChatCompletions => {
            if id.protocol == "openai" && has("chat_completions") {
                // Native OpenAI-compatible chat (incl. OpenAI-compat Ollama).
                Some((
                    GroupTier::Native,
                    UpstreamProtocol::OpenAI,
                    "chat_completions".into(),
                ))
            } else if id.protocol == "ollama" && has("api_chat") && flags.ollama_native {
                // Native Ollama `/api/chat` (T06 executor).  OFF until the
                // executor + downstream Chat chain pass their tests.
                Some((
                    GroupTier::Native,
                    UpstreamProtocol::Ollama,
                    "api_chat".into(),
                ))
            } else if id.protocol == "anthropic" && has("messages") && flags.cross_protocol_codec {
                Some((
                    GroupTier::Conversion,
                    UpstreamProtocol::Anthropic,
                    "messages".into(),
                ))
            } else {
                None
            }
        }
        EndpointKind::Responses => {
            // A channel carries the Responses→Chat debt when its config records
            // it explicitly OR when it is a legacy-inferred openai/custom row
            // (revision-0 era) that predates the native /responses path.  The
            // latter restores the pre-refactor de facto behavior (design 11.2).
            let debt = has_responses_debt(channel)
                || (id.inferred
                    && id.identity_revision == 0
                    && id.protocol == "openai"
                    && !has("responses"));
            if !debt && id.protocol == "openai" && has("responses") && flags.native_responses {
                // Native /responses passthrough.
                Some((
                    GroupTier::Native,
                    UpstreamProtocol::OpenAI,
                    "responses".into(),
                ))
            } else if debt && flags.cross_protocol_codec {
                // Explicit legacy Responses→Chat debt.
                Some((
                    GroupTier::Conversion,
                    UpstreamProtocol::OpenAI,
                    "chat_completions".into(),
                ))
            } else {
                None
            }
        }
        EndpointKind::Messages => {
            if id.protocol == "anthropic" && has("messages") {
                Some((
                    GroupTier::Native,
                    UpstreamProtocol::Anthropic,
                    "messages".into(),
                ))
            } else if id.protocol == "openai"
                && has("chat_completions")
                && flags.cross_protocol_codec
            {
                Some((
                    GroupTier::Conversion,
                    UpstreamProtocol::OpenAI,
                    "chat_completions".into(),
                ))
            } else {
                None
            }
        }
        EndpointKind::CountTokens => {
            if id.protocol == "anthropic" && has("count_tokens") {
                Some((
                    GroupTier::Native,
                    UpstreamProtocol::Anthropic,
                    "count_tokens".into(),
                ))
            } else {
                None
            }
        }
        EndpointKind::Embeddings => {
            if id.protocol == "openai" && has("embeddings") {
                Some((
                    GroupTier::Native,
                    UpstreamProtocol::OpenAI,
                    "embeddings".into(),
                ))
            } else {
                None
            }
        }
    }
}

/// Build the ordered group plan from the surviving model candidates.
fn build_route_plan<R: Rng + ?Sized>(
    endpoint: EndpointKind,
    model: &str,
    candidates: Vec<&Channel>,
    flags: &FeatureFlags,
    body: &Value,
    rng: &mut R,
) -> Result<RoutePlan, PlanError> {
    let mut native: Vec<RouteGroupCandidate> = Vec::new();
    let mut conversion: Vec<RouteGroupCandidate> = Vec::new();
    let mut config_errors = Vec::new();

    for ch in candidates {
        let row = ChannelIdentityRow::from(ch);
        let id = resolve_channel_identity(&row);
        if id.native_base_url.is_empty() && id.native_endpoints.is_empty() {
            config_errors.push(format!(
                "channel '{}' ({}): native identity not inferable",
                ch.name, ch.id
            ));
            continue;
        }
        if let Some((tier, proto, ep)) = classify_channel(endpoint, &id, ch, flags) {
            let candidate = RouteGroupCandidate {
                channel: ch.clone(),
                identity: id,
                tier,
                upstream_protocol: proto,
                upstream_endpoint: ep,
            };
            match tier {
                GroupTier::Native => native.push(candidate),
                GroupTier::Conversion => conversion.push(candidate),
            }
        }
    }

    // Responses with remote side effects: no automatic retry (T00 decision 5).
    let non_idempotent = is_non_idempotent_responses(endpoint, body);
    let per_group = if non_idempotent {
        1
    } else {
        DEFAULT_MAX_ATTEMPTS_PER_GROUP
    };

    let mut groups = Vec::new();
    if !native.is_empty() {
        let ordered = order_by_priority_weight(native, rng);
        let max_attempts = per_group.min(ordered.len()).max(1);
        let first = &ordered[0];
        groups.push(RouteGroup {
            id: format!("{}_g1_native", endpoint.as_str()),
            tier: GroupTier::Native,
            downstream: endpoint,
            upstream_protocol: first.upstream_protocol,
            upstream_endpoint: first.upstream_endpoint.clone(),
            max_attempts,
            candidates: ordered,
        });
    }
    if !conversion.is_empty() {
        let ordered = order_by_priority_weight(conversion, rng);
        let max_attempts = per_group.min(ordered.len()).max(1);
        let first = &ordered[0];
        groups.push(RouteGroup {
            id: format!("{}_g2_conversion", endpoint.as_str()),
            tier: GroupTier::Conversion,
            downstream: endpoint,
            upstream_protocol: first.upstream_protocol,
            upstream_endpoint: first.upstream_endpoint.clone(),
            max_attempts,
            candidates: ordered,
        });
    }

    if groups.is_empty() {
        return Err(PlanError::NoEndpointSupported(endpoint, model.to_string()));
    }

    let total = if non_idempotent {
        1
    } else {
        DEFAULT_MAX_ATTEMPTS_TOTAL
    };
    let total_attempts = total
        .min(groups.iter().map(|g| g.max_attempts).sum::<usize>())
        .max(1);

    Ok(RoutePlan {
        endpoint,
        model: model.to_string(),
        groups,
        max_attempts_total: total_attempts,
        flags: *flags,
        config_errors,
        non_idempotent,
    })
}

// ---------------------------------------------------------------------------
// The facade (design 6.0.1 / 11.3): authorize_and_plan
// ---------------------------------------------------------------------------

/// THE single routing facade used by every public endpoint and both stream and
/// non-stream paths.
///
/// Order of operations (design 6.0.1 / 11.3):
/// 1. `authorize_request` — status / expires_at / quota / allowed_models;
/// 2. `allowed_channels` filter (before model matching);
/// 3. model candidates (`models` hit / `model_mapping` source hit / wildcard);
/// 4. protocol grouping (native G1 first, then conversion G2);
/// 5. endpoint capability filtering;
/// 6. in-group priority tier + same-tier weight sampling (no replacement).
///
/// `body` is the gate's forwarded request JSON (used for non-idempotency
/// detection on Responses).  `rng` is injected so tests can seed a deterministic
/// RNG; production passes `&mut rand::rng()`.
pub fn authorize_and_plan<R: Rng + ?Sized>(
    api_key: &ApiKey,
    model: &str,
    endpoint: EndpointKind,
    channels: &[Channel],
    flags: &FeatureFlags,
    body: &Value,
    rng: &mut R,
) -> Result<RoutePlan, PlanError> {
    authorize_request(api_key, model)?;
    if channels.is_empty() {
        return Err(PlanError::NoChannels);
    }
    let candidates = resolve_model_candidates(channels, model, api_key);
    if candidates.is_empty() {
        return Err(PlanError::NoCandidateForModel(model.to_string()));
    }
    build_route_plan(endpoint, model, candidates, flags, body, rng)
}

impl RoutePlan {
    /// Sanitized JSON snapshot for logs/reports.  NEVER serializes channel
    /// api_keys or secrets — only id/name/priority/weight.
    pub fn debug_json(&self) -> Value {
        json!({
            "endpoint": self.endpoint.as_str(),
            "model": self.model,
            "max_attempts_total": self.max_attempts_total,
            "non_idempotent": self.non_idempotent,
            "flags": {
                "new_routeplan": self.flags.new_routeplan,
                "cross_protocol_codec": self.flags.cross_protocol_codec,
                "native_responses": self.flags.native_responses,
                "ollama_native": self.flags.ollama_native,
            },
            "config_errors": self.config_errors,
            "groups": self.groups.iter().map(|g| {
                json!({
                    "id": g.id,
                    "tier": g.tier.as_str(),
                    "upstream_protocol": g.upstream_protocol.as_str(),
                    "upstream_endpoint": g.upstream_endpoint,
                    "max_attempts": g.max_attempts,
                    "candidates": g.candidates.iter().map(|c| {
                        json!({
                            "channel_id": c.channel.id,
                            "channel_name": c.channel.name,
                            "priority": c.channel.priority,
                            "weight": c.channel.weight,
                        })
                    }).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn api_key(allowed_models: &[&str], allowed_channels: &[&str]) -> ApiKey {
        ApiKey {
            id: "key-1".into(),
            name: "test".into(),
            key: "sk-test".into(),
            status: 1,
            allowed_models: serde_json::to_string(allowed_models).unwrap(),
            allowed_channels: serde_json::to_string(allowed_channels).unwrap(),
            quota_limit: 0,
            quota_used: 0,
            expires_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn channel(
        id: &str,
        channel_type: &str,
        base_url: &str,
        models: &[&str],
        priority: i64,
        weight: i64,
        config: &str,
    ) -> Channel {
        Channel {
            id: id.into(),
            name: format!("ch-{}", id),
            channel_type: channel_type.into(),
            base_url: base_url.into(),
            api_key: "sk-test".into(),
            models: serde_json::to_string(
                &models.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .unwrap(),
            status: 1,
            priority,
            weight,
            config: config.into(),
            model_mapping: "{}".into(),
            timeout_secs: 30,
            protocol: None,
            provider: None,
            native_base_url: None,
            native_endpoints: None,
            preset_revision: None,
            identity_revision: 0,
            legacy_executor_override: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_test_at: None,
            last_test_ok: None,
        }
    }

    /// A channel written by the new dual-write path (identity_revision > 0)
    /// with explicit native endpoints.  Needed because legacy rows only report
    /// native `responses`/`count_tokens` when their legacy debt/resolver says so.
    #[allow(clippy::too_many_arguments)]
    fn new_channel(
        id: &str,
        protocol: &str,
        provider: &str,
        native_base_url: &str,
        native_endpoints: &[&str],
        priority: i64,
        weight: i64,
    ) -> Channel {
        Channel {
            id: id.into(),
            name: format!("ch-{}", id),
            channel_type: if protocol == "anthropic" {
                "claude"
            } else {
                "openai"
            }
            .into(),
            base_url: native_base_url.into(),
            api_key: "sk-test".into(),
            models: json!(["m"]).to_string(),
            status: 1,
            priority,
            weight,
            config: "{}".into(),
            model_mapping: "{}".into(),
            timeout_secs: 30,
            protocol: Some(protocol.into()),
            provider: Some(provider.into()),
            native_base_url: Some(native_base_url.into()),
            native_endpoints: Some(serde_json::to_string(native_endpoints).unwrap()),
            preset_revision: Some("2026-08-04".into()),
            identity_revision: 1,
            legacy_executor_override: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_test_at: None,
            last_test_ok: None,
        }
    }

    fn seeded() -> StdRng {
        StdRng::seed_from_u64(0x5EED)
    }

    fn flags(codec_on: bool) -> FeatureFlags {
        FeatureFlags {
            new_routeplan: true,
            cross_protocol_codec: codec_on,
            native_responses: true,
            ollama_native: false,
        }
    }

    // --- authorization ---

    #[test]
    fn authorize_empty_allowed_models_is_unrestricted() {
        let key = api_key(&[], &[]);
        assert_eq!(authorize_request(&key, "gpt-4o"), Ok(()));
    }

    #[test]
    fn authorize_rejects_model_outside_allowed() {
        let key = api_key(&["gpt-4o"], &[]);
        assert_eq!(
            authorize_request(&key, "claude-sonnet-4-6"),
            Err(PlanError::ModelNotAllowed("claude-sonnet-4-6".into()))
        );
    }

    #[test]
    fn authorize_accepts_model_in_allowed() {
        let key = api_key(&["gpt-4o"], &[]);
        assert_eq!(authorize_request(&key, "gpt-4o"), Ok(()));
    }

    #[test]
    fn authorize_rejects_disabled_key() {
        let mut key = api_key(&[], &[]);
        key.status = 0;
        assert_eq!(authorize_request(&key, "m"), Err(PlanError::KeyDisabled));
    }

    #[test]
    fn authorize_rejects_quota() {
        let mut key = api_key(&[], &[]);
        key.quota_limit = 100;
        key.quota_used = 100;
        assert_eq!(authorize_request(&key, "m"), Err(PlanError::QuotaExceeded));
    }

    #[test]
    fn authorize_rejects_expired_key() {
        let mut key = api_key(&[], &[]);
        key.expires_at = Some("2000-01-01T00:00:00Z".into());
        assert_eq!(authorize_request(&key, "m"), Err(PlanError::KeyExpired));
    }

    #[test]
    fn authorize_ignores_empty_expiry() {
        let mut key = api_key(&[], &[]);
        key.expires_at = Some("".into());
        assert_eq!(authorize_request(&key, "m"), Ok(()));
    }

    // --- model candidates ---

    #[test]
    fn wildcard_models_and_allowed_channels_filter() {
        let c1 = channel("c1", "openai", "https://api.openai.com/v1", &[], 1, 1, "{}");
        let c2 = channel(
            "c2",
            "openai",
            "https://api.openai.com/v1",
            &["gpt-4o"],
            1,
            1,
            "{}",
        );
        let key = api_key(&[], &["c1"]);
        let all = vec![c1.clone(), c2.clone()];
        let cands = resolve_model_candidates(&all, "gpt-4o", &key);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "c1");
        assert_eq!(cands[0].models, "[]");
    }

    #[test]
    fn model_mapping_source_name_hits() {
        let c1 = channel(
            "c1",
            "openai",
            "https://api.openai.com/v1",
            &["other"],
            1,
            1,
            "{}",
        );
        let mut c1 = c1;
        c1.model_mapping = serde_json::json!({ "alias-x": "gpt-4o" }).to_string();
        let key = api_key(&[], &[]);
        let all = vec![c1];
        let cands = resolve_model_candidates(&all, "alias-x", &key);
        assert_eq!(cands.len(), 1);
    }

    // --- ordering ---

    #[test]
    fn higher_priority_first_but_conversion_never_leapfrogs_native() {
        // Native candidate low priority; conversion candidate high priority.
        let native = channel(
            "n1",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let conv = channel(
            "c1",
            "claude",
            "https://api.anthropic.com/v1",
            &["m"],
            100,
            100,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[native, conv],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 2);
        assert_eq!(plan.groups[0].tier, GroupTier::Native);
        assert_eq!(plan.groups[1].tier, GroupTier::Conversion);
        // The first attempt must be the native candidate, not the higher-prio
        // conversion one.
        assert_eq!(plan.groups[0].candidates[0].channel.id, "n1");
        // Conversion group keeps its own priority ordering internally.
        assert_eq!(plan.groups[1].candidates[0].channel.id, "c1");
    }

    #[test]
    fn no_native_candidate_goes_straight_to_conversion() {
        let conv = channel(
            "c1",
            "claude",
            "https://api.anthropic.com/v1",
            &["m"],
            5,
            5,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[conv],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].tier, GroupTier::Conversion);
    }

    #[test]
    fn weight_sampling_is_deterministic_with_seed() {
        let channels: Vec<Channel> = (0..4)
            .map(|i| {
                channel(
                    &format!("c{}", i),
                    "openai",
                    "https://api.openai.com/v1",
                    &["m"],
                    10,
                    10,
                    "{}",
                )
            })
            .collect();
        let key = api_key(&[], &[]);
        let plan_a = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &channels,
            &flags(true),
            &json!({}),
            &mut StdRng::seed_from_u64(42),
        )
        .unwrap();
        let plan_b = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &channels,
            &flags(true),
            &json!({}),
            &mut StdRng::seed_from_u64(42),
        )
        .unwrap();
        let ids_a: Vec<&str> = plan_a.groups[0]
            .candidates
            .iter()
            .map(|c| c.channel.id.as_str())
            .collect();
        let ids_b: Vec<&str> = plan_b.groups[0]
            .candidates
            .iter()
            .map(|c| c.channel.id.as_str())
            .collect();
        assert_eq!(ids_a, ids_b);
        // With equal weights every channel appears exactly once.
        assert_eq!(ids_a.len(), 4);
    }

    #[test]
    fn different_seeds_differ_in_weight_order() {
        let channels: Vec<Channel> = (0..4)
            .map(|i| {
                channel(
                    &format!("c{}", i),
                    "openai",
                    "https://api.openai.com/v1",
                    &["m"],
                    10,
                    10,
                    "{}",
                )
            })
            .collect();
        let key = api_key(&[], &[]);
        let plan_a = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &channels,
            &flags(true),
            &json!({}),
            &mut StdRng::seed_from_u64(1),
        )
        .unwrap();
        let plan_b = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &channels,
            &flags(true),
            &json!({}),
            &mut StdRng::seed_from_u64(2),
        )
        .unwrap();
        let ids_a: Vec<&str> = plan_a.groups[0]
            .candidates
            .iter()
            .map(|c| c.channel.id.as_str())
            .collect();
        let ids_b: Vec<&str> = plan_b.groups[0]
            .candidates
            .iter()
            .map(|c| c.channel.id.as_str())
            .collect();
        assert_ne!(
            ids_a, ids_b,
            "two seeds should produce different weight orders"
        );
    }

    #[test]
    fn priority_tiers_respect_priority_desc() {
        let c_hi = channel(
            "hi",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            50,
            1,
            "{}",
        );
        let c_mid = channel(
            "mid",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            30,
            1,
            "{}",
        );
        let c_lo = channel(
            "lo",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            10,
            1,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[c_lo, c_hi, c_mid],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        let ids: Vec<&str> = plan.groups[0]
            .candidates
            .iter()
            .map(|c| c.channel.id.as_str())
            .collect();
        // High priority must come before low priority regardless of weight.
        assert_eq!(ids[0], "hi");
        assert_eq!(ids[1], "mid");
        assert_eq!(ids[2], "lo");
    }

    // --- Responses matrix ---

    #[test]
    fn responses_native_group_gated_by_native_responses_flag() {
        let native = new_channel(
            "n1",
            "openai",
            "openai",
            "https://api.openai.com/v1",
            &["chat_completions", "responses"],
            1,
            1,
        );
        let key = api_key(&[], &[]);
        let on = flags(true);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            std::slice::from_ref(&native),
            &on,
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups[0].tier, GroupTier::Native);
        assert_eq!(plan.groups[0].upstream_endpoint, "responses");
        // native_responses OFF → the native Responses group disappears.
        let off = FeatureFlags {
            native_responses: false,
            ..flags(true)
        };
        let err = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[native],
            &off,
            &json!({}),
            &mut seeded(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            PlanError::NoEndpointSupported(EndpointKind::Responses, "m".into())
        );
        // F6: Responses unavailability → 503 (design 6.3), not 501.
        assert_eq!(err.http_status(), 503);
    }

    #[test]
    fn legacy_openai_row_gets_responses_debt_at_routing() {
        // A revision-0 openai/custom row (no native identity, no explicit
        // legacy_capabilities flag) must still route /v1/responses through the
        // Responses→Chat debt path (G2), restoring the pre-refactor behavior
        // (design 11.2) instead of being silently dropped.
        let legacy = channel(
            "legacy",
            "openai",
            "https://gw.example.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[legacy],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].tier, GroupTier::Conversion);
        assert_eq!(plan.groups[0].upstream_endpoint, "chat_completions");
    }

    fn responses_debt_channel_goes_to_g2_not_g1() {
        let debt = channel(
            "legacy",
            "openai",
            "https://gw.example.com/v1",
            &["m"],
            1,
            1,
            r#"{"legacy_capabilities":["responses_via_chat_v1"]}"#,
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[debt],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].tier, GroupTier::Conversion);
        assert_eq!(plan.groups[0].upstream_endpoint, "chat_completions");
    }

    #[test]
    fn non_idempotent_responses_disable_retries() {
        let n1 = new_channel(
            "n1",
            "openai",
            "openai",
            "https://api.openai.com/v1",
            &["chat_completions", "responses"],
            1,
            1,
        );
        let n2 = new_channel(
            "n2",
            "openai",
            "openai",
            "https://api.openai.com/v1",
            &["chat_completions", "responses"],
            1,
            1,
        );
        let key = api_key(&[], &[]);
        // background=true → single attempt even though two candidates exist.
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[n1.clone(), n2.clone()],
            &flags(true),
            &json!({ "background": true }),
            &mut seeded(),
        )
        .unwrap();
        assert!(plan.non_idempotent);
        assert_eq!(plan.max_attempts_total, 1);
        assert_eq!(plan.groups[0].max_attempts, 1);

        // No side effect → retries allowed (2 candidates → 2 attempts).
        let plan2 = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[n1.clone(), n2.clone()],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert!(!plan2.non_idempotent);
        assert_eq!(plan2.max_attempts_total, 2);

        // F5: `store: false` is NOT a remote side effect → retries stay on.
        let plan3 = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[n1.clone(), n2.clone()],
            &flags(true),
            &json!({ "store": false }),
            &mut seeded(),
        )
        .unwrap();
        assert!(!plan3.non_idempotent, "store:false must stay retryable");
        assert_eq!(plan3.max_attempts_total, 2);

        // `store: true` IS a side effect → retries disabled.
        let plan4 = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Responses,
            &[n1, n2],
            &flags(true),
            &json!({ "store": true }),
            &mut seeded(),
        )
        .unwrap();
        assert!(plan4.non_idempotent);
        assert_eq!(plan4.max_attempts_total, 1);
    }

    #[test]
    fn endpoint_unavailable_status_is_503_for_chat_and_501_for_count_tokens() {
        // F6 (leader-ratified, design 6.3): no channel supporting the endpoint
        // → 503 for Chat/Responses/Messages; 501 only for CountTokens.
        let key = api_key(&[], &[]);
        // Chat with only an Anthropic channel, codec OFF → no group at all.
        let ant = channel(
            "a1",
            "claude",
            "https://api.anthropic.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let off_codec = FeatureFlags {
            cross_protocol_codec: false,
            native_responses: true,
            ..FeatureFlags::all_on()
        };
        let err = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[ant],
            &off_codec,
            &json!({}),
            &mut seeded(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlanError::NoEndpointSupported(EndpointKind::ChatCompletions, _)
        ));
        assert_eq!(err.http_status(), 503);

        // CountTokens with no anthropic count_tokens channel → 501.
        let oai = channel(
            "o1",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let err = authorize_and_plan(
            &key,
            "m",
            EndpointKind::CountTokens,
            &[oai],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlanError::NoEndpointSupported(EndpointKind::CountTokens, _)
        ));
        assert_eq!(err.http_status(), 501);
    }

    // --- Ollama native (T06) ---

    #[test]
    fn ollama_native_chat_group_is_gated_by_flag() {
        let ollama = new_channel(
            "o1",
            "ollama",
            "ollama",
            "http://localhost:11434",
            &["api_chat"],
            1,
            1,
        );
        let key = api_key(&[], &[]);
        // Flag OFF → no candidate (deferred until executor+codec tests pass).
        let off = FeatureFlags {
            ollama_native: false,
            ..flags(true)
        };
        let err = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            std::slice::from_ref(&ollama),
            &off,
            &json!({}),
            &mut seeded(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlanError::NoEndpointSupported(EndpointKind::ChatCompletions, _)
        ));

        // Flag ON → native Ollama `/api/chat` group (G1, same tier as OpenAI).
        let on = FeatureFlags {
            ollama_native: true,
            ..flags(true)
        };
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[ollama],
            &on,
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].tier, GroupTier::Native);
        assert_eq!(plan.groups[0].upstream_protocol, UpstreamProtocol::Ollama);
        assert_eq!(plan.groups[0].upstream_endpoint, "api_chat");
    }

    #[test]
    fn ollama_native_does_not_serve_count_tokens() {
        // Ollama `/api/chat` must never satisfy CountTokens (no codec path).
        let ollama = new_channel(
            "o1",
            "ollama",
            "ollama",
            "http://localhost:11434",
            &["api_chat"],
            1,
            1,
        );
        let key = api_key(&[], &[]);
        let on = FeatureFlags {
            ollama_native: true,
            ..flags(true)
        };
        let err = authorize_and_plan(
            &key,
            "m",
            EndpointKind::CountTokens,
            &[ollama],
            &on,
            &json!({}),
            &mut seeded(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlanError::NoEndpointSupported(EndpointKind::CountTokens, _)
        ));
        assert_eq!(err.http_status(), 501);
    }

    // --- Messages / CountTokens / Embeddings ---

    #[test]
    fn messages_native_anthropic_then_openai_conversion() {
        let ant = channel(
            "a1",
            "claude",
            "https://api.anthropic.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let oai = channel(
            "o1",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            100,
            100,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::Messages,
            &[ant, oai],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 2);
        assert_eq!(plan.groups[0].tier, GroupTier::Native);
        assert_eq!(plan.groups[0].candidates[0].channel.id, "a1");
        assert_eq!(plan.groups[1].tier, GroupTier::Conversion);
        assert_eq!(plan.groups[1].candidates[0].channel.id, "o1");
    }

    #[test]
    fn count_tokens_only_anthropic() {
        let ant = new_channel(
            "a1",
            "anthropic",
            "anthropic",
            "https://api.anthropic.com",
            &["messages", "count_tokens"],
            1,
            1,
        );
        let oai = channel(
            "o1",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            100,
            100,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::CountTokens,
            &[ant, oai],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].tier, GroupTier::Native);
        assert_eq!(plan.groups[0].candidates[0].channel.id, "a1");
    }

    #[test]
    fn config_error_channel_is_dropped_and_reported() {
        // A gemini row with an empty base_url yields an identity with neither a
        // base URL nor endpoints — it must be dropped and reported, never routed.
        let bad = channel("bad", "gemini", "", &["m"], 1, 1, "{}");
        let good = channel(
            "good",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[bad, good],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        assert_eq!(plan.groups[0].candidates.len(), 1);
        assert_eq!(plan.groups[0].candidates[0].channel.id, "good");
        assert!(!plan.config_errors.is_empty());
    }

    #[test]
    fn upstream_model_string_and_array_sampling() {
        let mapping = json!({
            "alias": "mapped-single",
            "alias-arr": ["a", "b", "c"]
        });
        let mut rng = seeded();
        assert_eq!(
            resolve_upstream_model(&mapping, "alias", &mut rng),
            "mapped-single"
        );
        let v = resolve_upstream_model(&mapping, "alias-arr", &mut rng);
        assert!(["a", "b", "c"].contains(&v.as_str()));
        // Deterministic for the same seed: two freshly-seeded RNGs produce the
        // same first sample.
        let mut rng_a = StdRng::seed_from_u64(42);
        let mut rng_b = StdRng::seed_from_u64(42);
        assert_eq!(
            resolve_upstream_model(&mapping, "alias-arr", &mut rng_a),
            resolve_upstream_model(&mapping, "alias-arr", &mut rng_b)
        );
        // No mapping → requested model.
        assert_eq!(
            resolve_upstream_model(&mapping, "unknown", &mut rng),
            "unknown"
        );
    }

    #[test]
    fn debug_json_never_leaks_api_key() {
        let native = channel(
            "n1",
            "openai",
            "https://api.openai.com/v1",
            &["m"],
            1,
            1,
            "{}",
        );
        let key = api_key(&[], &[]);
        let plan = authorize_and_plan(
            &key,
            "m",
            EndpointKind::ChatCompletions,
            &[native],
            &flags(true),
            &json!({}),
            &mut seeded(),
        )
        .unwrap();
        let s = serde_json::to_string(&plan.debug_json()).unwrap();
        assert!(
            !s.contains("sk-test"),
            "api_key must never leak into plan debug output"
        );
        assert!(s.contains("chat_completions_g1_native"));
    }
}
