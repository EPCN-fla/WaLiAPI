use super::models::*;
use super::project;
use super::ingest;
use super::repository::WikiRepository;
use crate::server::router::SharedState;
use crate::core::proxy;
use crate::db::repository::Repository;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize { 10 }

// ── Project handlers ──

pub async fn list_projects(State(shared): State<SharedState>) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.list_projects().await {
        Ok(projects) => Json(serde_json::json!({ "data": projects })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn create_project(
    State(shared): State<SharedState>,
    Json(input): Json<CreateProjectInput>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    let project_id = project::new_uuid();
    let schema = input.schema_text.clone().unwrap_or_else(|| {
        super::repository::DEFAULT_SCHEMA.to_string()
    });

    // Create directory structure
    if let Err(e) = project::init_project_dir(&project_id, &schema).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    let dir = project::project_wiki_dir(&project_id);
    let wiki_dir = dir.to_string_lossy().to_string();

    match repo.create_project(&input, &wiki_dir).await {
        Ok(p) => (StatusCode::CREATED, Json(p)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn get_project(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.get_project(&id).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}

pub async fn update_project(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateProjectInput>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());

    // If schema_text changed, write to disk
    if let Some(ref schema) = input.schema_text {
        let dir = project::project_wiki_dir(&id);
        let schema_path = dir.join("schema").join("CLAUDE.md");
        let _ = tokio::fs::write(schema_path, schema).await;
    }

    match repo.update_project(&id, &input).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn delete_project(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    if let Err(e) = repo.delete_project(&id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Err(e) = project::remove_project_dir(&id).await {
        tracing::warn!("Failed to remove project dir: {}", e);
    }
    (StatusCode::NO_CONTENT, "").into_response()
}

pub async fn get_project_stats(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.get_stats(&id).await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Source handlers ──

pub async fn list_sources(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.list_sources(&id).await {
        Ok(sources) => Json(serde_json::json!({ "data": sources })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn add_source(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Json(input): Json<AddSourceInput>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());

    let content_hash = input.content.as_ref().map(|c| {
        let mut hasher = Sha256::new();
        hasher.update(c.as_bytes());
        format!("{:x}", hasher.finalize())
    });

    let file_size = input.content.as_ref().map(|c| c.len() as i64).unwrap_or(0);

    // If content provided, write to disk
    if let Some(ref content) = input.content {
        if let Err(e) = project::write_source_file(&id, &input.filename, content.as_bytes()).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }

    match repo.add_source(&id, &input, content_hash.as_deref(), file_size).await {
        Ok(s) => (StatusCode::CREATED, Json(s)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn delete_source(
    State(shared): State<SharedState>,
    Path((_id, sid)): Path<(String, String)>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    if let Err(e) = repo.delete_source(&sid).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    (StatusCode::NO_CONTENT, "").into_response()
}

pub async fn ingest_source(
    State(shared): State<SharedState>,
    Path((id, sid)): Path<(String, String)>,
) -> Response {
    let app = shared.app.clone();
    let pool = shared.state.db.pool.clone();

    // Spawn ingest in background, return immediately with task info
    let project_id = id.clone();
    let source_id = sid.clone();

    match ingest::ingest_source(&app, &pool, &project_id, &source_id).await {
        Ok(result) => Json(serde_json::json!({
            "status": "done",
            "pages_created": result.pages_created,
            "page_paths": result.page_paths,
        })).into_response(),
        Err(e) => {
            // Update source status to failed
            let repo = WikiRepository::new(pool);
            let _ = repo.update_source_status(&source_id, "failed", 0, Some(&e)).await;
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

pub async fn rescan_sources(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let app = shared.app.clone();
    let pool = shared.state.db.pool.clone();
    let repo = WikiRepository::new(pool.clone());

    // Get all pending sources and ingest them
    let sources = match repo.list_sources(&id).await {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let pending: Vec<_> = sources.iter().filter(|s| s.status == "pending").collect();
    let mut results = Vec::new();

    for source in &pending {
        match ingest::ingest_source(&app, &pool, &id, &source.id).await {
            Ok(r) => results.push(serde_json::json!({
                "source_id": source.id,
                "filename": source.filename,
                "status": "done",
                "pages": r.pages_created,
            })),
            Err(e) => results.push(serde_json::json!({
                "source_id": source.id,
                "filename": source.filename,
                "status": "failed",
                "error": e,
            })),
        }
    }

    Json(serde_json::json!({
        "status": "done",
        "processed": pending.len(),
        "results": results,
    })).into_response()
}

// ── Page handlers ──

pub async fn list_pages(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.list_pages(&id).await {
        Ok(pages) => Json(serde_json::json!({ "data": pages })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn get_page(
    State(shared): State<SharedState>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());

    // Try DB first
    if let Ok(Some(page)) = repo.get_page(&id, &path).await {
        // Also read file content
        if let Ok(content) = project::read_page(&id, &path).await {
            return Json(serde_json::json!({
                "id": page.id,
                "project_id": page.project_id,
                "path": page.path,
                "title": page.title,
                "page_type": page.page_type,
                "content_hash": page.content_hash,
                "token_count": page.token_count,
                "wikilinks": page.wikilinks,
                "frontmatter": page.frontmatter,
                "status": page.status,
                "content": content,
                "created_at": page.created_at,
                "updated_at": page.updated_at,
            })).into_response();
        }
    }

    // Try reading file directly from disk
    match project::read_page(&id, &path).await {
        Ok(content) => {
            let title = path.split('/').last().unwrap_or(&path)
                .trim_end_matches(".md").to_string();
            Json(serde_json::json!({
                "path": path,
                "title": title,
                "content": content,
                "page_type": "unknown",
            })).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}

pub async fn update_page(
    State(shared): State<SharedState>,
    Path((id, path)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let content = body.get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");

    if let Err(e) = project::write_page(&id, &path, content).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    let repo = WikiRepository::new(shared.state.db.pool.clone());
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let title = path.split('/').last().unwrap_or(&path)
        .trim_end_matches(".md").to_string();
    let page_type = if path.contains("entities/") { "entity" }
        else if path.contains("concepts/") { "concept" }
        else if path.contains("summaries/") { "summary" }
        else if path.ends_with("index.md") { "index" }
        else if path.ends_with("log.md") { "log" }
        else { "entity" };

    let token_count = (content.len() / 4) as i64; // rough estimate

    // Extract wikilinks
    let wikilinks: Vec<String> = extract_wikilinks(content);
    let wikilinks_json = serde_json::to_string(&wikilinks).unwrap_or("[]".to_string());

    if let Err(e) = repo.upsert_page(&id, &path, &title, page_type, &hash, token_count, &wikilinks_json, "{}").await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    // Append log
    let _ = project::append_log(&id, &format!("update | {}", path)).await;

    Json(serde_json::json!({ "ok": true, "path": path })).into_response()
}

pub async fn delete_page(
    State(shared): State<SharedState>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    if let Err(e) = repo.delete_page(&id, &path).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let _ = project::delete_page_file(&id, &path).await;
    (StatusCode::NO_CONTENT, "").into_response()
}

// ── Search & Ask ──

pub async fn search(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Query(params): Query<SearchQuery>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.search_pages(&id, &params.q, params.top_k).await {
        Ok(results) => Json(serde_json::json!({ "data": results, "query": params.q })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn ask(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Json(input): Json<WikiAskInput>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    let db_repo = Arc::new(Repository::new(shared.state.db.pool.clone()));
    let app = shared.app.clone();

    // Search relevant pages
    let top_k = input.top_k.unwrap_or(5);
    let results = repo.search_pages(&id, &input.question, top_k).await.unwrap_or_default();

    // Read page contents
    let mut contexts = Vec::new();
    for r in &results {
        if let Ok(content) = project::read_page(&id, &r.path).await {
            let snippet: String = content.chars().take(2000).collect();
            contexts.push(format!("## {} ({})\n{}", r.title, r.path, snippet));
        }
    }

    if contexts.is_empty() {
        return Json(serde_json::json!({
            "answer": "No relevant wiki pages found for your question. Please ingest some documents first.",
            "sources": []
        })).into_response();
    }

    let context_text = contexts.join("\n\n---\n\n");

    // Get project config
    let proj = match repo.get_project(&id).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let chat_model = input.model.as_deref()
        .or(proj.chat_model.as_deref())
        .unwrap_or("gpt-4o");
    let chat_channel_id = match proj.chat_channel_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            let row: Option<(String,)> = match sqlx::query_as("SELECT id FROM channels WHERE status = 1 ORDER BY priority DESC LIMIT 1")
                .fetch_optional(&shared.state.db.pool).await {
                Ok(r) => r,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
            };
            match row.map(|(id,)| id) {
                Some(id) => id,
                None => return (StatusCode::INTERNAL_SERVER_ERROR, "No active channel configured. Please create a channel first or set chat_channel_id in Wiki project settings.".to_string()).into_response(),
            }
        }
    };

    // Build history
    let history_text = input.history.as_ref().map(|h| {
        h.iter().map(|m| format!("{}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n")
    }).unwrap_or_default();

    let system_prompt = "You are a Wiki knowledge assistant. Answer questions based on the provided wiki pages. Be concise and cite source pages using [[wikilinks]] format.";
    let user_prompt = format!(
        "Based on the following wiki pages, answer the question.\n\nWiki pages:\n{}\n\n{}\nQuestion: {}\n\nAnswer:",
        context_text,
        if history_text.is_empty() { String::new() } else { format!("Conversation history:\n{}\n\n", history_text) },
        input.question
    );

    // Save user message
    let _ = repo.add_session(&id, "user", &input.question, None, None).await;

    let chat_request = serde_json::json!({
        "model": chat_model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt}
        ],
        "stream": false,
        "temperature": 0.4
    });
    let chat_request_str: String = serde_json::to_string(&chat_request).unwrap_or_default();

    let proxy_result = proxy::handle_request(
        &db_repo,
        &app,
        &chat_channel_id,
        "Wiki Chat",
        chat_request,
        false,
        Some(chat_request_str),
        Some(format!("wiki-chat_{}", id)),
        None,
    ).await;

    let (answer, usage) = match proxy_result {
        Ok(result) => {
            let answer_text = result.body
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|t| t.as_str())
                .unwrap_or("Failed to generate answer.");

            let usage = result.body.get("usage").map(|u| serde_json::json!({
                "prompt_tokens": u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                "completion_tokens": u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                "total_tokens": u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            }));

            (answer_text.to_string(), usage)
        }
        Err((code, msg)) => {
            let err_answer = format!("LLM request failed ({}): {}", code, msg);
            (err_answer, None)
        }
    };

    let sources: Vec<WikiAnswerSource> = results.iter().map(|r| WikiAnswerSource {
        path: r.path.clone(),
        title: r.title.clone(),
        score: r.score,
        snippet: r.snippet.clone(),
    }).collect();

    // Save assistant message
    let _ = repo.add_session(&id, "assistant", &answer, Some(&serde_json::to_string(&sources).unwrap_or_default()), Some(chat_model)).await;

    Json(serde_json::json!({
        "answer": answer,
        "sources": sources,
        "usage": usage,
    })).into_response()
}

// ── Graph ──

pub async fn get_graph(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.get_graph(&id).await {
        Ok(graph) => Json(graph).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Reviews ──

pub async fn list_reviews(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    let resolved = params.get("resolved").and_then(|v| match v.as_str() {
        "false" | "0" => Some(false),
        "true" | "1" => Some(true),
        _ => None,
    });
    match repo.list_reviews(&id, resolved).await {
        Ok(reviews) => Json(serde_json::json!({ "data": reviews })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn resolve_review(
    State(shared): State<SharedState>,
    Path((_id, rid)): Path<(String, String)>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    if let Err(e) = repo.resolve_review(&rid).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ── Lint ──

pub async fn run_lint(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    // TODO: Phase 3 - implement full lint engine
    // For now, check for orphan pages (no incoming wikilinks)
    match repo.list_pages(&id).await {
        Ok(pages) => {
            let mut all_linked: std::collections::HashSet<String> = std::collections::HashSet::new();
            for p in &pages {
                let links: Vec<String> = serde_json::from_str(&p.wikilinks).unwrap_or_default();
                for l in links {
                    all_linked.insert(l);
                }
            }
            let orphans: Vec<&WikiPage> = pages.iter()
                .filter(|p| !all_linked.contains(&p.path) && p.page_type != "index" && p.page_type != "log")
                .collect();

            Json(serde_json::json!({
                "orphan_count": orphans.len(),
                "total_pages": pages.len(),
                "orphans": orphans.iter().map(|p| serde_json::json!({
                    "path": p.path,
                    "title": p.title,
                })).collect::<Vec<_>>(),
                "note": "Full lint engine (contradiction, missing pages, stale) will be implemented in Phase 3."
            })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Deep Research ──

pub async fn deep_research(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let topic = body.get("topic").and_then(|t| t.as_str()).unwrap_or("");
    let repo = WikiRepository::new(shared.state.db.pool.clone());

    match repo.create_task(&id, None, "deep_research").await {
        Ok(task_id) => {
            Json(serde_json::json!({
                "task_id": task_id,
                "status": "pending",
                "topic": topic,
                "message": "Deep research task queued. Will be implemented in Phase 5."
            })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Sessions ──

pub async fn list_sessions(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.list_sessions(&id).await {
        Ok(sessions) => Json(serde_json::json!({ "data": sessions })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn clear_sessions(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    if let Err(e) = repo.clear_sessions(&id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    (StatusCode::NO_CONTENT, "").into_response()
}

// ── Queue ──

pub async fn get_queue_status(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = WikiRepository::new(shared.state.db.pool.clone());
    match repo.list_tasks(&id).await {
        Ok(tasks) => Json(serde_json::json!({ "data": tasks })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Helpers ──

fn extract_wikilinks(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut start = 0;
    loop {
        if let Some(s) = content[start..].find("[[") {
            let s = start + s + 2;
            if let Some(e) = content[s..].find("]]") {
                let link = &content[s..s + e];
                if !link.is_empty() {
                    links.push(link.to_string());
                }
                start = s + e + 2;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    links
}
