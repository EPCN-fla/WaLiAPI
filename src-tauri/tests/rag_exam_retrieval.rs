//! 真实迁移与 FTS 的离线考试检索回归；不访问网络或个人知识库。
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use waliapi_lib::services::knowledge::{
    exam::{self, ExamQuestion},
    models::SearchResult,
    repository::KbRepository,
    retriever::{self, ScoredSearchResult},
    text,
};

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

fn question() -> ExamQuestion {
    serde_json::from_value(serde_json::json!({
        "type":"single", "polarity":"positive", "stem":"以下哪些控制语句不支持",
        "options":[{"id":"A","text":"FETCH"},{"id":"B","text":"FETCHALL"},{"id":"C","text":"WHERE"}]
    }))
    .unwrap()
}

#[tokio::test]
async fn exact_anchors_recover_rules_before_final_context_selection_and_stay_in_kb() {
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
        "{}3.2.2. 【强制】不支持 FETCH 控制语句。\n检查方式：工具检查",
        "页眉\n".repeat(100)
    );
    chunk(&pool, "needed", "current", &rule).await;
    chunk(
        &pool,
        "foreign",
        "other",
        "3.2.2. 【强制】不支持 FETCH 控制语句。",
    )
    .await;
    let exam = question();
    let anchors = exam
        .options
        .iter()
        .map(|o| o.text.clone())
        .chain(exam::retrieval_anchors(&exam))
        .collect::<Vec<_>>();
    let hits = retriever::keyword_search_with_anchors(
        &pool,
        "current",
        "以下哪些控制语句不支持 FETCH FETCHALL WHERE",
        &anchors,
        &exam.stem,
        5,
    )
    .await
    .unwrap();
    assert!(hits.iter().any(|r| r.chunk_id == "needed"));
    assert!(!hits.iter().any(|r| r.chunk_id == "foreign"));
    let candidates = hits
        .into_iter()
        .map(|result| ScoredSearchResult {
            keyword_score: Some(result.score),
            result,
            vector_score: None,
        })
        .collect();
    let ranked = retriever::rank_exam_candidates(candidates, &exam);
    assert_eq!(ranked[0].result.chunk_id, "needed");
    let (window, start) = retriever::evidence_window(&ranked[0].result.content, &anchors, 200);
    assert!(start > 200 && window.contains("不支持 FETCH"));
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
fn exact_keyword_boundaries_and_equal_rank_are_deterministic() {
    let exam = serde_json::from_value(serde_json::json!({"type":"single","polarity":"positive","stem":"控制语句", "options":[{"id":"A","text":"FETCH"},{"id":"B","text":"RETURN"}]})).unwrap();
    let result = |id: &str, content: &str| ScoredSearchResult {
        result: SearchResult {
            chunk_id: id.into(),
            doc_id: "d".into(),
            filename: "f".into(),
            content: content.into(),
            score: 1.0,
            metadata: serde_json::json!({}),
        },
        vector_score: None,
        keyword_score: Some(1.0),
    };
    let ranked = retriever::rank_exam_candidates(
        vec![
            result("prefix-only", "【强制】使用 FETCHALL 函数。"),
            result("rule-first", "【强制】不支持 FETCH 控制语句。"),
            result("rule-second", "【建议】使用 RETURN 控制语句。"),
        ],
        &exam,
    );
    assert_eq!(
        ranked
            .iter()
            .map(|c| c.result.chunk_id.as_str())
            .collect::<Vec<_>>(),
        ["rule-first", "rule-second", "prefix-only"]
    );
}

