use tauri::State;
use std::sync::Arc;
use crate::AppState;
use crate::db::repository::Repository;
use crate::services::knowledge::{repository::KbRepository, models::*};
use serde::Deserialize;

#[tauri::command]
pub async fn get_knowledge_bases(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<KbKnowledgeBase>, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.get_all_kbs().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn create_knowledge_base(
    state: State<'_, Arc<AppState>>,
    input: CreateKbInput,
) -> Result<KbKnowledgeBase, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.create_kb(&input).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_knowledge_base(
    state: State<'_, Arc<AppState>>,
    id: String,
    input: UpdateKbInput,
) -> Result<KbKnowledgeBase, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.update_kb(&id, &input).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_knowledge_base(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.delete_kb(&id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_kb_documents(
    state: State<'_, Arc<AppState>>,
    kb_id: String,
) -> Result<Vec<KbDocument>, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.get_documents(&kb_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_kb_document(
    state: State<'_, Arc<AppState>>,
    doc_id: String,
    kb_id: String,
) -> Result<(), String> {
    let repo = KbRepository::new(state.db.pool.clone());
    // Get doc to find file path
    if let Ok(doc) = repo.get_document(&doc_id).await {
        if let Some(path) = &doc.file_path {
            std::fs::remove_file(path).ok();
        }
    }
    repo.delete_document(&doc_id).await.map_err(|e| e.to_string())?;
    repo.update_kb_counts(&kb_id).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn reindex_kb_document(
    state: State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    doc_id: String,
) -> Result<(), String> {
    let pool = state.db.pool.clone();
    crate::services::knowledge::processor::reindex_document(&pool, &app, &doc_id)
        .await
        .map_err(|e| e)
}

#[derive(Debug, Deserialize)]
pub struct KbSearchInput {
    pub query: String,
    pub kb_id: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize { 5 }

#[tauri::command]
pub async fn search_knowledge_base(
    state: State<'_, Arc<AppState>>,
    input: KbSearchInput,
) -> Result<Vec<SearchResult>, String> {
    let pool = &state.db.pool;
    let repo = Repository::new(pool.clone());

    // Get embedding model
    let emb_model = if let Some(kb_id) = &input.kb_id {
        let kb_repo = KbRepository::new(pool.clone());
        kb_repo.get_kb(kb_id).await.ok()
            .and_then(|kb| kb.embedding_model)
            .unwrap_or_else(|| "text-embedding-3-small".to_string())
    } else {
        "text-embedding-3-small".to_string()
    };

    let embeddings = crate::services::knowledge::embedder::embed(
        &[input.query.clone()], &emb_model, &repo
    ).await.map_err(|e| e)?;

    if embeddings.is_empty() {
        return Err("Failed to embed query".to_string());
    }

    let results = if let Some(kb_id) = &input.kb_id {
        crate::services::knowledge::retriever::search(pool, kb_id, &embeddings[0], input.top_k).await
    } else {
        crate::services::knowledge::retriever::search_all(pool, &embeddings[0], input.top_k, false).await
    };

    results.map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct KbAskInput {
    pub question: String,
    pub kb_id: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default = "default_chat_model")]
    pub model: String,
}

fn default_chat_model() -> String { "gpt-4o".to_string() }

#[tauri::command]
pub async fn ask_knowledge_base(
    state: State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    input: KbAskInput,
) -> Result<RagAnswer, String> {
    let pool = &state.db.pool;
    let kb_id = input.kb_id.unwrap_or_default();

    let emb_model = if !kb_id.is_empty() {
        let kb_repo = KbRepository::new(pool.clone());
        kb_repo.get_kb(&kb_id).await.ok()
            .and_then(|kb| kb.embedding_model)
            .unwrap_or_else(|| "text-embedding-3-small".to_string())
    } else {
        "text-embedding-3-small".to_string()
    };

    crate::services::knowledge::rag::ask(
        pool, &kb_id, &input.question, &emb_model, &input.model, input.top_k, false, &app
    ).await.map_err(|e| e)
}

#[tauri::command]
pub async fn get_kb_stats(
    state: State<'_, Arc<AppState>>,
    kb_id: String,
) -> Result<serde_json::Value, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    let kb = repo.get_kb(&kb_id).await.map_err(|e| e.to_string())?;
    let docs = repo.get_documents(&kb_id).await.unwrap_or_default();
    let ready = docs.iter().filter(|d| d.status == "ready").count();
    let processing = docs.iter().filter(|d| d.status == "processing").count();
    let failed = docs.iter().filter(|d| d.status == "failed").count();

    Ok(serde_json::json!({
        "kb": kb,
        "documents": {
            "total": docs.len(),
            "ready": ready,
            "processing": processing,
            "failed": failed,
        }
    }))
}

#[derive(Debug, Deserialize)]
pub struct UploadDocInput {
    pub kb_id: String,
    pub filename: String,
    pub content: String, // base64 encoded
}

#[tauri::command]
pub async fn upload_kb_document(
    state: State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    input: UploadDocInput,
) -> Result<KbDocument, String> {
    use sha2::Digest;
    use tauri::Manager;

    let pool = &state.db.pool;
    let repo = KbRepository::new(pool.clone());

    // Decode base64
    let content = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD, &input.content
    ).map_err(|e| format!("Invalid base64: {}", e))?;

    // Hash
    let hash = sha2::Sha256::digest(&content);
    let hash_hex = hex::encode(hash);

    // Check duplicate
    if let Ok(Some(_)) = repo.find_document_by_hash(&input.kb_id, &hash_hex).await {
        return Err("Document with same content already exists".to_string());
    }

    let file_type = crate::services::knowledge::parser::get_file_type(&input.filename);
    let file_size = content.len() as i64;

    // Save file
    let app_data_dir = app.path().app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let kb_dir = app_data_dir.join("kb_files").join(&input.kb_id);
    std::fs::create_dir_all(&kb_dir).ok();
    let doc_id = uuid::Uuid::new_v4().to_string();
    let file_path = kb_dir.join(format!("{}_{}", &doc_id, &input.filename));
    std::fs::write(&file_path, &content).ok();
    let file_path_str = file_path.to_string_lossy().to_string();

    // Create doc record
    let doc = repo.create_document(
        &input.kb_id, &input.filename, Some(&file_path_str),
        &file_type, file_size, &hash_hex
    ).await.map_err(|e| e.to_string())?;

    // Get KB embedding model
    let kb = repo.get_kb(&input.kb_id).await.map_err(|e| e.to_string())?;
    let emb_model = kb.embedding_model.clone();

    // Spawn processing task
    let pool_clone = pool.clone();
    let app_clone = app.clone();
    let doc_id_clone = doc.id.clone();
    let filename_clone = input.filename.clone();

    tokio::spawn(async move {
        if let Err(e) = crate::services::knowledge::processor::process_document(
            &pool_clone, &app_clone, &input.kb_id, &doc_id_clone,
            &filename_clone, &content, emb_model.as_deref()
        ).await {
            tracing::error!("Document processing failed: {}", e);
        }
    });

    Ok(doc)
}
