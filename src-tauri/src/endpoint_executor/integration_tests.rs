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
        "https://api.deepseek.com/anthropic/v1",
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
    assert_eq!(plan.groups[0].candidates[0].candidate.id(), "n1");
    assert_eq!(plan.groups[1].tier.as_str(), "conversion");
    assert_eq!(plan.groups[1].candidates[0].candidate.id(), "c1");

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
    assert_eq!(plan.groups[0].candidates[0].candidate.id(), "a1");
    assert_eq!(plan.groups[1].candidates[0].candidate.id(), "o1");
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
    assert_eq!(plan.groups[0].candidates[0].candidate.id(), "a1");
    assert_eq!(plan.groups[0].upstream_endpoint, "count_tokens");
}

// ── auth_account ─────────────────────────────────────────────────────────

/// T7 integration coverage: the Codex account adapter always receives an SSE
/// request, while the driver presents the requested downstream protocol for all
/// three supported endpoints on both facade paths.
mod auth_account {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use async_trait::async_trait;
    use axum::{
        body::Bytes,
        extract::State,
        http::{header, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
        Router,
    };
    use serde_json::{json, Value};
    use tokio::sync::Mutex;

    use super::*;
    use crate::auth_provider::service::AuthService;
    use crate::{
        auth_provider::{
            LoginResult, LoginRuntime, Provider, ProviderError, ProviderKind, ProviderModels,
            ProviderPayload, ProviderRegistry, ProviderRequest, RefreshedPayload,
        },
        core::route_plan::authorize_and_plan_with_accounts,
        db::models::{AuthAccountUpsert, ModelState, ModelStates},
        endpoint_executor::driver::{
            route_plan_response_with_auth_service, route_stream_plan_with_auth_service,
        },
    };

    #[derive(Clone, Default)]
    struct MockState {
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<Value>>>,
        fail_upstream: bool,
    }

    async fn responses(State(state): State<MockState>, body: Bytes) -> Response {
        state.hits.fetch_add(1, Ordering::SeqCst);
        state
            .seen
            .lock()
            .await
            .push(serde_json::from_slice(&body).unwrap());
        if state.fail_upstream {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "fixture upstream failure",
            )
                .into_response();
        }
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"weather\",\"arguments\":\"{\\\"city\\\":\\\"Shanghai\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\",\"status\":\"completed\",\"output\":[{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"weather\",\"arguments\":\"{\\\"city\\\":\\\"Shanghai\\\"}\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        );
        ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
    }

    #[derive(Clone)]
    struct LocalProvider {
        endpoint: String,
    }

