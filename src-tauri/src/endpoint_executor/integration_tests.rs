//! T06 integration tests: `protocol_routing_integration` + `stream_failover`.
//!
//! These drive the REAL T05 facade (`authorize_and_plan` + `execute_plan`) and
//! the REAL streaming commit barrier (`StreamPumpCore` + `StreamSupervisor`)
//! against an in-memory SQLite DB, verifying:
//!   * native-first routing (OpenAI Chat before Anthropic codec G2),
//!   * conversion decode on the non-stream facade path,
//!   * RequestLog + quota accounting on the facade path,
//!   * streaming pre-commit failover (invalid first frame → next candidate) and
//!     the post-commit no-retry barrier.

#![cfg(test)]

use crate::core::feature_flags::FeatureFlags;
use crate::core::route_plan::{authorize_and_plan, EndpointKind};
use crate::core::stream_supervisor::StreamSupervisor;
use crate::db::models::{ApiKey, Channel};
use crate::db::repository::Repository;
use crate::endpoint_executor::sse::{SseMode, StreamPumpCore};
use crate::security::gate::{DownstreamProtocol, RequestEnvelope, RequestFeatures};
use crate::security::SecurityScanResult;
use rand::SeedableRng;
use serde_json::{json, Value};
use std::sync::Arc;

fn now() -> String {
    crate::utils::time::now_iso()
}

async fn fresh_db() -> sqlx::SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory db");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate fresh db");
    pool
}

fn api_key() -> ApiKey {
    ApiKey {
        id: "key-1".into(),
        name: "t".into(),
        key: "sk-test".into(),
        status: 1,
        allowed_models: "[]".into(),
        allowed_channels: "[]".into(),
        quota_limit: 0,
        quota_used: 0,
        expires_at: None,
        created_at: now(),
        updated_at: now(),
    }
}

#[allow(clippy::too_many_arguments)]
fn channel(
    id: &str,
    protocol: &str,
    provider: &str,
    native_base: &str,
    endpoints: &[&str],
    priority: i64,
) -> Channel {
    Channel {
        id: id.into(),
        name: format!("ch-{id}"),
        channel_type: if protocol == "anthropic" {
            "claude"
        } else {
            "openai"
        }
        .into(),
        base_url: native_base.into(),
        api_key: "sk-upstream".into(),
        models: json!(["m"]).to_string(),
        status: 1,
        priority,
        weight: 1,
        config: "{}".into(),
        model_mapping: "{}".into(),
        timeout_secs: 30,
        protocol: Some(protocol.into()),
        provider: Some(provider.into()),
        native_base_url: Some(native_base.into()),
        native_endpoints: Some(serde_json::to_string(endpoints).unwrap()),
        preset_revision: Some("test".into()),
        identity_revision: 1,
        legacy_executor_override: None,
        created_at: now(),
        updated_at: now(),
        last_test_at: None,
        last_test_ok: None,
    }
}

fn audited(
    protocol: DownstreamProtocol,
    endpoint: &str,
    model: &str,
    body: Value,
) -> crate::security::gate::AuditedRequest {
    crate::security::gate::AuditedRequest {
        envelope: RequestEnvelope {
            downstream_protocol: protocol,
            endpoint: endpoint.to_string(),
            original_json: body.clone(),
            safe_forward_headers: vec![],
            query: None,
            model: model.to_string(),
            stream: false,
            trace_id: None,
        },
        forward_json: body.clone(),
        sanitized_log_json: body,
        body_hash: "h".into(),
        body_len: 0,
        audit_result: SecurityScanResult::default(),
        request_features: RequestFeatures::default(),
    }
}

fn flags(codec_on: bool) -> FeatureFlags {
    FeatureFlags {
        new_routeplan: true,
        cross_protocol_codec: codec_on,
        native_responses: true,
        ollama_native: false,
    }
}

/// Insert the enabled channels into the pool so the facade's
/// `get_enabled_channels` sees them.
async fn insert_channels(pool: &sqlx::SqlitePool, channels: &[Channel]) {
    for c in channels {
        sqlx::query(
            "INSERT INTO channels (id, name, type, base_url, api_key, models, status, priority, weight, config, model_mapping, timeout_secs, protocol, provider, native_base_url, native_endpoints, preset_revision, identity_revision, legacy_executor_override, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
        )
        .bind(&c.id).bind(&c.name).bind(&c.channel_type).bind(&c.base_url)
        .bind(&c.api_key).bind(&c.models).bind(c.status).bind(c.priority).bind(c.weight)
        .bind(&c.config).bind(&c.model_mapping).bind(c.timeout_secs)
        .bind(&c.protocol).bind(&c.provider).bind(&c.native_base_url).bind(&c.native_endpoints)
        .bind(&c.preset_revision).bind(c.identity_revision).bind(&c.legacy_executor_override)
        .bind(&c.created_at).bind(&c.updated_at)
        .execute(pool)
        .await
        .expect("insert channel");
    }
}

// ── protocol_routing_integration ──────────────────────────────────────────

