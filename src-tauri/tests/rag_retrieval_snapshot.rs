//! 检索快照、候选投影和失效边界：只使用临时 SQLite 与随机知识库 ID。
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    Acquire, SqlitePool,
};
use waliapi_lib::server::event_bridge::EventSink;
use waliapi_lib::services::knowledge::{
    repository::{ChunkInsert, KbRepository},
    retriever,
};

struct TestDirectory(std::path::PathBuf);
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn fixture() -> (TestDirectory, SqlitePool, String, String, EventSink) {
    let directory =
        TestDirectory(std::env::temp_dir().join(format!("rag-snapshot-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&directory.0).unwrap();
    let options = SqliteConnectOptions::new()
        .filename(directory.0.join("test.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let repo = KbRepository::new(pool.clone());
    let kb = repo
        .create_kb(&serde_json::from_value(serde_json::json!({"name":"snapshot"})).unwrap())
        .await
        .unwrap();
    let doc = repo
        .create_document(&kb.id, "test.txt", None, "txt", 1, "hash")
        .await
        .unwrap();
    let (sender, _) = tokio::sync::broadcast::channel(16);
    (directory, pool, kb.id, doc.id, EventSink::headless(sender))
}

fn chunk(kb: &str, doc: &str, id: &str, content: &str, vector: &[f32]) -> ChunkInsert {
    ChunkInsert {
        id: id.into(),
        doc_id: doc.into(),
        kb_id: kb.into(),
        chunk_index: 0,
        content: content.into(),
        token_count: 1,
        embedding: retriever::encode_embedding(vector),
        embedding_dim: vector.len() as i64,
        metadata: "{}".into(),
        content_hash: Some(id.into()),
        created_at: "now".into(),
    }
}

#[tokio::test]
async fn candidate_projection_and_complete_fallback_share_a_read_snapshot() {
    let (_directory, pool, kb, doc, _) = fixture().await;
    let repo = KbRepository::new(pool.clone());
    repo.replace_document_chunks(
        &doc,
        &kb,
        &[chunk(&kb, &doc, "old", "old body", &[1.0, 0.0])],
        None,
    )
    .await
    .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let mut snapshot = connection.begin().await.unwrap();
    let before = KbRepository::search_chunk_identities(&mut snapshot, &kb)
        .await
        .unwrap();
    assert_eq!(before[0].embedding_bytes, 16, "bincode 向量含 8 字节长度头");
    repo.replace_document_chunks(
        &doc,
        &kb,
        &[chunk(&kb, &doc, "new", "new body", &[0.0, 1.0])],
        None,
    )
    .await
    .unwrap();
    let ids = before.into_iter().map(|row| row.id).collect::<Vec<_>>();
    let candidates = KbRepository::search_chunks_by_ids(&mut snapshot, &kb, &ids)
        .await
        .unwrap();
    let fallback = KbRepository::search_vector_chunks(&mut snapshot, &kb)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].content, "old body");
    assert_eq!(fallback[0].id, "old");
    snapshot.rollback().await.unwrap();
    drop(connection);
    let current = retriever::search(&pool, &kb, &[0.0, 1.0], 1).await.unwrap();
    assert_eq!(current[0].content, "new body");
}

#[tokio::test]
async fn failed_replacement_keeps_the_existing_index_snapshot_searchable() {
    let (_directory, pool, kb, doc, events) = fixture().await;
    let repo = KbRepository::new(pool.clone());
    let other = repo
        .create_document(&kb, "other.txt", None, "txt", 1, "other")
        .await
        .unwrap();
    repo.replace_document_chunks(
        &doc,
        &kb,
        &[chunk(&kb, &doc, "original", "original body", &[1.0, 0.0])],
        None,
    )
    .await
    .unwrap();
    repo.replace_document_chunks(
        &other.id,
        &kb,
        &[chunk(&kb, &other.id, "conflict", "other body", &[0.0, 1.0])],
        None,
    )
    .await
    .unwrap();
    retriever::build_index(&pool, &kb, &events).await.unwrap();
    assert!(repo
        .replace_document_chunks(
            &doc,
            &kb,
            &[chunk(&kb, &doc, "conflict", "invalid", &[0.0, 1.0])],
            None
        )
        .await
        .is_err());
    let result = retriever::search(&pool, &kb, &[1.0, 0.0], 1).await.unwrap();
    assert_eq!(result[0].chunk_id, "original");
    retriever::drop_index(&pool, &kb).await.unwrap();
}

#[tokio::test]
async fn dimension_and_malformed_vector_invalidate_matching_index_ids() {
    let (_directory, pool, kb, doc, events) = fixture().await;
    let repo = KbRepository::new(pool.clone());
    repo.replace_document_chunks(
        &doc,
        &kb,
        &[chunk(&kb, &doc, "original", "body", &[1.0, 0.0])],
        None,
    )
    .await
    .unwrap();
    retriever::build_index(&pool, &kb, &events).await.unwrap();
    sqlx::query("UPDATE kb_chunks SET embedding_dim = 0 WHERE id = 'original'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        retriever::search(&pool, &kb, &[1.0, 0.0], 1).await.unwrap()[0].chunk_id,
        "original",
        "旧切片维度未知时仍通过解码验证实际维度"
    );
    assert!(retriever::search(&pool, &kb, &[], 1)
        .await
        .unwrap()
        .is_empty());
    sqlx::query("UPDATE kb_knowledge_bases SET embedding_dim = 3 WHERE id = ?")
        .bind(&kb)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE kb_chunks SET embedding_dim = 2 WHERE id = 'original'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(retriever::search(&pool, &kb, &[1.0, 0.0], 1)
        .await
        .unwrap()
        .is_empty());
    sqlx::query("UPDATE kb_knowledge_bases SET embedding_dim = 2 WHERE id = ?")
        .bind(&kb)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE kb_chunks SET embedding_dim = 3 WHERE id = 'original'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(retriever::search(&pool, &kb, &[1.0, 0.0], 1)
        .await
        .unwrap()
        .is_empty());
    sqlx::query("UPDATE kb_chunks SET embedding_dim = 2, embedding = X'00' WHERE id = 'original'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(retriever::search(&pool, &kb, &[1.0, 0.0], 1)
        .await
        .unwrap()
        .is_empty());
    retriever::drop_index(&pool, &kb).await.unwrap();
}

#[tokio::test]
async fn fts_search_does_not_backfill_another_knowledge_base() {
    let (_directory, pool, kb, doc, _) = fixture().await;
    let repo = KbRepository::new(pool.clone());
    let other = repo
        .create_kb(&serde_json::from_value(serde_json::json!({"name":"other"})).unwrap())
        .await
        .unwrap();
    let other_doc = repo
        .create_document(&other.id, "other.txt", None, "txt", 1, "other")
        .await
        .unwrap();
    repo.replace_document_chunks(
        &doc,
        &kb,
        &[chunk(&kb, &doc, "current", "中文日志", &[1.0, 0.0])],
        None,
    )
    .await
    .unwrap();
    repo.replace_document_chunks(
        &other_doc.id,
        &other.id,
        &[chunk(
            &other.id,
            &other_doc.id,
            "other",
            "中文日志",
            &[1.0, 0.0],
        )],
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE kb_chunks SET search_text = NULL")
        .execute(&pool)
        .await
        .unwrap();
    let result = retriever::keyword_only_search(&pool, &kb, "日志", 5)
        .await
        .unwrap();
    assert_eq!(result[0].chunk_id, "current");
    let pending: Vec<String> =
        sqlx::query_scalar("SELECT id FROM kb_chunks WHERE search_text IS NULL")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(pending, ["other"]);
    assert_eq!(repo.backfill_search_text().await.unwrap(), 1);
}

/// 可复跑的 SQL 投影成本比较；首次读取不是清空操作系统页缓存的真实冷读。
#[tokio::test]
#[ignore = "fixture measurement, run explicitly with --nocapture"]
async fn fixture_projection_measurement() {
    let (_directory, pool, kb, doc, _) = fixture().await;
    let repo = KbRepository::new(pool.clone());
    let vector = vec![0.125; 1536];
    let content = "数据库索引规范中文投影 ".repeat(400);
    let chunks = (0..65)
        .map(|index| chunk(&kb, &doc, &format!("row-{index:03}"), &content, &vector))
        .collect::<Vec<_>>();
    repo.replace_document_chunks(&doc, &kb, &chunks, None)
        .await
        .unwrap();
    let ids = chunks
        .iter()
        .take(5)
        .map(|chunk| chunk.id.clone())
        .collect::<Vec<_>>();
    let mut full = Vec::new();
    let mut projection = Vec::new();
    let mut full_payload = 0;
    let mut projection_payload = 0;
    for _ in 0..31 {
        let started = std::time::Instant::now();
        let rows = repo.get_chunks_by_kb(&kb).await.unwrap();
        full.push(started.elapsed().as_micros());
        full_payload = rows
            .iter()
            .map(|(id, content, metadata, blob, filename, doc)| {
                id.len() + content.len() + metadata.len() + blob.len() + filename.len() + doc.len()
            })
            .sum::<usize>();
        let started = std::time::Instant::now();
        let mut connection = pool.acquire().await.unwrap();
        let mut snapshot = connection.begin().await.unwrap();
        let identities = KbRepository::search_chunk_identities(&mut snapshot, &kb)
            .await
            .unwrap();
        let rows = KbRepository::search_chunks_by_ids(&mut snapshot, &kb, &ids)
            .await
            .unwrap();
        snapshot.rollback().await.unwrap();
        projection.push(started.elapsed().as_micros());
        projection_payload = identities
            .iter()
            .map(|row| row.id.len() + 32 + row.index_status.len())
            .sum::<usize>()
            + rows
                .iter()
                .map(|row| {
                    row.id.len()
                        + row.content.len()
                        + row.metadata.len()
                        + row.filename.len()
                        + row.doc_id.len()
                })
                .sum::<usize>();
        assert_eq!(rows.len(), 5);
    }
    assert!(projection_payload < full_payload / 8);
    let (full_first, projection_first) = (full.remove(0), projection.remove(0));
    full.sort_unstable();
    projection.sort_unstable();
    println!(
        "{}",
        serde_json::json!({
            "fixture": {"chunks":65,"dimension":1536,"top_k":5,"content_bytes_per_chunk":content.len(),"samples":30},
            "scope":"SQLite fetch and read transaction only; excludes index, embedding, model, permissions, OS-cold cache",
            "first_read_us":{"full":full_first,"projection":projection_first},
            "warm_us":{"full_p50":full[14],"full_p95":full[28],"projection_p50":projection[14],"projection_p95":projection[28]},
            "application_payload_bytes":{"full":full_payload,"projection_estimate":projection_payload}
        })
    );
}