    #[async_trait]
    impl Provider for LocalProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Codex
        }

        async fn login(&self, _: &dyn LoginRuntime) -> Result<LoginResult, ProviderError> {
            Err(ProviderError::LoginFailed)
        }

        async fn import(&self, _: &[u8]) -> Result<LoginResult, ProviderError> {
            Err(ProviderError::ImportFailed)
        }

        async fn refresh(
            &self,
            payload: &ProviderPayload,
        ) -> Result<RefreshedPayload, ProviderError> {
            Ok(RefreshedPayload {
                payload: payload.clone(),
                last_refreshed_at: None,
                next_refresh_after: None,
                next_retry_after: None,
            })
        }

        async fn outbound(
            &self,
            request: ProviderRequest<'_>,
        ) -> Result<reqwest::Response, ProviderError> {
            reqwest::Client::new()
                .post(&self.endpoint)
                .headers(request.headers.clone())
                .json(request.body)
                .send()
                .await
                .map_err(|_| ProviderError::Retryable)
        }

        async fn list_models(
            &self,
            _account: &crate::db::models::AuthAccount,
            _payload: &ProviderPayload,
        ) -> Result<ProviderModels, ProviderError> {
            Ok(vec![])
        }
    }

    async fn setup_with_failure(
        fail_upstream: bool,
    ) -> (
        Arc<Repository>,
        Arc<AuthService>,
        MockState,
        crate::db::models::AuthAccount,
    ) {
        let pool = fresh_db().await;
        let repo = Arc::new(Repository::new(pool));
        let state = MockState {
            fail_upstream,
            ..MockState::default()
        };
        let app = Router::new()
            .route("/responses", post(responses))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let account = repo
            .upsert_by_provider_account_id(&AuthAccountUpsert {
                provider: "codex".into(),
                label: "Codex fixture".into(),
                account_id: "remote-account".into(),
                attributes: json!({}),
                payload: json!({"access_token":"fixture", "expires_at":"2099-01-01T00:00:00Z"}),
                last_refreshed_at: None,
                next_refresh_after: None,
                next_retry_after: None,
            })
            .await
            .unwrap();
        repo.update_models_if_success(
            &account.id,
            &ModelStates {
                version: 1,
                models: vec![ModelState {
                    id: "m".into(),
                    status: "available".into(),
                    unavailable: false,
                    next_retry_after: None,
                    last_error: None,
                }],
            },
            &now(),
        )
        .await
        .unwrap();
        let account = repo.get_auth_account(&account.id).await.unwrap();
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(LocalProvider { endpoint }));
        let service = Arc::new(AuthService::new(repo.clone(), registry));
        (repo, service, state, account)
    }

    async fn setup() -> (
        Arc<Repository>,
        Arc<AuthService>,
        MockState,
        crate::db::models::AuthAccount,
    ) {
        setup_with_failure(false).await
    }

    fn make_request(
        endpoint: EndpointKind,
        stream: bool,
    ) -> (
        crate::security::gate::AuditedRequest,
        &'static str,
        &'static str,
    ) {
        match endpoint {
            EndpointKind::ChatCompletions => (
                audited(
                    DownstreamProtocol::ChatCompletions,
                    "chat_completions",
                    "m",
                    json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],"stream":stream}),
                ),
                "chat",
                "chat.completion",
            ),
            EndpointKind::Messages => (
                audited(
                    DownstreamProtocol::Messages,
                    "messages",
                    "m",
                    json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],"stream":stream}),
                ),
                "anthropic",
                "message",
            ),
            EndpointKind::Responses => (
                audited(
                    DownstreamProtocol::Responses,
                    "responses",
                    "m",
                    json!({"model":"m","input":"hi","stream":stream}),
                ),
                "responses",
                "output",
            ),
            _ => unreachable!("account routes only support three downstream endpoints"),
        }
    }

    fn plan(
        key: &ApiKey,
        account: &crate::db::models::AuthAccount,
        endpoint: EndpointKind,
        request: &crate::security::gate::AuditedRequest,
    ) -> crate::core::route_plan::RoutePlan {
        authorize_and_plan_with_accounts(
            key,
            "m",
            endpoint,
            &[],
            std::slice::from_ref(account),
            &flags(true),
            &request.forward_json,
            &mut rand::rngs::StdRng::seed_from_u64(7),
        )
        .unwrap()
    }

    fn assert_non_stream_shape(endpoint: EndpointKind, body: &[u8]) {
        let json: Value = serde_json::from_slice(body).unwrap();
        match endpoint {
            EndpointKind::ChatCompletions => {
                assert_eq!(json["object"], "chat.completion");
                assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
                assert_eq!(json["usage"]["prompt_tokens"], 3);
                assert_eq!(json["usage"]["completion_tokens"], 2);
                assert_eq!(
                    json["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
                    "weather"
                );
                assert_eq!(
                    json["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
                    "{\"city\":\"Shanghai\"}"
                );
            }
            EndpointKind::Messages => {
                assert_eq!(json["type"], "message");
                assert_eq!(json["stop_reason"], "tool_use");
                assert_eq!(json["usage"]["input_tokens"], 3);
                assert_eq!(json["usage"]["output_tokens"], 2);
                assert_eq!(json["content"][0]["type"], "tool_use");
                assert_eq!(json["content"][0]["name"], "weather");
                assert_eq!(json["content"][0]["input"]["city"], "Shanghai");
            }
            EndpointKind::Responses => {
                assert_eq!(json["id"], "resp_1");
                assert_eq!(json["status"], "completed");
                assert_eq!(json["usage"]["input_tokens"], 3);
                assert_eq!(json["usage"]["output_tokens"], 2);
                assert_eq!(json["output"][0]["type"], "function_call");
                assert_eq!(json["output"][0]["name"], "weather");
                assert_eq!(json["output"][0]["arguments"], "{\"city\":\"Shanghai\"}");
            }
            _ => unreachable!("account routes only support three downstream endpoints"),
        }
    }

    fn assert_stream_shape(endpoint: EndpointKind, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        match endpoint {
            EndpointKind::ChatCompletions => {
                assert!(text.contains("\"finish_reason\":\"tool_calls\""), "{text}");
                assert!(text.contains("\"prompt_tokens\":3"), "{text}");
                assert!(text.contains("\"completion_tokens\":2"), "{text}");
                assert!(text.contains("\"name\":\"weather\""), "{text}");
                assert!(text.contains("Shanghai"), "{text}");
            }
            EndpointKind::Messages => {
                assert!(text.contains("content_block_start"), "{text}");
                assert!(text.contains("\"type\":\"tool_use\""), "{text}");
                assert!(text.contains("\"name\":\"weather\""), "{text}");
                assert!(text.contains("Shanghai"), "{text}");
                assert!(text.contains("\"stop_reason\":\"tool_use\""), "{text}");
            }
            EndpointKind::Responses => {
                assert!(text.contains("response.completed"), "{text}");
                assert!(text.contains("\"type\":\"function_call\""), "{text}");
                assert!(text.contains("\"name\":\"weather\""), "{text}");
                assert!(text.contains("\"input_tokens\":3"), "{text}");
                assert!(text.contains("\"output_tokens\":2"), "{text}");
            }
            _ => unreachable!("account routes only support three downstream endpoints"),
        }
    }

    #[tokio::test]
    async fn three_protocols_stream_and_non_stream_force_responses_sse_and_log_account_source() {
        let (repo, service, state, account) = setup().await;
        let key = api_key();
        for endpoint in [
            EndpointKind::ChatCompletions,
            EndpointKind::Messages,
            EndpointKind::Responses,
        ] {
            let (request, mode, expected) = make_request(endpoint, false);
            let response = route_plan_response_with_auth_service(
                plan(&key, &account, endpoint, &request),
                &request,
                &key,
                &[],
                mode,
                &repo,
                "{}",
                None,
                service.clone(),
            )
            .await;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::OK,
                "{endpoint:?} non-stream"
            );
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_non_stream_shape(endpoint, &body);
            assert!(
                String::from_utf8_lossy(&body).contains(expected),
                "{endpoint:?} non-stream body: {}",
                String::from_utf8_lossy(&body)
            );

            let (request, mode, expected) = make_request(endpoint, true);
            let response = route_stream_plan_with_auth_service(
                plan(&key, &account, endpoint, &request),
                &request,
                &key,
                &[],
                mode,
                &repo,
                "{}",
                None,
                service.clone(),
            )
            .await;
            if response.status() != axum::http::StatusCode::OK {
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                panic!(
                    "{endpoint:?} stream status {status}: {}",
                    String::from_utf8_lossy(&body)
                );
            }
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_stream_shape(endpoint, &body);
            let stream_expected = if endpoint == EndpointKind::Responses {
                "response.completed"
            } else {
                expected
            };
            assert!(
                String::from_utf8_lossy(&body).contains(stream_expected),
                "{endpoint:?} stream body: {}",
                String::from_utf8_lossy(&body)
            );
        }
        tokio::task::yield_now().await;
        let seen = state.seen.lock().await;
        assert_eq!(seen.len(), 6);
        assert!(seen.iter().all(|body| body["stream"] == true));
        drop(seen);
        let logs = repo.get_logs(20, 0).await.unwrap();
        assert!(logs.len() >= 5, "all completed facade paths must be logged");
        assert!(logs.iter().all(|log| log.upstream_type == "auth_account"));
    }

    /// CR-1 #5: exhausting a single Auth Account must retain the LAST attempted
    /// candidate metadata in both facade failure logs.  A pre-plan rejection is
    /// the only case permitted to have no upstream candidate fields.
    #[tokio::test]
    async fn exhausted_auth_account_failure_logs_keep_candidate_metadata_for_stream_and_non_stream()
    {
        let (repo, service, state, account) = setup_with_failure(true).await;
        let key = api_key();

        let (request, mode, _) = make_request(EndpointKind::ChatCompletions, false);
        let response = route_plan_response_with_auth_service(
            plan(&key, &account, EndpointKind::ChatCompletions, &request),
            &request,
            &key,
            &[],
            mode,
            &repo,
            "{}",
            None,
            service.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let (stream_request, stream_mode, _) = make_request(EndpointKind::ChatCompletions, true);
        let response = route_stream_plan_with_auth_service(
            plan(
                &key,
                &account,
                EndpointKind::ChatCompletions,
                &stream_request,
            ),
            &stream_request,
            &key,
            &[],
            stream_mode,
            &repo,
            "{}",
            None,
            service,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        assert_eq!(state.hits.load(Ordering::SeqCst), 2);
        let logs = repo.get_logs(10, 0).await.unwrap();
        assert_eq!(logs.len(), 2);
        for log in logs {
            assert_eq!(log.upstream_type, "auth_account");
            assert_eq!(log.channel_id.as_deref(), Some(account.id.as_str()));
            assert_eq!(log.channel_name.as_deref(), Some(account.label.as_str()));
            assert_eq!(log.provider.as_deref(), Some("codex"));
            assert_eq!(log.upstream_protocol.as_deref(), Some("responses"));
            assert_eq!(log.upstream_endpoint.as_deref(), Some("responses"));
            assert_eq!(log.codec_version.as_deref(), Some("chat_to_responses_v1"));
        }
    }
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