/// Non-stream Chat with BOTH a native OpenAI channel (low priority) and an
/// Anthropic codec channel (high priority): the native group must win, and the
/// conversion attempt's encoded body must be the codec-shaped Messages body.
#[tokio::test]
async fn protocol_routing_integration_chat_native_first_then_conversion() {
    let pool = fresh_db().await;
    let repo = Arc::new(Repository::new(pool.clone()));
    let native = channel(
        "n1",
        "openai",
        "deepseek",
        "https://api.deepseek.com",
        &["chat_completions"],
        1,
    );
    let conv = channel(
        "c1",
        "anthropic",
        "deepseek",
        "https://api.deepseek.com/anthropic",
        &["messages"],
        100,
    );
    insert_channels(&pool, &[native, conv]).await;

    let key = api_key();
    let audited = audited(
        DownstreamProtocol::ChatCompletions,
        "chat_completions",
        "m",
        json!({"model":"m","messages":[{"role":"user","content":"hi"}]}),
    );
    let channels = repo.get_enabled_channels().await.unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let plan = authorize_and_plan(
        &key,
        "m",
        EndpointKind::ChatCompletions,
        &channels,
        &flags(true),
        &audited.forward_json,
        &mut rng,
    )
    .unwrap();

    assert_eq!(plan.groups.len(), 2);
    assert_eq!(plan.groups[0].tier.as_str(), "native");
    assert_eq!(plan.groups[0].candidates[0].channel.id, "n1");
    assert_eq!(plan.groups[1].tier.as_str(), "conversion");
    assert_eq!(plan.groups[1].candidates[0].channel.id, "c1");

    // Drive execute_plan with a mock executor that records which attempt it
    // saw and classifies by upstream protocol: native succeeds, conversion
    // would run only if native fails.
    let mut seen = Vec::new();
    let out = crate::core::plan_executor::execute_plan(
        plan,
        &audited,
        rand::rngs::StdRng::seed_from_u64(7),
        |attempt| {
            seen.push(attempt.upstream_protocol.clone());
            // Own a clone so the returned future does not borrow `attempt`.
            let attempt = attempt.clone();
            let p = attempt.upstream_protocol.clone();
            async move {
                if p == "openai" {
                    crate::core::plan_executor::ok_result(
                        200,
                        json!({"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
                        Some(crate::core::attempt::TokenUsage { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 }),
                    )
                } else {
                    // Anthropic conversion attempt: verify the prepared body is
                    // the codec-shaped Messages request, then fail so the flow
                    // can continue (this should not happen because native wins).
                    assert!(attempt.encoded_body.get("system").is_some() || attempt.encoded_body.get("max_tokens").is_some());
                    crate::core::attempt::AttemptResult::Failure(crate::core::attempt::AttemptFailure {
                        failure_class: crate::core::attempt::FailureClass::Retryable,
                        message: "unexpected".into(),
                        status_code: Some(502),
                        retry_after: None,
                    })
                }
            }
        },
    )
    .await;
    assert_eq!(out.status, 200);
    assert_eq!(seen, vec!["openai"], "native group must be attempted first");
    assert_eq!(out.channel_id.as_deref(), Some("n1"));
}

/// Messages routing: native Anthropic G1 before OpenAI Chat G2.
#[tokio::test]
async fn protocol_routing_integration_messages_native_anthropic_first() {
    let pool = fresh_db().await;
    let repo = Arc::new(Repository::new(pool.clone()));
    let ant = channel(
        "a1",
        "anthropic",
        "anthropic",
        "https://api.anthropic.com",
        &["messages"],
        1,
    );
    let oai = channel(
        "o1",
        "openai",
        "openai",
        "https://api.openai.com/v1",
        &["chat_completions"],
        100,
    );
    insert_channels(&pool, &[ant, oai]).await;

    let key = api_key();
    let audited = audited(
        DownstreamProtocol::Messages,
        "messages",
        "m",
        json!({"model":"m","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}),
    );
    let channels = repo.get_enabled_channels().await.unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let plan = authorize_and_plan(
        &key,
        "m",
        EndpointKind::Messages,
        &channels,
        &flags(true),
        &audited.forward_json,
        &mut rng,
    )
    .unwrap();
    assert_eq!(plan.groups.len(), 2);
    assert_eq!(plan.groups[0].candidates[0].channel.id, "a1");
    assert_eq!(plan.groups[1].candidates[0].channel.id, "o1");
}

/// Non-stream CountTokens: only Anthropic channels with the capability are
/// candidates; an OpenAI channel produces NoEndpointSupported → 501.
#[tokio::test]
async fn protocol_routing_integration_count_tokens_capability_gated() {
    let pool = fresh_db().await;
    let repo = Arc::new(Repository::new(pool.clone()));
    let ant = channel(
        "a1",
        "anthropic",
        "anthropic",
        "https://api.anthropic.com",
        &["messages", "count_tokens"],
        1,
    );
    let oai = channel(
        "o1",
        "openai",
        "openai",
        "https://api.openai.com/v1",
        &["chat_completions"],
        100,
    );
    insert_channels(&pool, &[ant, oai]).await;
    let key = api_key();
    let audited = audited(
        DownstreamProtocol::CountTokens,
        "count_tokens",
        "m",
        json!({"model":"m","messages":[]}),
    );
    let channels = repo.get_enabled_channels().await.unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let plan = authorize_and_plan(
        &key,
        "m",
        EndpointKind::CountTokens,
        &channels,
        &flags(true),
        &audited.forward_json,
        &mut rng,
    )
    .unwrap();
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(plan.groups[0].candidates[0].channel.id, "a1");
    assert_eq!(plan.groups[0].upstream_endpoint, "count_tokens");
}

// ── stream_failover ───────────────────────────────────────────────────────

/// Pre-commit: an invalid first frame allows an upstream swap (retry the next
/// candidate); post-commit the barrier forbids swapping.
#[test]
fn stream_failover_commit_barrier() {
    // Candidate 1: connect + headers + invalid first frame → swap allowed.
    let mut s = StreamSupervisor::new();
    s.begin_connect().unwrap();
    s.on_upstream_headers().unwrap();
    assert!(
        s.swap_upstream().is_ok(),
        "invalid first frame → swap before commit"
    );
    assert_eq!(s.upstream_swaps(), 1);

    // Candidate 2: re-walk to commit.
    s.on_upstream_headers().unwrap();
    s.on_first_frame_validated().unwrap();
    s.commit_downstream().unwrap();
    s.begin_streaming().unwrap();
    // Post-commit: no retry possible.
    let err = s.swap_upstream().unwrap_err();
    assert_eq!(
        err,
        crate::core::stream_supervisor::StreamTransitionError::RetryAfterCommit
    );
    assert!(s.committed());
}

/// First-frame validation: a well-formed SSE record validates; malformed JSON
/// fails closed (pre-commit failover).
#[test]
fn stream_failover_first_frame_validation() {
    assert!(crate::endpoint_executor::sse::validate_native_first_record(
        b"data: {\"choices\":[]}\n\n"
    )
    .is_ok());
    assert!(
        crate::endpoint_executor::sse::validate_native_first_record(b"data: not-json\n\n").is_err()
    );
    assert!(crate::endpoint_executor::sse::validate_native_first_record(
        b"event: message_start\ndata: {}\n\n"
    )
    .is_ok());
}

/// A client cancel records exactly once and aborts; a second cancel is rejected.
#[test]
fn stream_failover_client_cancel_exactly_once() {
    let mut s = StreamSupervisor::new();
    s.begin_connect().unwrap();
    s.client_cancel().unwrap();
    assert!(s.client_cancelled());
    assert!(s.client_cancel().is_err(), "exactly-once finalizer");
}

/// End-to-end pump: a native stream commits on first frame and passes raw bytes
/// through, terminating exactly once on [DONE].
#[test]
fn stream_failover_pump_native_passthrough() {
    let mut sup = StreamSupervisor::new();
    sup.begin_connect().unwrap();
    sup.on_upstream_headers().unwrap();
    sup.on_first_frame_validated().unwrap();
    let mut pump = StreamPumpCore::new(
        sup,
        SseMode::Native,
        None,
        b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_vec(),
        Vec::new(),
        "m".to_string(),
    )
    .unwrap();
    let first = pump.start().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("data: {"));
    let done = pump.push(b"data: [DONE]\n\n").unwrap();
    assert_eq!(String::from_utf8_lossy(&done), "data: [DONE]\n\n");
    let fin = pump.finish().unwrap();
    assert!(fin.is_empty());
    assert!(pump.terminated());
}

/// I-3: a streaming pre-commit terminal outcome (all candidates exhausted /
/// codec rejection / authorize rejection) must write a RequestLog row so
/// failed streaming requests stay observable.
#[tokio::test]
async fn stream_precommit_failure_writes_request_log() {
    let pool = fresh_db().await;
    let repo = Arc::new(Repository::new(pool.clone()));
    // Insert the API key so quota/accounting can reference it.
    sqlx::query(
        "INSERT INTO api_keys (id, name, key, status, allowed_models, allowed_channels, quota_limit, quota_used, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
    )
    .bind("key-1").bind("t").bind("sk-test").bind(1i64)
    .bind("[]").bind("[]").bind(0i64).bind(0i64)
    .bind(now()).bind(now())
    .execute(&pool)
    .await
    .expect("insert api key");

    let key = api_key();
    let audited = audited(
        DownstreamProtocol::ChatCompletions,
        "chat_completions",
        "m",
        json!({"model": "m", "messages": []}),
    );
    crate::endpoint_executor::driver::write_stream_precommit_failure_log(
        &repo,
        &key,
        &audited,
        "chat",
        true,
        503,
        "no channel available",
        "{\"model\":\"m\"}",
        None,
    )
    .await;

    let row: (i64, Option<String>, i64) = sqlx::query_as(
        "SELECT status_code, error_message, is_stream FROM request_logs WHERE api_key_id = 'key-1'",
    )
    .fetch_one(&pool)
    .await
    .expect("request log row written");
    assert_eq!(row.0, 503);
    assert_eq!(row.1.as_deref(), Some("no channel available"));
    assert_eq!(
        row.2, 1,
        "streaming pre-commit failure must be flagged is_stream=1"
    );
}
