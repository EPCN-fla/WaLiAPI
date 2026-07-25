use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Json, IntoResponse, Response},
};
use serde::Deserialize;
use crate::server::router::SharedState;
use crate::db::repository::Repository;
use tauri::Manager;
use sha2::Digest;
use super::models::*;
use super::repository::KbRepository;
use super::processor;
use super::rag;
use super::embedder;
use super::retriever;

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ─── Knowledge Base CRUD ──────────────────────────────────────────

pub async fn list_knowledge_bases(
    State(shared): State<SharedState>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.get_all_kbs().await {
        Ok(kbs) => Json(serde_json::json!({ "data": kbs })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    }
}

pub async fn create_knowledge_base(
    State(shared): State<SharedState>,
    Json(input): Json<CreateKbInput>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.create_kb(&input).await {
        Ok(kb) => (StatusCode::CREATED, Json(kb)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    }
}

pub async fn get_knowledge_base(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.get_kb(&id).await {
        Ok(kb) => Json(kb).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Knowledge base not found").into_response(),
    }
}

pub async fn update_knowledge_base(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateKbInput>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.update_kb(&id, &input).await {
        Ok(kb) => Json(kb).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    }
}

pub async fn delete_knowledge_base(
    State(shared): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.delete_kb(&id).await {
        Ok(_) => (StatusCode::NO_CONTENT, "").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    }
}

// ─── Document Management ──────────────────────────────────────────

pub async fn list_documents(
    State(shared): State<SharedState>,
    Path(kb_id): Path<String>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.get_documents(&kb_id).await {
        Ok(docs) => Json(serde_json::json!({ "data": docs })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    }
}

pub async fn upload_document(
    State(shared): State<SharedState>,
    Path(kb_id): Path<String>,
    Json(input): Json<UploadDocInput>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());

    // Decode base64 content
    let content = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &input.content) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)).into_response(),
    };

    // Compute SHA-256 hash
    let hash = sha2::Sha256::digest(&content);
    let hash_hex = hex::encode(hash);

    // Check for duplicate
    if let Ok(Some(_)) = repo.find_document_by_hash(&kb_id, &hash_hex).await {
        return (StatusCode::CONFLICT, "Document with same content already exists").into_response();
    }

    let file_type = super::parser::get_file_type(&input.filename);
    let file_size = content.len() as i64;

    // Save file to app data dir
    let app_data_dir = shared.app.path().app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let kb_dir = app_data_dir.join("kb_files").join(&kb_id);
    std::fs::create_dir_all(&kb_dir).ok();
    let doc_id = uuid::Uuid::new_v4().to_string();
    let file_path = kb_dir.join(format!("{}_{}", &doc_id, &input.filename));
    std::fs::write(&file_path, &content).ok();
    let file_path_str = file_path.to_string_lossy().to_string();

    // Create document record
    let doc = match repo.create_document(
        &kb_id,
        &input.filename,
        Some(&file_path_str),
        &file_type,
        file_size,
        &hash_hex,
    ).await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    };

    // Get KB embedding model
    let kb = match repo.get_kb(&kb_id).await {
        Ok(k) => k,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("KB not found: {}", e)).into_response(),
    };

    // Process document asynchronously
    let pool = shared.state.db.pool.clone();
    let app = shared.app.clone();
    let doc_id_clone = doc.id.clone();
    let filename_clone = input.filename.clone();
    let emb_model = kb.embedding_model.clone();

    tokio::spawn(async move {
        if let Err(e) = processor::process_document(
            &pool,
            &app,
            &kb_id,
            &doc_id_clone,
            &filename_clone,
            &content,
            emb_model.as_deref(),
        ).await {
            tracing::error!("Document processing failed: {}", e);
        }
    });

    Json(doc).into_response()
}

pub async fn get_document(
    State(shared): State<SharedState>,
    Path((_kb_id, doc_id)): Path<(String, String)>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());
    match repo.get_document(&doc_id).await {
        Ok(doc) => Json(doc).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Document not found").into_response(),
    }
}

