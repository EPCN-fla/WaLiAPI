//! 真实迁移、检索投影与原文窗口回归；不访问网络或个人知识库。
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use waliapi_lib::services::knowledge::{repository::KbRepository, retriever, text};

async fn fixture() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    for kb in ["current", "other"] {
        sqlx::query("INSERT INTO kb_knowledge_bases (id,name,created_at,updated_at) VALUES (?,?,'now','now')")
            .bind(kb).bind(kb).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO kb_documents (id,kb_id,filename,file_type,content_hash,status,created_at,updated_at) VALUES (?,?,'rules.pdf','pdf','document-hash','ready','now','now')")
            .bind(format!("{kb}-doc")).bind(kb).execute(&pool).await.unwrap();
    }
    pool
}

async fn chunk(pool: &SqlitePool, id: &str, kb: &str, content: &str) {
    sqlx::query("INSERT INTO kb_chunks (id,doc_id,kb_id,chunk_index,content,embedding,embedding_dim,content_hash,metadata,created_at,search_text,search_text_version) VALUES (?,?,?,0,?, ?,2,'preserved-hash','{}','now','old-projection',1)")
        .bind(id).bind(format!("{kb}-doc")).bind(kb).bind(content)
        .bind(retriever::encode_embedding(&[1.0,0.0])).execute(pool).await.unwrap();
}

#[tokio::test]
async fn keyword_search_stays_in_kb_and_returns_original_matching_window() {
    let pool = fixture().await;
    for i in 0..12 {
        chunk(
            &pool,
            &format!("noise-{i}"),
            "current",
            &format!(
                "以下控制语句不支持 FETCHALL WHERE 嵌套函数：{}",
                "FETCHALL ".repeat(20)
            ),
        )
        .await;
    }
    let rule = format!(
        "{}3.2.2. 【强制】记录 OnlyRelevantMarker 修订标识。\n检查方式：工具检查",
        "页眉\n".repeat(100)
    );
    chunk(&pool, "needed", "current", &rule).await;
    chunk(
        &pool,
        "foreign",
        "other",
        "3.2.2. 【强制】记录 OnlyRelevantMarker 修订标识。",
    )
    .await;
    let anchors = vec!["OnlyRelevantMarker".into()];
    let hits = retriever::keyword_only_search(&pool, "current", "OnlyRelevantMarker", 5)
        .await
        .unwrap();
    assert!(hits.iter().any(|r| r.chunk_id == "needed"));
    assert!(!hits.iter().any(|r| r.chunk_id == "foreign"));
    let needed = hits.iter().find(|r| r.chunk_id == "needed").unwrap();
    let (window, start) = retriever::evidence_window(&needed.content, &anchors, 200);
    assert!(start > 200 && window.contains("OnlyRelevantMarker"));
    assert_eq!(
        retriever::section_at(&rule, start).as_deref(),
        Some("3.2.2")
    );
    assert_eq!(
        window,
        rule.chars().skip(start).take(200).collect::<String>()
    );
}

#[test]
fn rule_window_and_section_keep_crlf_and_non_bmp_character_offsets() {
    let raw = "😀前言\r\n2.2.5. 【建议】其他规范。\r\n页眉🤖\r\n3.2.2. 【强制】不支持 FETCH 控制语句。\r\n检查方式：工具检查\r\n";
    let expected = raw[..raw.find("3.2.2.").unwrap()].chars().count();
    let (window, start) = retriever::evidence_window(raw, &["FETCH".into()], 200);
    assert_eq!(start, expected);
    assert_eq!(
        window,
        raw.chars().skip(expected).take(200).collect::<String>()
    );
    assert!(window.starts_with("3.2.2."));
    assert_eq!(retriever::section_at(raw, start).as_deref(), Some("3.2.2"));
    assert_eq!(
        retriever::section_at(raw, start - 1).as_deref(),
        Some("2.2.5")
    );
}

#[tokio::test]
async fn stale_nonnull_projection_is_upgraded_without_changing_original_or_vectors() {
    let pool = fixture().await;
    let raw = "超多分⻚场景、⻓度、意⻅；String s=\"Ａ①²\";";
    chunk(&pool, "old", "current", raw).await;
    chunk(&pool, "unrelated", "other", raw).await;
    let before: (String, Vec<u8>, String) =
        sqlx::query_as("SELECT content,embedding,content_hash FROM kb_chunks WHERE id='old'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let repo = KbRepository::new(pool.clone());
    assert_eq!(
        repo.backfill_search_text_for_kb("current").await.unwrap(),
        1
    );
    assert_eq!(
        repo.backfill_search_text_for_kb("current").await.unwrap(),
        0
    );
    let after: (String, Vec<u8>, String) =
        sqlx::query_as("SELECT content,embedding,content_hash FROM kb_chunks WHERE id='old'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let (projection, version): (String, i64) =
        sqlx::query_as("SELECT search_text,search_text_version FROM kb_chunks WHERE id='old'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, text::SEARCH_PROJECTION_VERSION);
    for word in ["分页", "长度", "意见"] {
        assert!(projection.split_whitespace().any(|t| t == word));
    }
    let other_version: i64 =
        sqlx::query_scalar("SELECT search_text_version FROM kb_chunks WHERE id='unrelated'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(other_version, 1);
    let hits = retriever::keyword_only_search(&pool, "current", "分页", 5)
        .await
        .unwrap();
    assert_eq!(hits[0].content, raw);
}

#[tokio::test]
async fn committed_projection_batches_resume_after_interruption() {
    let pool = fixture().await;
    for i in 0..129 {
        chunk(&pool, &format!("c{i:03}"), "current", "超多分⻚场景").await;
    }
    sqlx::query("CREATE TRIGGER projection_interrupt BEFORE UPDATE OF search_text_version ON kb_chunks WHEN NEW.id='c128' BEGIN SELECT RAISE(FAIL,'fixture interruption'); END").execute(&pool).await.unwrap();
    let repo = KbRepository::new(pool.clone());
    assert!(repo.backfill_search_text_for_kb("current").await.is_err());
    let done: i64 =
        sqlx::query_scalar("SELECT count(*) FROM kb_chunks WHERE search_text_version=2")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(done, 128);
    sqlx::query("DROP TRIGGER projection_interrupt")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        repo.backfill_search_text_for_kb("current").await.unwrap(),
        1
    );
    assert_eq!(
        repo.backfill_search_text_for_kb("current").await.unwrap(),
        0
    );
}

#[tokio::test]
async fn content_edit_invalidates_version_and_rebuilds_current_text() {
    let pool = fixture().await;
    chunk(&pool, "edited", "current", "旧的分⻚规范").await;
    let repo = KbRepository::new(pool.clone());
    repo.backfill_search_text_for_kb("current").await.unwrap();
    sqlx::query("UPDATE kb_chunks SET content='新的⻓度规范' WHERE id='edited'")
        .execute(&pool)
        .await
        .unwrap();
    let pending: (Option<String>, i64) =
        sqlx::query_as("SELECT search_text,search_text_version FROM kb_chunks WHERE id='edited'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, (None, 0));
    repo.backfill_search_text_for_kb("current").await.unwrap();
    assert!(retriever::keyword_only_search(&pool, "current", "长度", 5)
        .await
        .unwrap()
        .iter()
        .any(|r| r.chunk_id == "edited"));
    assert!(retriever::keyword_only_search(&pool, "current", "分页", 5)
        .await
        .unwrap()
        .is_empty());
}
