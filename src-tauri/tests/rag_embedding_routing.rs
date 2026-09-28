//! 内部建库必须与网关使用相同的模型/能力路由和端点执行器。
use axum::{http::HeaderMap, http::StatusCode, http::Uri, Json, Router};
use rand::SeedableRng;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use waliapi_lib::{
    core::{
        feature_flags::FeatureFlags,
        route_plan::{authorize_and_plan, EndpointKind, PlanError},
    },
    db::{models::Channel, repository::Repository},
    services::knowledge::embedder,
};

#[derive(Clone)]
struct Request {
    path: String,
    authorization: String,
    custom_header: String,
    body: Value,
}

struct Upstream {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn upstream(responses: Vec<(StatusCode, Value)>, delay: Duration) -> Upstream {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let responses = Arc::new(responses);
    let app = Router::new().fallback(
        move |uri: Uri, headers: HeaderMap, Json(body): Json<Value>| {
            let observed = observed.clone();
            let responses = responses.clone();
            async move {
                let index = {
                    let mut requests = observed.lock().unwrap();
                    let index = requests.len();
                    requests.push(Request {
                        path: uri.path().to_string(),
                        authorization: headers
                            .get("authorization")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .into(),
                        custom_header: headers
                            .get("x-embedding-test")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .into(),
                        body,
                    });
                    index
                };
                tokio::time::sleep(delay).await;
                let (status, body) = responses[index.min(responses.len() - 1)].clone();
                (status, Json(body))
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/native/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream {
        url,
        requests,
        server,
    }
}

fn success() -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({"data":[{"index":0,"embedding":[1.0,0.0]}]}),
    )
}

async fn repo() -> Repository {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    Repository::new(pool)
}

async fn channel(repo: &Repository, server: &Upstream, extra: Value) -> Channel {
    let mut input = json!({
        "name":"embedding-test", "type":"openai", "base_url":server.url,
        "api_key":"local-test", "models":["embed-model"],
        "protocol":"openai", "provider":"custom", "native_base_url":server.url,
        "native_endpoints":["embeddings"], "config":{"proxy":{"mode":"direct"}}
    });
    input
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    repo.create_channel(&serde_json::from_value(input).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn missing_capability_rejects_both_internal_and_gateway_before_upstream_access() {
    let server = upstream(vec![success()], Duration::ZERO).await;
    let repo = repo().await;
    let channel = channel(
        &repo,
        &server,
        json!({"native_endpoints":["chat_completions"]}),
    )
    .await;
    let internal = embedder::embed(&["document".into()], "embed-model", &repo)
        .await
        .unwrap_err();
    assert!(internal.contains("501") && internal.contains("embeddings"));
    let key = serde_json::from_value(json!({
        "id":"external", "name":"external", "key":"external-test", "status":1,
        "allowed_models":"[]", "allowed_channels":"[]", "denied_models":"[]",
        "denied_channels":"[]", "quota_limit":0, "quota_used":0,
        "created_at":"now", "updated_at":"now"
    }))
    .unwrap();
    let external = authorize_and_plan(
        &key,
        "embed-model",
        EndpointKind::Embeddings,
        &[channel],
        &FeatureFlags::default(),
        &json!({"model":"embed-model", "input":["query"]}),
        &mut rand::rngs::StdRng::seed_from_u64(1),
    )
    .unwrap_err();
    assert!(matches!(
        external,
        PlanError::NoEndpointSupported(EndpointKind::Embeddings, _)
    ));
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn legacy_gemini_executor_cannot_claim_openai_embeddings() {
    let server = upstream(vec![success()], Duration::ZERO).await;
    let repo = repo().await;
    let channel = channel(&repo, &server, json!({})).await;
    sqlx::query("UPDATE channels SET legacy_executor_override = 'gemini_native' WHERE id = ?")
        .bind(&channel.id)
        .execute(repo.pool())
        .await
        .unwrap();
    let error = embedder::embed(&["document".into()], "embed-model", &repo)
        .await
        .unwrap_err();
    assert!(error.contains("501"));
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_model_and_disabled_mapping_do_not_fall_back_to_all_channels() {
    let server = upstream(vec![success()], Duration::ZERO).await;
    let repo = repo().await;
    channel(
        &repo,
        &server,
        json!({
            "model_mapping":{"disabled-alias":"embed-model"},
            "model_mapping_disabled":[["disabled-alias","embed-model"]]
        }),
    )
    .await;
    for model in ["unknown-model", "disabled-alias"] {
        let error = embedder::embed(&["document".into()], model, &repo)
            .await
            .unwrap_err();
        assert!(error.contains("503"));
    }
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn shared_executor_preserves_native_url_mapping_headers_and_multiple_keys() {
    let server = upstream(
        vec![
            (
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":{"message":"retry local test"}}),
            ),
            success(),
        ],
        Duration::ZERO,
    )
    .await;
    let repo = repo().await;
    let channel = channel(
        &repo,
        &server,
        json!({
            "api_key":"",
            "model_mapping":{"public-alias":["disabled-upstream","real-embedding"]},
            "model_mapping_disabled":[["public-alias","disabled-upstream"]],
            "extra_keys":[{"api_key":"local-key-one"},{"api_key":"local-key-two"}],
            "request_headers":[{"name":"x-embedding-test","value":"forwarded","status":1}]
        }),
    )
    .await;
    // 显式 identity 的 URL 必须优先于遗留 base_url。
    sqlx::query("UPDATE channels SET base_url = 'http://127.0.0.1:1/obsolete' WHERE id = ?")
        .bind(&channel.id)
        .execute(repo.pool())
        .await
        .unwrap();
    // 旧字段写入会触发 migration 015 的身份失效机制，随后恢复显式 identity，
    // 与正式双写顺序一致；否则本用例测到的只是旧客户端配置失效。
    sqlx::query(
        "UPDATE channels SET protocol = ?, provider = ?, native_base_url = ?,
         native_endpoints = ?, identity_revision = ? WHERE id = ?",
    )
    .bind(&channel.protocol)
    .bind(&channel.provider)
    .bind(&channel.native_base_url)
    .bind(&channel.native_endpoints)
    .bind(channel.identity_revision)
    .bind(&channel.id)
    .execute(repo.pool())
    .await
    .unwrap();
    let result = embedder::embed(&["⼀个文档".into()], "public-alias", &repo)
        .await
        .unwrap();
    assert_eq!(result, vec![vec![1.0, 0.0]]);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "限流时应切换同渠道的另一个 Key");
    assert_ne!(requests[0].authorization, requests[1].authorization);
    for request in requests.iter() {
        assert_eq!(request.path, "/native/v1/embeddings");
        assert_eq!(request.custom_header, "forwarded");
        assert_eq!(request.body["model"], "real-embedding");
        assert_eq!(request.body["input"], json!(["一个文档"]));
        assert_eq!(request.body["encoding_format"], "float");
    }
}

#[tokio::test]
async fn channel_timeout_uses_configured_limit_and_fails_over() {
    let slow = upstream(vec![success()], Duration::from_secs(4)).await;
    let fallback = upstream(vec![success()], Duration::ZERO).await;
    let repo = repo().await;
    channel(&repo, &slow, json!({"priority":10,"timeout_secs":1})).await;
    channel(&repo, &fallback, json!({"priority":0})).await;
    let started = Instant::now();
    let result = embedder::embed(&["document".into()], "embed-model", &repo)
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(result, vec![vec![1.0, 0.0]]);
    assert_eq!(slow.requests.lock().unwrap().len(), 1);
    assert_eq!(fallback.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn internal_routing_respects_embedding_mode_cooldown() {
    let cooling = upstream(vec![success()], Duration::ZERO).await;
    let fallback = upstream(vec![success()], Duration::ZERO).await;
    let repo = repo().await;
    let channel = channel(&repo, &cooling, json!({"priority":10})).await;
    let now = chrono::Utc::now().to_rfc3339();
    let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
    for _ in 0..2 {
        repo.record_channel_mode_failure(
            &channel.id,
            "embeddings",
            false,
            &now,
            &until,
            "invalid JSON",
        )
        .await
        .unwrap();
    }
    self::channel(&repo, &fallback, json!({"priority":0})).await;
    embedder::embed(&["document".into()], "embed-model", &repo)
        .await
        .unwrap();
    assert!(cooling.requests.lock().unwrap().is_empty());
    assert_eq!(fallback.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn upstream_secrets_are_not_exposed_in_internal_embedding_errors() {
    let server = upstream(
        vec![(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"upstream leaked sk-private-value document text"}}),
        )],
        Duration::ZERO,
    )
    .await;
    let repo = repo().await;
    channel(&repo, &server, json!({})).await;
    let error = embedder::embed(&["document".into()], "embed-model", &repo)
        .await
        .unwrap_err();
    assert!(error.contains("400") && error.contains("caller_terminal"));
    assert!(!error.contains("sk-private-value") && !error.contains("document text"));
}