#[tokio::test]
async fn greedy_rule_coverage_keeps_new_options_ahead_of_duplicate_rules() {
    let exam: ExamQuestion = serde_json::from_value(serde_json::json!({"type":"multiple","polarity":"positive","stem":"控制语句", "options":[{"id":"A","text":"FETCH"},{"id":"B","text":"RETURN"},{"id":"C","text":"DELETE"}]})).unwrap();
    let result = |id: &str, content: &str| ScoredSearchResult {
        result: SearchResult {
            chunk_id: id.into(),
            doc_id: "d".into(),
            filename: "f".into(),
            content: content.into(),
            score: 1.0,
            metadata: serde_json::json!({}),
        },
        vector_score: None,
        keyword_score: Some(1.0),
    };
    let candidates = vec![
        result("a", "【强制】不支持 FETCH RETURN 控制语句。"),
        result("b", "【强制】不支持 FETCH RETURN 控制语句。"),
        result("c", "【建议】使用 DELETE 控制语句。"),
    ];
    let ranked = retriever::rank_exam_candidates_bounded(candidates.clone(), &exam)
        .await
        .unwrap();
    let mut reversed = candidates;
    reversed.reverse();
    let reversed_ranked = retriever::rank_exam_candidates(reversed, &exam);
    let ids = |ranked: &[ScoredSearchResult]| {
        ranked
            .iter()
            .map(|r| r.result.chunk_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&ranked), ["a", "c", "b"]);
    assert_eq!(ids(&ranked), ids(&reversed_ranked));
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

/// 手工离线参数对照。fixture 只含正文、题干与选项，禁止包含标准答案。
#[tokio::test]
#[ignore = "requires WALIAPI_EXAM_RETRIEVAL_FIXTURE exported from an authorized local snapshot"]
async fn offline_candidate_context_grid() {
    let path = std::env::var("WALIAPI_EXAM_RETRIEVAL_FIXTURE").unwrap();
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let pool = fixture().await;
    for row in input["chunks"].as_array().unwrap() {
        chunk(
            &pool,
            row["id"].as_str().unwrap(),
            "current",
            row["content"].as_str().unwrap(),
        )
        .await;
    }
    KbRepository::new(pool.clone())
        .backfill_search_text_for_kb("current")
        .await
        .unwrap();
    for row in input["questions"].as_array().unwrap() {
        let exam: ExamQuestion = serde_json::from_value(row["exam"].clone()).unwrap();
        let anchors: Vec<_> = exam
            .options
            .iter()
            .map(|o| o.text.clone())
            .chain(exam::retrieval_anchors(&exam))
            .collect();
        let query = format!(
            "{} {}",
            exam.stem,
            exam.options
                .iter()
                .map(|o| o.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for candidate_k in [5usize, 10, 20] {
            let started = std::time::Instant::now();
            let hits = retriever::keyword_search_with_anchors(
                &pool,
                "current",
                &query,
                &anchors,
                &exam.stem,
                candidate_k * 2,
            )
            .await
            .unwrap();
            let ranked = retriever::rank_exam_candidates_bounded(
                hits.into_iter()
                    .map(|result| ScoredSearchResult {
                        keyword_score: Some(result.score),
                        result,
                        vector_score: None,
                    })
                    .collect(),
                &exam,
            )
            .await
            .unwrap();
            let elapsed_us = started.elapsed().as_micros();
            for context_k in [4usize, 5, 8] {
                let context: Vec<_> = ranked.iter().take(candidate_k).take(context_k).collect();
                let section = row["required_section"].as_str().unwrap();
                println!(
                    "OFFLINE_GRID {}",
                    serde_json::json!({
                        "question_id":row["id"], "candidate_k":candidate_k,
                        "context_k":context_k, "selected":context.len(),
                        "rule_section_present":context.iter().any(|c|c.result.content.contains(section)),
                        "rule_section_in_window":context.iter().any(|c|retriever::evidence_window(&c.result.content,&anchors,200).0.contains(section)),
                        "chunk_ids":context.iter().map(|c|&c.result.chunk_id).collect::<Vec<_>>(),
                        "context_chars":context.iter().map(|c|c.result.content.chars().count()).sum::<usize>(),
                        "retrieval_us":elapsed_us
                    })
                );
            }
        }
    }
}
