//! 使用真实 HTTP 响应验证向量与输入文本的对应关系和异常响应保护。
use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use waliapi_lib::{
    db::repository::Repository,
    server::event_bridge::EventSink,
    services::knowledge::{
        embedder, processor,
        repository::{ChunkInsert, KbRepository},
        retriever,
    },
    settings_store::SettingsStore,
};

async fn model(response: Value) -> (Repository, Arc<Mutex<Value>>, tokio::task::JoinHandle<()>) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let (base_url, response, server) = mock(response).await;
    let repo = Repository::new(pool);
    repo.create_channel(
        &serde_json::from_value(json!({
            "name": "embedding-response-test", "type": "openai", "base_url": base_url,
            "api_key": "test-only", "models": ["embed-test"],
            "protocol": "openai", "provider": "custom", "native_base_url": base_url,
            "native_endpoints": ["embeddings"]
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    (repo, response, server)
}

async fn mock(response: Value) -> (String, Arc<Mutex<Value>>, tokio::task::JoinHandle<()>) {
    let response = Arc::new(Mutex::new(response));
    let state = response.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let state = state.clone();
            async move { Json(state.lock().unwrap().clone()) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base_url, response, server)
}

#[tokio::test]
async fn embedding_indexes_preserve_input_order() {
    let (repo, _, server) = model(json!({"data": [
        {"index": 1, "embedding": [0.0, 1.0]},
        {"index": 0, "embedding": [1.0, 0.0]}
    ]}))
    .await;
    let actual = embedder::embed(&["first".into(), "second".into()], "embed-test", &repo)
        .await
        .unwrap();
    server.abort();
    assert_eq!(
        actual,
        vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        "响应数组顺序不能改变向量所属的输入文本"
    );
}

#[tokio::test]
async fn malformed_embeddings_are_rejected_as_a_whole() {
    let (repo, response, server) = model(json!({})).await;
    for (reason, data) in [
        (
            "empty vector",
            json!([{"index": 0, "embedding": []}, {"index": 1, "embedding": []}]),
        ),
        (
            "non-number",
            json!([{"index": 0, "embedding": [1.0, "bad"]}, {"index": 1, "embedding": [1.0, "bad"]}]),
        ),
        (
            "overflow",
            json!([{"index": 0, "embedding": [1e100]}, {"index": 1, "embedding": [1e100]}]),
        ),
        (
            "duplicate index",
            json!([{"index": 0, "embedding": [1.0]}, {"index": 0, "embedding": [2.0]}]),
        ),
        (
            "out of bounds",
            json!([{"index": 0, "embedding": [1.0]}, {"index": 2, "embedding": [2.0]}]),
        ),
        (
            "missing index",
            json!([{"index": 0, "embedding": [1.0]}, {"embedding": [2.0]}]),
        ),
        ("wrong count", json!([{"index": 0, "embedding": [1.0]}])),
        (
            "dimension mismatch",
            json!([{"index": 0, "embedding": [1.0]}, {"index": 1, "embedding": [1.0, 2.0]}]),
        ),
    ] {
        *response.lock().unwrap() = json!({"data": data});
        assert!(
            embedder::embed(&["first".into(), "second".into()], "embed-test", &repo)
                .await
                .is_err(),
            "必须拒绝整批异常向量: {reason}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn legacy_responses_without_any_index_keep_array_order() {
    let (repo, _, server) = model(json!({"data": [
        {"embedding": [1.0, 0.0]}, {"embedding": [0.0, 1.0]}
    ]}))
    .await;
    let actual = embedder::embed(&["first".into(), "second".into()], "embed-test", &repo)
        .await
        .unwrap();
    server.abort();
    assert_eq!(actual, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
}

#[tokio::test]
async fn malformed_primary_response_still_allows_channel_failover() {
    let (repo, _, primary) = model(json!({"data":[{"index":0,"embedding":[]}]})).await;
    let (url, _, fallback) = mock(json!({"data":[{"index":0,"embedding":[1.0,0.0]}]})).await;
    repo.create_channel(
        &serde_json::from_value(json!({
            "name":"fallback", "type":"openai", "base_url":url,
            "api_key":"test-only", "models":["embed-test"], "priority":-1,
            "protocol":"openai", "provider":"custom", "native_base_url":url,
            "native_endpoints":["embeddings"]
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let result = embedder::embed(&["query".into()], "embed-test", &repo).await;
    primary.abort();
    fallback.abort();
    assert_eq!(result.unwrap(), vec![vec![1.0, 0.0]]);
}

#[tokio::test]
async fn malformed_response_during_reindex_preserves_the_ready_document() {
    let (main_repo, _, server) = model(json!({"data":[{"index":0,"embedding":[]}]})).await;
    let pool = main_repo.pool();
    let repo = KbRepository::new(pool.clone());
    let kb = repo
        .create_kb(
            &serde_json::from_value(json!({
                "name":"preserve-ready", "embedding_model":"embed-test"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let directory = std::env::temp_dir().join(format!("rag-invalid-reindex-{}", kb.id));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("source.txt");
    std::fs::write(&path, "changed text requires a new embedding").unwrap();
    let doc = repo
        .create_document(&kb.id, "source.txt", path.to_str(), "txt", 10, "old-hash")
        .await
        .unwrap();
    repo.replace_document_chunks(
        &doc.id,
        &kb.id,
        &[ChunkInsert {
            id: "old-chunk".into(),
            doc_id: doc.id.clone(),
            kb_id: kb.id.clone(),
            chunk_index: 0,
            content: "old searchable text".into(),
            token_count: 5,
            embedding: retriever::encode_embedding(&[1.0, 0.0]),
            embedding_dim: 2,
            metadata: "{}".into(),
            content_hash: Some("old-hash".into()),
            created_at: "now".into(),
        }],
        None,
    )
    .await
    .unwrap();
    let (tx, _) = tokio::sync::broadcast::channel(100);
    let events = EventSink::headless(tx);
    retriever::build_index(pool, &kb.id, &events).await.unwrap();
    let settings = SettingsStore::file(directory.join("settings.json"));
    let result = processor::reindex_document(pool, &events, &doc.id, &settings, &directory).await;
    let found = retriever::search(pool, &kb.id, &[1.0, 0.0], 5)
        .await
        .unwrap();
    let after = repo.get_document(&doc.id).await.unwrap();
    let kb_after = repo.get_kb(&kb.id).await.unwrap();
    retriever::drop_index(pool, &kb.id).await.unwrap();
    server.abort();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(result.is_err());
    assert_eq!(after.status, "ready");
    assert_eq!((after.chunk_count, after.token_count), (1, 5));
    assert_eq!(kb_after.chunk_count, 1);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].chunk_id, "old-chunk");
    assert_eq!(found[0].content, "old searchable text");
}
