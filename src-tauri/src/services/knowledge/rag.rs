use super::embedder;
use super::retriever;
use super::models::{RagAnswer, SourceInfo, UsageInfo};
use crate::core::proxy;
use crate::db::repository::Repository;
use tauri::AppHandle;
use std::sync::Arc;
use sqlx::SqlitePool;

/// RAG: Retrieve relevant chunks, then generate answer via WaLiAPI proxy
pub async fn ask(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,
    embedding_model: &str,
    chat_model: &str,
    top_k: usize,
    mcp_only: bool,
    app: &AppHandle,
) -> Result<RagAnswer, String> {
    let repo = Repository::new(pool.clone());

    // 1. Embed the query
    let embeddings = embedder::embed(&[query.to_string()], embedding_model, &repo)
        .await
        .map_err(|e| format!("Embedding failed: {}", e))?;

    if embeddings.is_empty() {
        return Err("Failed to embed query".to_string());
    }

    let query_emb = &embeddings[0];

    // 2. Vector search
    let results = if kb_id.is_empty() {
        retriever::search_all(pool, query_emb, top_k, mcp_only).await?
    } else {
        retriever::search(pool, kb_id, query_emb, top_k).await?
    };

    if results.is_empty() {
        return Ok(RagAnswer {
            answer: "知识库中没有找到相关内容。".to_string(),
            sources: vec![],
            usage: None,
        });
    }

    // 3. Build context
    let context = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "--- 文档 {} [{}] (相似度: {:.2}) ---\n{}",
                i + 1,
                r.filename,
                r.score,
                r.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    // 4. Build RAG prompt
    let prompt = format!(
        r#"基于以下知识库内容回答问题。如果知识库中没有相关信息，请明确说明。

<knowledge_base>
{}
</knowledge_base>

问题: {}
"#,
        context, query
    );

    // 5. Call LLM via proxy (internal, no API key needed)
    let chat_request = serde_json::json!({
        "model": chat_model,
        "messages": [
            {"role": "system", "content": "你是知识库助手。基于检索到的知识库内容回答问题。回答要准确、简洁，并标注信息来源。"},
            {"role": "user", "content": prompt}
        ],
        "stream": false
    });

    let proxy_result = proxy::handle_request(
        &Arc::new(repo),
        app,
        "kb-internal",
        "知识库RAG",
        chat_request,
        false,
        None,
        None,
    )
    .await;

    match proxy_result {
        Ok(result) => {
            let answer = result
                .body
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("生成回答失败")
                .to_string();

            let usage = result.usage.map(|u| UsageInfo {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            });

            let sources = results
                .iter()
                .map(|r| SourceInfo {
                    filename: r.filename.clone(),
                    score: r.score,
                    snippet: r.content.chars().take(200).collect(),
                })
                .collect();

            Ok(RagAnswer {
                answer,
                sources,
                usage,
            })
        }
        Err((code, msg)) => Err(format!("LLM request failed ({}): {}", code, msg)),
    }
}