pub async fn delete_document(
    State(shared): State<SharedState>,
    Path((kb_id, doc_id)): Path<(String, String)>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());

    // Get document to find file path
    if let Ok(doc) = repo.get_document(&doc_id).await {
        if let Some(path) = &doc.file_path {
            std::fs::remove_file(path).ok();
        }
    }

    match repo.delete_document(&doc_id).await {
        Ok(_) => {
            repo.update_kb_counts(&kb_id).await.ok();
            (StatusCode::NO_CONTENT, "").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    }
}

pub async fn reindex_document(
    State(shared): State<SharedState>,
    Path((_kb_id, doc_id)): Path<(String, String)>,
) -> Response {
    let pool = shared.state.db.pool.clone();
    let app = shared.app.clone();

    // Spawn reindex task
    tokio::spawn(async move {
        if let Err(e) = processor::reindex_document(&pool, &app, &doc_id).await {
            tracing::error!("Reindex failed: {}", e);
        }
    });

    Json(serde_json::json!({ "message": "Reindex started" })).into_response()
}

// ─── Search ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub kb_id: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize { 5 }

pub async fn search(
    State(shared): State<SharedState>,
    Query(query): Query<SearchQuery>,
) -> Response {
    let repo = Repository::new(shared.state.db.pool.clone());

    // Get embedding model from KB or use default
    let emb_model = if let Some(kb_id) = &query.kb_id {
        let kb_repo = KbRepository::new(shared.state.db.pool.clone());
        kb_repo.get_kb(kb_id).await
            .ok()
            .and_then(|kb| kb.embedding_model)
            .unwrap_or_else(|| "text-embedding-3-small".to_string())
    } else {
        "text-embedding-3-small".to_string()
    };

    // Embed query
    let embeddings = match embedder::embed(&[query.q.clone()], &emb_model, &repo).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Embedding failed: {}", e)).into_response(),
    };

    if embeddings.is_empty() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to embed query").into_response();
    }

    let query_emb = &embeddings[0];

    // Search
    let results = if let Some(kb_id) = &query.kb_id {
        retriever::search(&shared.state.db.pool, kb_id, query_emb, query.top_k).await
    } else {
        retriever::search_all(&shared.state.db.pool, query_emb, query.top_k, false).await
    };

    match results {
        Ok(results) => Json(serde_json::json!({ "data": results })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Search failed: {}", e)).into_response(),
    }
}

// ─── RAG Ask ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AskInput {
    pub question: String,
    pub kb_id: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default = "default_chat_model")]
    pub model: String,
}

fn default_chat_model() -> String { "gpt-4o".to_string() }

pub async fn ask(
    State(shared): State<SharedState>,
    Json(input): Json<AskInput>,
) -> Response {
    let kb_id = input.kb_id.unwrap_or_default();

    // Get embedding model
    let emb_model = if !kb_id.is_empty() {
        let kb_repo = KbRepository::new(shared.state.db.pool.clone());
        kb_repo.get_kb(&kb_id).await
            .ok()
            .and_then(|kb| kb.embedding_model)
            .unwrap_or_else(|| "text-embedding-3-small".to_string())
    } else {
        "text-embedding-3-small".to_string()
    };

    match rag::ask(
        &shared.state.db.pool,
        &kb_id,
        &input.question,
        &emb_model,
        &input.model,
        input.top_k,
        false,
        &shared.app,
    ).await {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("RAG failed: {}", e)).into_response(),
    }
}

// ─── Stats ────────────────────────────────────────────────────────

pub async fn kb_stats(
    State(shared): State<SharedState>,
    Path(kb_id): Path<String>,
) -> Response {
    let repo = KbRepository::new(shared.state.db.pool.clone());

    let kb = match repo.get_kb(&kb_id).await {
        Ok(k) => k,
        Err(_) => return (StatusCode::NOT_FOUND, "KB not found").into_response(),
    };

    let docs = repo.get_documents(&kb_id).await.unwrap_or_default();
    let ready_count = docs.iter().filter(|d| d.status == "ready").count();
    let processing_count = docs.iter().filter(|d| d.status == "processing").count();
    let failed_count = docs.iter().filter(|d| d.status == "failed").count();
    let pending_count = docs.iter().filter(|d| d.status == "pending").count();

    Json(serde_json::json!({
        "kb": kb,
        "documents": {
            "total": docs.len(),
            "ready": ready_count,
            "processing": processing_count,
            "failed": failed_count,
            "pending": pending_count,
        },
    })).into_response()
}
