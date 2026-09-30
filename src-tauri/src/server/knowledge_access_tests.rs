use super::{
    build_router,
    tests::{json_request, request, test_shared, test_state},
};
use crate::{
    db::{models::ApiKey, repository::Repository},
    server::knowledge_access::{get_grants, set_grants},
    services::knowledge::{
        models::KbKnowledgeBase,
        repository::{ChunkInsert, KbRepository},
    },
    AppState,
};
use axum::{body::to_bytes, http::StatusCode, response::Response, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

async fn setup() -> (
    Arc<AppState>,
    ApiKey,
    KbKnowledgeBase,
    KbKnowledgeBase,
    Router,
) {
    let state = test_state().await;
    let repo = Repository::new(state.db.pool.clone());
    let key = repo
        .create_api_key(
            &serde_json::from_value(json!({"name":"query-test", "quota_limit":10000})).unwrap(),
        )
        .await
        .unwrap();
    let kb_repo = KbRepository::new(state.db.pool.clone());
    let first = kb_repo
        .create_kb(
            &serde_json::from_value(json!({"name":"granted", "embedding_model":"embed-test"}))
                .unwrap(),
        )
        .await
        .unwrap();
    let second = kb_repo
        .create_kb(
            &serde_json::from_value(json!({"name":"private", "embedding_model":"embed-test"}))
                .unwrap(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE kb_knowledge_bases SET mcp_enabled = 1")
        .execute(&state.db.pool)
        .await
        .unwrap();
    set_grants(&state.db.pool, &key.id, std::slice::from_ref(&first.id))
        .await
        .unwrap();
    let app = build_router(state.clone(), test_shared(&state, None, None));
    (state, key, first, second, app)
}

#[tokio::test]
async fn candidate_limits_are_validated_before_model_call() {
    let (_, key, first, _, app) = setup().await;
    for count in [0, 4, 101] {
        let response = app.clone().oneshot(json_request("POST", "/api/kb/ask", Some(&key.key),
            &json!({"kb_id":first.id,"question":"备份要求","top_k":5,"candidate_k":count,"search_mode":"keyword"}).to_string())).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = app.clone().oneshot(request("GET", &format!(
            "/api/kb/search?q=alpha&kb_id={}&search_mode=keyword&top_k=5&candidate_k={count}", first.id), Some(&key.key))).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(response).await["error"]["code"], "invalid_query");
    }
}

async fn body(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn rpc(key: &str, name: &str, arguments: Value) -> axum::http::Request<axum::body::Body> {
    json_request("POST", "/mcp", Some(key), &json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":name,"arguments":arguments}}).to_string())
}

async fn document(state: &AppState, kb_id: &str, content: &str) -> String {
    let repo = KbRepository::new(state.db.pool.clone());
    let doc = repo
        .create_document(
            kb_id,
            "test.txt",
            None,
            "txt",
            content.len() as i64,
            content,
        )
        .await
        .unwrap();
    repo.create_chunk(&ChunkInsert {
        id: uuid::Uuid::new_v4().to_string(),
        doc_id: doc.id.clone(),
        kb_id: kb_id.to_string(),
        chunk_index: 0,
        content: content.into(),
        token_count: 8,
        embedding: bincode::serialize(&vec![1.0_f32, 0.0, 0.0]).unwrap(),
        embedding_dim: 3,
        metadata: "{}".into(),
        content_hash: None,
        created_at: crate::utils::time::now_iso(),
    })
    .await
    .unwrap();
    repo.update_document_status(&doc.id, "ready", None)
        .await
        .unwrap();
    doc.id
}

#[tokio::test]
async fn newly_created_key_can_list_all_existing_kbs_over_rest_and_mcp() {
    let (state, _, first, second, app) = setup().await;
    let key = Repository::new(state.db.pool.clone())
        .create_api_key(&serde_json::from_value(json!({"name":"default-grants"})).unwrap())
        .await
        .unwrap();
    let result = body(
        app.clone()
            .oneshot(request("GET", "/api/kb", Some(&key.key)))
            .await
            .unwrap(),
    )
    .await;
    let ids = result["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kb| kb["id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids, [first.id.as_str(), second.id.as_str()].into());
    let result = body(
        app.oneshot(rpc(&key.key, "list_knowledge_bases", json!({})))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["isError"], false);
    assert!(result.to_string().contains(&first.id));
    assert!(result.to_string().contains(&second.id));
}

#[tokio::test]
async fn newly_created_kb_is_visible_to_existing_keys_and_can_be_revoked() {
    let (state, key, first, private, app) = setup().await;
    let kb = KbRepository::new(state.db.pool.clone())
        .create_kb(&serde_json::from_value(json!({"name":"new-shared"})).unwrap())
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/kb/{}/stats", kb.id),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(
        app.clone()
            .oneshot(rpc(
                &key.key,
                "get_knowledge_base_stats",
                json!({"kb_id":kb.id}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["isError"], false);
    // 新库的默认授权不恢复此前被显式撤销的其他库。
    let response = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/kb/{}/stats", private.id),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    set_grants(&state.db.pool, &key.id, &[first.id])
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/kb/{}/stats", kb.id),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let result = body(
        app.oneshot(rpc(
            &key.key,
            "get_knowledge_base_stats",
            json!({"kb_id":kb.id}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);
}

#[tokio::test]
async fn grants_filter_lists_and_block_management_and_cross_kb_reads() {
    let (state, key, first, second, app) = setup().await;
    let foreign_doc = document(&state, &second.id, "private secret").await;
    let result = body(
        app.clone()
            .oneshot(request("GET", "/api/kb", Some(&key.key)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["data"].as_array().unwrap().len(), 1);
    assert_eq!(result["data"][0]["id"], first.id);
    for (method, path) in [
        ("GET", format!("/api/kb/{}", second.id)),
        ("DELETE", format!("/api/kb/{}", first.id)),
        ("POST", format!("/api/kb/{}/documents", first.id)),
        ("POST", format!("/api/kb/{}/index", first.id)),
        ("GET", format!("/api/kb/{}/conversations", first.id)),
        ("GET", "/api/wiki/projects".into()),
    ] {
        let response = app
            .clone()
            .oneshot(request(method, &path, Some(&key.key)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    let response = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/kb/{}/documents/{foreign_doc}", first.id),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let result = body(
        app.clone()
            .oneshot(rpc(
                &key.key,
                "read_document",
                json!({"kb_id":first.id,"doc_id":foreign_doc}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);
    assert!(!result.to_string().contains("private secret"));
    let response = app
        .oneshot(request(
            "GET",
            &format!("/api/kb/{}/stats", first.id),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_requires_explicit_granted_kb_and_bounds() {
    let (_, key, first, second, app) = setup().await;
    for (input, status) in [
        (json!({"question":"alpha"}), StatusCode::BAD_REQUEST),
        (
            json!({"question":"alpha","kb_id":second.id}),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"question":"alpha","kb_id":first.id,"top_k":1000}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"question":"alpha","kb_id":first.id,"deep_research":true}),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    let response = app
        .oneshot(request("GET", "/api/kb/search?q=alpha", Some(&key.key)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn revocation_expiry_and_disabled_keys_take_effect_without_restart() {
    let (state, key, first, _, app) = setup().await;
    for (column, value, expected) in [
        ("status", "0", StatusCode::UNAUTHORIZED),
        (
            "expires_at",
            "'2000-01-01T00:00:00Z'",
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        sqlx::query(&format!(
            "UPDATE api_keys SET {column} = {value} WHERE id = ?"
        ))
        .bind(&key.id)
        .execute(&state.db.pool)
        .await
        .unwrap();
        let response = app
            .clone()
            .oneshot(request("GET", "/api/kb", Some(&key.key)))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        sqlx::query("UPDATE api_keys SET status = 1, expires_at = NULL WHERE id = ?")
            .bind(&key.id)
            .execute(&state.db.pool)
            .await
            .unwrap();
    }
    // 非法保存整体回滚，不丢失原授权；清空后立即失效。
    assert!(set_grants(
        &state.db.pool,
        &key.id,
        &[first.id.clone(), "missing-kb".into()]
    )
    .await
    .is_err());
    assert_eq!(
        get_grants(&state.db.pool, &key.id).await.unwrap(),
        vec![first.id]
    );
    set_grants(&state.db.pool, &key.id, &[]).await.unwrap();
    assert_eq!(
        app.oneshot(request("GET", "/api/kb", Some(&key.key)))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn mcp_limits_tools_exposure_and_legacy_sessions() {
    let (state, key, first, _, app) = setup().await;
    let result = body(
        app.clone()
            .oneshot(json_request(
                "POST",
                "/mcp",
                Some(&key.key),
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["tools"].as_array().unwrap().len(), 5);
    assert!(!result.to_string().contains("delete_knowledge_base"));
    let result = body(
        app.clone()
            .oneshot(rpc(
                &key.key,
                "delete_knowledge_base",
                json!({"kb_id":first.id}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["error"]["code"], -32601);
    for path in ["/mcp?session_id=foreign", "/mcp/sse"] {
        assert_eq!(
            app.clone()
                .oneshot(json_request("POST", path, Some(&key.key), "{}"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.clone()
            .oneshot(request("GET", "/mcp", Some(&key.key)))
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    sqlx::query("UPDATE kb_knowledge_bases SET mcp_enabled = 0 WHERE id = ?")
        .bind(&first.id)
        .execute(&state.db.pool)
        .await
        .unwrap();
    let result = body(
        app.clone()
            .oneshot(rpc(
                &key.key,
                "get_knowledge_base_stats",
                json!({"kb_id":first.id}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);
    let result = body(
        app.oneshot(rpc(&key.key, "list_knowledge_bases", json!({})))
            .await
            .unwrap(),
    )
    .await;
    assert!(!result.to_string().contains(&first.id));
}

async fn mock_model(state: &AppState) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    mock_model_with_answer(state, "BCD").await
}

async fn mock_model_with_answer(
    state: &AppState,
    answer_text: &'static str,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    mock_model_with_delay(state, answer_text, std::time::Duration::ZERO).await
}

async fn mock_model_with_delay(
    state: &AppState,
    answer_text: &'static str,
    embedding_delay: std::time::Duration,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    mock_model_with_delays(
        state,
        answer_text,
        embedding_delay,
        std::time::Duration::ZERO,
    )
    .await
}

async fn mock_model_with_delays(
    state: &AppState,
    answer_text: &'static str,
    embedding_delay: std::time::Duration,
    answer_delay: std::time::Duration,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    mock_model_with_capture(state, answer_text, embedding_delay, answer_delay, None).await
}

async fn mock_model_with_capture(
    state: &AppState,
    answer_text: &'static str,
    embedding_delay: std::time::Duration,
    answer_delay: std::time::Duration,
    captured: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let embed_calls = calls.clone();
    let chat_calls = calls.clone();
    let upstream = Router::new()
        .route("/v1/embeddings", post(move || { let calls = embed_calls.clone(); async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(embedding_delay).await;
            Json(json!({"object":"list","data":[{"object":"embedding","index":0,"embedding":[1.0,0.0,0.0]}],"model":"embed-test","usage":{"prompt_tokens":7,"total_tokens":7}}))
        }}))
        .route("/v1/chat/completions", post(move |Json(body): Json<Value>| { let calls = chat_calls.clone(); let captured = captured.clone(); async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if let Some(captured) = captured { captured.lock().unwrap().push(body.clone()); }
            tokio::time::sleep(answer_delay).await;
            let system = body["messages"][0]["content"].as_str().unwrap_or("");
            let answer = if system.contains("改写器") { "alpha" } else if system.contains("重排器") { "[1,0]" } else { answer_text };
            Json(json!({"id":"test-answer","object":"chat.completion","model":"chat-test","choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let channel = Repository::new(state.db.pool.clone()).create_channel(&serde_json::from_value(json!({
        "name":"local-mock", "type":"openai", "base_url":format!("http://127.0.0.1:{port}/v1"), "api_key":"mock-upstream",
        "models":["chat-test","embed-test"], "protocol":"openai", "provider":"custom", "native_base_url":format!("http://127.0.0.1:{port}/v1"), "native_endpoints":["chat_completions","embeddings"]
    })).unwrap()).await.unwrap();
    (channel.id, calls, task)
}

#[tokio::test]
async fn rag_reasoning_levels_reach_the_canonical_gateway_body_without_claiming_application() {
    let (state, key, first, _, app) = setup().await;
    let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let (_, calls, task) = mock_model_with_capture(
        &state,
        "BCD",
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
        Some(captured.clone()),
    )
    .await;
    document(&state, &first.id, "alpha answer is BCD").await;
    for (index, level) in [
        None,
        Some("default"),
        Some("none"),
        Some("low"),
        Some("medium"),
        Some("high"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"keyword"});
        if let Some(level) = level {
            input["reasoning_effort"] = json!(level);
        }
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = body(response).await;
        let sent = captured.lock().unwrap()[index].clone();
        if let Some(level @ ("none" | "low" | "medium" | "high")) = level {
            assert_eq!(
                result["reasoning"],
                json!({"requested":level,"status":"requested"})
            );
            assert_eq!(sent["reasoning_effort"], level);
        } else {
            assert!(result.get("reasoning").is_none());
            assert!(sent.get("reasoning_effort").is_none());
        }
        assert_eq!(sent["model"], "chat-test");
        for provider_field in ["thinking", "enable_thinking", "reasoning"] {
            assert!(sent.get(provider_field).is_none());
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "每个请求仅一次生成，不重试或换模型"
    );
    task.abort();
}

#[tokio::test]
async fn rag_invalid_reasoning_and_explicit_deep_research_are_rejected_before_upstream() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    for (effort, deep) in [
        ("", false),
        ("HIGH", false),
        ("auto", false),
        ("max", false),
        ("high", true),
        ("none", true),
    ] {
        let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","reasoning_effort":effort,"deep_research":deep});
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let result = body(response).await;
        assert_eq!(result["error"]["code"], "invalid_reasoning_effort");
        if deep {
            assert!(result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("deep_research"));
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn rag_empty_retrieval_records_reasoning_as_not_sent() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    for effort in [None, Some("default"), Some("high")] {
        let mut input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"keyword"});
        if let Some(effort) = effort {
            input["reasoning_effort"] = json!(effort);
        }
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = body(response).await;
        if effort == Some("high") {
            assert_eq!(
                result["reasoning"],
                json!({"requested":"high","status":"not_sent"})
            );
        } else {
            assert!(result.get("reasoning").is_none());
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn rag_calls_embedding_and_chat_with_the_callers_quota_and_logs() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha answer is BCD").await;
    let input =
        json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"vector"});
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &input.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(response).await;
    assert_eq!(result["answer"], "BCD");
    assert_eq!(result["sources"].as_array().unwrap().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let quota: i64 = sqlx::query_scalar("SELECT quota_used FROM api_keys WHERE id = ?")
        .bind(&key.id)
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(quota, 21);
    let logs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_logs WHERE api_key_id = ? AND status_code = 200",
    )
    .bind(&key.id)
    .fetch_one(&state.db.pool)
    .await
    .unwrap();
    assert_eq!(logs, 2);
    let result = body(
        app.oneshot(rpc(&key.key, "ask_knowledge_base", input))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["result"]["isError"], false);
    assert!(result.to_string().contains("BCD"));
    let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_conversations WHERE kb_id = ?")
        .bind(&first.id)
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(
        history, 0,
        "external callers must not share conversation history"
    );
    task.abort();
}

#[tokio::test]
async fn denied_models_channels_and_exhausted_quota_never_reach_upstream() {
    let (state, key, first, _, app) = setup().await;
    let (channel, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha answer is BCD").await;
    for (column, value, mode, expected) in [
        (
            "allowed_models",
            json!(["other"]).to_string(),
            "vector",
            StatusCode::FORBIDDEN,
        ),
        (
            "denied_models",
            json!(["embed-test"]).to_string(),
            "vector",
            StatusCode::FORBIDDEN,
        ),
        (
            "denied_models",
            json!(["chat-test"]).to_string(),
            "keyword",
            StatusCode::FORBIDDEN,
        ),
        (
            "denied_channels",
            json!([channel]).to_string(),
            "vector",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "allowed_channels",
            json!(["other"]).to_string(),
            "vector",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        sqlx::query(&format!("UPDATE api_keys SET {column} = ? WHERE id = ?"))
            .bind(value)
            .bind(&key.id)
            .execute(&state.db.pool)
            .await
            .unwrap();
        let input =
            json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":mode});
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{column} {mode}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        sqlx::query(&format!("UPDATE api_keys SET {column} = '[]' WHERE id = ?"))
            .bind(&key.id)
            .execute(&state.db.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE api_keys SET quota_used = quota_limit WHERE id = ?")
        .bind(&key.id)
        .execute(&state.db.pool)
        .await
        .unwrap();
    let response = app
        .oneshot(request(
            "GET",
            &format!("/api/kb/search?q=alpha&kb_id={}", first.id),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn rewrite_and_rerank_use_the_same_key_and_accounting() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha first paragraph").await;
    document(&state, &first.id, "alpha second paragraph").await;
    state
        .settings
        .set_many(&[
            ("kb.query_rewrite".into(), json!(true)),
            ("kb.rerank_enabled".into(), json!(true)),
        ])
        .unwrap();
    let input = json!({"kb_id":first.id,"question":"alpha?","model":"chat-test","search_mode":"vector","history":[{"role":"user","content":"alpha"}]});
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &input.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["answer"], "BCD");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    let quota: i64 = sqlx::query_scalar("SELECT quota_used FROM api_keys WHERE id = ?")
        .bind(&key.id)
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(quota, 49);
    task.abort();
}

#[tokio::test]
async fn embedding_spend_is_checked_before_answer_generation() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha answer BCD").await;
    sqlx::query("UPDATE api_keys SET quota_limit = 7 WHERE id = ?")
        .bind(&key.id)
        .execute(&state.db.pool)
        .await
        .unwrap();
    let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"vector","diagnostics":true});
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &input.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let result = body(response).await;
    assert_eq!(result["error"]["stage"], "answer");
    assert_eq!(result["error"]["code"], "quota_or_rate_limited");
    assert_eq!(result["diagnostics"]["stages"][1]["status"], "passed");
    assert_eq!(result["diagnostics"]["stages"][3]["status"], "failed");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn connection_test_checks_real_rest_and_mcp_authorization() {
    let (state, key, first, _, app) = setup().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    *state.server_port.write().await = listener.local_addr().unwrap().port();
    state.server_running.store(true, Ordering::SeqCst);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let shared = test_shared(&state, None, None);
    let result = crate::commands::api_key::test_api_key_knowledge_access(
        shared.state_static.clone(),
        key.id.clone(),
        first.id.clone(),
    )
    .await
    .unwrap();
    let result = json!(result);
    assert_eq!(result["rest_ok"], true);
    assert_eq!(result["mcp_ok"], true);
    crate::commands::api_key::set_api_key_knowledge_access(
        shared.state_static.clone(),
        key.id.clone(),
        vec![],
    )
    .await
    .unwrap();
    let result = crate::commands::api_key::test_api_key_knowledge_access(
        shared.state_static,
        key.id,
        first.id,
    )
    .await
    .unwrap();
    let result = json!(result);
    assert_eq!(result["rest_status"], 403);
    assert_eq!(result["rest_ok"], false);
    assert_eq!(result["mcp_ok"], false);
    task.abort();
}

#[tokio::test]
async fn rag_diagnostics_cover_real_stages_and_correlate_gateway_logs() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha answer is BCD").await;
    let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"vector","diagnostics":true});
    let mut request = json_request("POST", "/api/kb/ask", Some(&key.key), &input.to_string());
    request
        .headers_mut()
        .insert("x-request-id", "sk-untrusted-client-value".parse().unwrap());
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(response).await;
    assert_eq!(result["answer"], "BCD");
    assert!(!result["sources"].as_array().unwrap().is_empty());
    let diagnostics = &result["diagnostics"];
    let request_id = diagnostics["request_id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
    let stages = diagnostics["stages"].as_array().unwrap();
    assert_eq!(
        stages
            .iter()
            .map(|s| s["stage"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "permission",
            "embedding",
            "retrieval",
            "answer",
            "validation"
        ]
    );
    assert!(stages
        .iter()
        .all(|s| s["status"] == "passed" && s["elapsed_ms"].is_u64()));
    let logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE api_key_id = ? AND trace_id = ? AND status_code = 200")
        .bind(&key.id).bind(request_id).fetch_one(&state.db.pool).await.unwrap();
    assert_eq!(logs, 2, "诊断编号必须对应实际网关日志");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(!result.to_string().contains("sk-untrusted-client-value"));
    task.abort();
}

#[tokio::test]
async fn rag_diagnostics_identify_missing_embedding_capability_without_calling_upstream() {
    let (state, key, first, _, app) = setup().await;
    let (channel, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha answer is BCD").await;
    sqlx::query("UPDATE channels SET native_endpoints = '[\"chat_completions\"]' WHERE id = ?")
        .bind(&channel)
        .execute(&state.db.pool)
        .await
        .unwrap();
    for mode in ["hybrid", "vector"] {
        let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":mode,"diagnostics":true});
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let result = body(response).await;
        assert_eq!(result["error"]["stage"], "embedding");
        assert_eq!(result["error"]["code"], "endpoint_not_configured");
        assert_eq!(result["diagnostics"]["stages"][0]["status"], "passed");
        assert_eq!(result["diagnostics"]["stages"][1]["status"], "failed");
        let trace = result["error"]["request_id"].as_str().unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_logs WHERE trace_id = ? AND status_code = 501",
        )
        .bind(trace)
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"keyword","diagnostics":true});
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &input.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(response).await;
    assert_eq!(result["diagnostics"]["stages"][1]["stage"], "embedding");
    assert_eq!(result["diagnostics"]["stages"][1]["status"], "skipped");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn rag_diagnostics_reject_empty_retrieval_without_changing_regular_ask() {
    let (_, key, first, _, app) = setup().await;
    for diagnostics in [false, true] {
        let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"keyword","diagnostics":diagnostics});
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if diagnostics {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            }
        );
        let result = body(response).await;
        if diagnostics {
            assert_eq!(result["error"]["code"], "retrieval_empty");
            assert_eq!(result["error"]["stage"], "retrieval");
            assert_eq!(result["diagnostics"]["stages"][2]["status"], "failed");
        } else {
            assert!(result["sources"].as_array().unwrap().is_empty());
            assert!(result.get("diagnostics").is_none());
        }
    }
}

#[tokio::test]
async fn rag_diagnostics_reject_empty_answers_and_missing_sources() {
    for (answer_text, history, content, expected_status, expected_code) in [
        (
            "   ",
            "",
            "alpha answer is BCD",
            StatusCode::BAD_GATEWAY,
            "answer_empty",
        ),
        (
            "BCD",
            "x",
            "alpha answer is BCD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "sources_empty",
        ),
        (
            "BCD",
            "",
            "   ",
            StatusCode::UNPROCESSABLE_ENTITY,
            "sources_empty",
        ),
    ] {
        let (state, key, first, _, app) = setup().await;
        let (_, calls, task) = mock_model_with_answer(&state, answer_text).await;
        document(&state, &first.id, content).await;
        let mut input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"vector","diagnostics":true});
        if !history.is_empty() {
            input["history"] = json!([{"role":"user","content":history.repeat(40000)}]);
        }
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status);
        let result = body(response).await;
        assert_eq!(result["error"]["code"], expected_code);
        assert_eq!(result["error"]["stage"], "validation");
        assert_eq!(result["diagnostics"]["stages"][4]["status"], "failed");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }
}

#[tokio::test]
async fn rag_regular_ask_preserves_empty_answer_while_diagnostics_rejects_it() {
    let (state, key, first, _, app) = setup().await;
    let (_, _, task) = mock_model_with_answer(&state, "").await;
    document(&state, &first.id, "alpha answer is BCD").await;
    for diagnostics in [false, true] {
        let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"vector","diagnostics":diagnostics});
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/kb/ask",
                Some(&key.key),
                &input.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if diagnostics {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::OK
            }
        );
        let result = body(response).await;
        if diagnostics {
            assert_eq!(result["error"]["code"], "answer_empty");
        } else {
            assert_eq!(result["answer"], "");
        }
    }
    task.abort();
}

#[tokio::test]
async fn rag_diagnostics_identify_real_upstream_timeout_without_changing_status() {
    let (state, key, first, _, app) = setup().await;
    let (channel, calls, task) =
        mock_model_with_delay(&state, "BCD", std::time::Duration::from_secs(3)).await;
    sqlx::query("UPDATE channels SET timeout_secs = 1 WHERE id = ?")
        .bind(channel)
        .execute(&state.db.pool)
        .await
        .unwrap();
    state
        .settings
        .set_many(&[("retry.enabled".into(), json!(false))])
        .unwrap();
    document(&state, &first.id, "alpha answer is BCD").await;
    let input = json!({"kb_id":first.id,"question":"alpha","model":"chat-test","search_mode":"vector","diagnostics":true});
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &input.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let result = body(response).await;
    assert_eq!(result["error"]["stage"], "embedding");
    assert_eq!(result["error"]["code"], "model_timeout");
    assert_eq!(result["diagnostics"]["stages"][1]["status"], "failed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "关闭重试后只进行一次真实模型请求"
    );
    let request_id = result["error"]["request_id"].as_str().unwrap();
    let logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE api_key_id = ? AND trace_id = ? AND status_code = 502")
        .bind(&key.id).bind(request_id).fetch_one(&state.db.pool).await.unwrap();
    assert_eq!(logs, 1);
    task.abort();
}

#[tokio::test]
async fn rag_budget_stops_slow_embedding_without_another_key_or_answer() {
    let (state, key, first, _, app) = setup().await;
    let (channel, calls, task) =
        mock_model_with_delay(&state, "BCD", std::time::Duration::from_secs(2)).await;
    Repository::new(state.db.pool.clone())
        .replace_channel_api_keys(
            &channel,
            &serde_json::from_value::<Vec<crate::db::models::ChannelApiKeyInput>>(
                json!([{"api_key":"extra-one","weight":1},{"api_key":"extra-two","weight":1}]),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    document(&state, &first.id, "alpha answer BCD").await;
    let started = std::time::Instant::now();
    let response = app.oneshot(json_request("POST", "/api/kb/ask", Some(&key.key),
        &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":200,"diagnostics":true}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < std::time::Duration::from_millis(700));
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    let result = body(response).await;
    assert_eq!(result["error"]["code"], "rag_deadline_exceeded");
    assert_eq!(result["error"]["request_id"], request_id);
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "预算到期后不能轮换 Key 或生成回答"
    );
    task.abort();
}

#[tokio::test]
async fn rag_explicit_keyword_fallback_uses_remaining_budget_and_reports_actual_mode() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) =
        mock_model_with_delay(&state, "BCD", std::time::Duration::from_secs(2)).await;
    document(&state, &first.id, "alpha answer BCD").await;
    let started = std::time::Instant::now();
    let response = app.oneshot(json_request("POST", "/api/kb/ask", Some(&key.key),
        &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":1000,"allow_keyword_fallback":true,"diagnostics":true}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(response).await;
    assert_eq!(result["answer"], "BCD");
    assert_eq!(result["retrieval_mode"], "keyword");
    assert_eq!(result["degradation_reason"], "rag_deadline_exceeded");
    assert_eq!(result["diagnostics"]["stages"][1]["status"], "degraded");
    assert!(!result["sources"].as_array().unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    task.abort();
}

#[tokio::test]
async fn rag_fallback_cannot_bypass_revoked_models_or_exhausted_quota() {
    for quota in [false, true] {
        let (state, key, first, _, app) = setup().await;
        let (_, calls, task) = mock_model(&state).await;
        document(&state, &first.id, "alpha answer BCD").await;
        if quota {
            sqlx::query("UPDATE api_keys SET quota_used = quota_limit WHERE id = ?")
                .bind(&key.id)
                .execute(&state.db.pool)
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE api_keys SET denied_models = '[\"embed-test\"]' WHERE id = ?")
                .bind(&key.id)
                .execute(&state.db.pool)
                .await
                .unwrap();
        }
        let response = app.oneshot(json_request("POST", "/api/kb/ask", Some(&key.key),
            &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":500,"allow_keyword_fallback":true}).to_string())).await.unwrap();
        assert_eq!(
            response.status(),
            if quota {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::FORBIDDEN
            }
        );
        let result = body(response).await;
        assert!(result.get("answer").is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }
}

#[tokio::test]
async fn rag_future_abort_does_not_continue_to_answer() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) =
        mock_model_with_delay(&state, "BCD", std::time::Duration::from_millis(350)).await;
    document(&state, &first.id, "alpha answer BCD").await;
    let request = json_request("POST", "/api/kb/ask", Some(&key.key),
        &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":2000,"allow_keyword_fallback":true}).to_string());
    let ask = tokio::spawn(async move { app.oneshot(request).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    ask.abort();
    assert!(ask.await.unwrap_err().is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn rag_fallback_with_empty_material_is_an_error_even_without_diagnostics() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) =
        mock_model_with_delay(&state, "BCD", std::time::Duration::from_secs(2)).await;
    let response = app.oneshot(json_request("POST", "/api/kb/ask", Some(&key.key),
        &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":500,"allow_keyword_fallback":true}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(response).await["error"]["code"], "retrieval_empty");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn rag_revocation_during_answer_blocks_the_output() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model_with_delays(
        &state,
        "BCD",
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(350),
    )
    .await;
    document(&state, &first.id, "alpha answer BCD").await;
    let request = json_request(
        "POST",
        "/api/kb/ask",
        Some(&key.key),
        &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":2000})
            .to_string(),
    );
    let ask = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    set_grants(&state.db.pool, &key.id, &[]).await.unwrap();
    let response = ask.await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(body(response).await.get("answer").is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn rag_completed_answer_can_exhaust_quota_without_losing_its_response() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha answer BCD").await;
    sqlx::query("UPDATE api_keys SET quota_limit = 21 WHERE id = ?")
        .bind(&key.id)
        .execute(&state.db.pool)
        .await
        .unwrap();
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":2000})
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["answer"], "BCD");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn rag_explicit_budget_rejects_empty_answers_without_diagnostics() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model_with_answer(&state, "").await;
    document(&state, &first.id, "alpha answer BCD").await;
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/kb/ask",
            Some(&key.key),
            &json!({"kb_id":first.id,"question":"alpha","model":"chat-test","timeout_ms":2000})
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let result = body(response).await;
    assert_eq!(result["error"]["code"], "answer_empty");
    assert!(result.get("diagnostics").is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn generic_search_preserves_legacy_data_and_returns_authorized_original_content() {
    let (state, key, first, second, app) = setup().await;
    let original = "alpha 规范原文：值是 'a\u{a0}b'，运算符 !=；😀保持原文。";
    let doc_id = document(&state, &first.id, original).await;
    document(&state, &second.id, "alpha PRIVATE MATERIAL").await;
    for extended in [false, true] {
        let suffix = if extended {
            "&candidate_k=20&timeout_ms=2000&allow_keyword_fallback=true&diagnostics=true"
        } else {
            ""
        };
        let response = app
            .clone()
            .oneshot(request(
                "GET",
                &format!(
                    "/api/kb/search?q=alpha&kb_id={}&search_mode=keyword&top_k=5{suffix}",
                    first.id
                ),
                Some(&key.key),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_string();
        let result = body(response).await;
        let data = result["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["doc_id"], doc_id);
        assert_eq!(data[0]["content"], original, "原文不是摘要或业务加工文本");
        assert!(!data[0]["chunk_id"].as_str().unwrap().is_empty());
        assert!(!result.to_string().contains("PRIVATE MATERIAL"));
        if extended {
            assert_eq!(result["request_id"], request_id);
            assert_eq!(result["retrieval_mode"], "keyword");
            assert_eq!(result["diagnostics"]["request_id"], request_id);
            assert_eq!(result["diagnostics"]["stages"][1]["status"], "skipped");
            assert_eq!(result["diagnostics"]["stages"][2]["stage"], "retrieval");
        } else {
            assert_eq!(
                result.as_object().unwrap().len(),
                1,
                "旧 search 只包含 data"
            );
        }
    }
    let response = app
        .oneshot(request(
            "GET",
            &format!(
                "/api/kb/search?q=missing&kb_id={}&search_mode=keyword&diagnostics=true",
                first.id
            ),
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(response).await;
    assert_eq!(result["data"], json!([]));
    assert_eq!(result["diagnostics"]["stages"][2]["status"], "empty");
}

#[tokio::test]
async fn generic_search_uses_only_embedding_even_when_generation_features_are_enabled() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha original first paragraph").await;
    document(&state, &first.id, "alpha original second paragraph").await;
    state
        .settings
        .set_many(&[
            ("kb.query_rewrite".into(), json!(true)),
            ("kb.rerank_enabled".into(), json!(true)),
        ])
        .unwrap();
    for mode in ["vector", "hybrid"] {
        let response = app.clone().oneshot(request("GET", &format!(
            "/api/kb/search?q=alpha&kb_id={}&search_mode={mode}&top_k=1&candidate_k=20&timeout_ms=3000&diagnostics=true", first.id),
            Some(&key.key))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = body(response).await;
        assert_eq!(result["data"].as_array().unwrap().len(), 1);
        assert_eq!(result["retrieval_mode"], mode);
        let stages = result["diagnostics"]["stages"].as_array().unwrap();
        assert_eq!(
            stages
                .iter()
                .map(|s| s["stage"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["permission", "embedding", "retrieval"]
        );
        assert!(result.get("answer").is_none());
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "Search 不调用回答/改写/重排模型"
    );
    let quota: i64 = sqlx::query_scalar("SELECT quota_used FROM api_keys WHERE id = ?")
        .bind(&key.id)
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(quota, 14, "每次仅计一次 Embedding 用量");
    task.abort();
}

#[tokio::test]
async fn generic_search_budget_and_opt_in_fallback_share_the_retrieval_path() {
    for fallback in [false, true] {
        let (state, key, first, _, app) = setup().await;
        let (_, calls, task) =
            mock_model_with_delay(&state, "unused", std::time::Duration::from_secs(3)).await;
        document(&state, &first.id, "alpha usable original evidence").await;
        // 预热确切非流式客户端；不将冷初始化误当作目标网络超时。
        let _ = crate::adaptor::blocking_client(60, None);
        let started = std::time::Instant::now();
        let response = app.oneshot(request("GET", &format!(
            "/api/kb/search?q=alpha&kb_id={}&search_mode=hybrid&top_k=5&candidate_k=20&timeout_ms=1000&allow_keyword_fallback={fallback}&diagnostics=true", first.id),
            Some(&key.key))).await.unwrap();
        assert_eq!(
            response.status(),
            if fallback {
                StatusCode::OK
            } else {
                StatusCode::GATEWAY_TIMEOUT
            }
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(1400));
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_string();
        let result = body(response).await;
        if fallback {
            assert_eq!(result["retrieval_mode"], "keyword");
            assert_eq!(result["degradation_reason"], "rag_deadline_exceeded");
            assert_eq!(
                result["data"][0]["content"],
                "alpha usable original evidence"
            );
            assert_eq!(result["diagnostics"]["stages"][1]["status"], "degraded");
        } else {
            assert_eq!(result["error"]["code"], "rag_deadline_exceeded");
            assert_eq!(result["error"]["request_id"], request_id);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "超时不换 Key 或调用生成模型"
        );
        task.abort();
    }
}

#[tokio::test]
async fn generic_search_fallback_cannot_bypass_model_permission_or_quota() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) = mock_model(&state).await;
    document(&state, &first.id, "alpha original evidence").await;
    for quota in [false, true] {
        sqlx::query(if quota {
            "UPDATE api_keys SET quota_used = quota_limit, denied_models = '[]' WHERE id = ?"
        } else {
            "UPDATE api_keys SET denied_models = '[\"embed-test\"]' WHERE id = ?"
        })
        .bind(&key.id)
        .execute(&state.db.pool)
        .await
        .unwrap();
        let response = app.clone().oneshot(request("GET", &format!(
            "/api/kb/search?q=alpha&kb_id={}&search_mode=hybrid&timeout_ms=1000&allow_keyword_fallback=true&diagnostics=true", first.id),
            Some(&key.key))).await.unwrap();
        assert_eq!(
            response.status(),
            if quota {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::FORBIDDEN
            }
        );
        let result = body(response).await;
        assert!(result.get("data").is_none());
        assert_eq!(result["error"]["stage"], "permission");
        assert_eq!(
            result["error"]["code"],
            if quota {
                "quota_exceeded"
            } else {
                "access_denied"
            }
        );
        assert_eq!(result["diagnostics"]["stages"][1]["stage"], "embedding");
        assert_eq!(result["diagnostics"]["stages"][1]["status"], "failed");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    task.abort();
}

#[tokio::test]
async fn generic_search_rechecks_revocation_after_the_embedding_await() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) =
        mock_model_with_delay(&state, "unused", std::time::Duration::from_millis(150)).await;
    document(&state, &first.id, "alpha original evidence").await;
    let input = request(
        "GET",
        &format!(
            "/api/kb/search?q=alpha&kb_id={}&search_mode=vector&timeout_ms=2000&diagnostics=true",
            first.id
        ),
        Some(&key.key),
    );
    let search = tokio::spawn(async move { app.oneshot(input).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    set_grants(&state.db.pool, &key.id, &[]).await.unwrap();
    let response = search.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let result = body(response).await;
    assert!(result.get("data").is_none());
    assert_eq!(result["error"]["code"], "knowledge_access_denied");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn generic_search_future_abort_does_not_start_another_request() {
    let (state, key, first, _, app) = setup().await;
    let (_, calls, task) =
        mock_model_with_delay(&state, "unused", std::time::Duration::from_millis(250)).await;
    document(&state, &first.id, "alpha original evidence").await;
    let input = request("GET", &format!(
        "/api/kb/search?q=alpha&kb_id={}&search_mode=hybrid&timeout_ms=2000&allow_keyword_fallback=true", first.id), Some(&key.key));
    let search = tokio::spawn(async move { app.oneshot(input).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    search.abort();
    assert!(search.await.unwrap_err().is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn admin_search_new_options_keep_cross_kb_vector_fallback_without_expanding_api_key_access() {
    let (state, key, first, second, _) = setup().await;
    let (channel, calls, task) = mock_model(&state).await;
    sqlx::query("UPDATE channels SET models = '[\"chat-test\",\"embed-test\",\"text-embedding-3-small\"]' WHERE id = ?")
        .bind(channel).execute(&state.db.pool).await.unwrap();
    document(&state, &first.id, "alpha first original material").await;
    document(&state, &second.id, "alpha second original material").await;
    let admin = "test-admin-0123456789abcdef0123456789abcdef";
    let app = build_router(state.clone(), test_shared(&state, Some(admin), None));
    let legacy = app
        .clone()
        .oneshot(request(
            "GET",
            "/api/kb/search?q=alpha&search_mode=keyword&top_k=2",
            Some(admin),
        ))
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::OK);
    let legacy = body(legacy).await;
    assert_eq!(legacy["data"].as_array().unwrap().len(), 2);
    let mut expected: Vec<_> = legacy["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["chunk_id"].as_str().unwrap().to_string())
        .collect();
    expected.sort();
    for mode in ["keyword", "hybrid", "vector"] {
        let response = app.clone().oneshot(request("GET", &format!(
            "/api/kb/search?q=alpha&search_mode={mode}&top_k=2&candidate_k=20&timeout_ms=2000&allow_keyword_fallback=true"), Some(admin))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = body(response).await;
        assert_eq!(result["retrieval_mode"], "vector", "报告实际跨库检索方式");
        let mut actual: Vec<_> = result["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["chunk_id"].as_str().unwrap().to_string())
            .collect();
        actual.sort();
        assert_eq!(actual, expected, "新参数不改变管理员原有跨库结果");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4, "各次检索仅一次Embedding");
    let response = app
        .oneshot(request(
            "GET",
            "/api/kb/search?q=alpha&search_mode=keyword&top_k=2&candidate_k=20&timeout_ms=2000",
            Some(&key.key),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "API Key 仍必须指定单个已授权KB"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    task.abort();
}
