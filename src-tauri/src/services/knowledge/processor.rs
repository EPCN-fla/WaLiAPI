use super::embedder;
use super::parser;
use super::splitter;
use super::repository::{KbRepository, ChunkInsert};
use super::retriever;
use crate::db::models::now_iso;
use crate::db::repository::Repository;
use sqlx::SqlitePool;
use tauri::AppHandle;

/// Default embedding model
const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// Process an uploaded document: parse → split → embed → store
pub async fn process_document(
    pool: &SqlitePool,
    app: &AppHandle,
    kb_id: &str,
    doc_id: &str,
    filename: &str,
    content: &[u8],
    embedding_model: Option<&str>,
) -> Result<(), String> {
    let repo = KbRepository::new(pool.clone());

    // Update status to processing
    repo.update_document_status(doc_id, "processing", None)
        .await
        .map_err(|e| e.to_string())?;

    let result = process_document_inner(
        pool, app, kb_id, doc_id, filename, content, embedding_model,
    ).await;

    if let Err(ref e) = result {
        let err_msg = format!("文档「{}」处理失败: {}", filename, e);
        let _ = repo.update_document_status(doc_id, "failed", Some(&err_msg)).await;
        // Emit error event to frontend
        use tauri::Emitter;
        let _ = app.emit("kb-document-error", serde_json::json!({
            "doc_id": doc_id,
            "kb_id": kb_id,
            "filename": filename,
            "error": e,
        }));
    }

    result
}

async fn process_document_inner(
    pool: &SqlitePool,
    _app: &AppHandle,
    kb_id: &str,
    doc_id: &str,
    filename: &str,
    content: &[u8],
    embedding_model: Option<&str>,
) -> Result<(), String> {
    let repo = KbRepository::new(pool.clone());

    // 1. Parse file
    let parsed = parser::parse_file(filename, content)?;

    let (text, file_type_label): (String, String) = match &parsed {
        parser::ParsedContent::PlainText(t) => (t.clone(), "text".to_string()),
        parser::ParsedContent::Markdown { text } => (text.clone(), "markdown".to_string()),
        parser::ParsedContent::Code { text, language } => (text.clone(), language.clone()),
        parser::ParsedContent::Structured(t) => (t.clone(), "structured".to_string()),
    };

    // 2. Split into chunks
    let config = splitter::SplitConfig::default();
    let base_metadata = splitter::ChunkMetadata {
        file_path: Some(filename.to_string()),
        ..Default::default()
    };

    let chunks = splitter::split(&text, &file_type_label, &config, &base_metadata);

    if chunks.is_empty() {
        repo.update_document_status(doc_id, "ready", None)
            .await
            .map_err(|e| e.to_string())?;
        repo.update_document_counts(doc_id, 0, 0)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    let total_chunks = chunks.len() as i64;
    let total_tokens: i64 = chunks.iter().map(|c| c.token_count as i64).sum();

    // 3. Embed chunks in batches
    let emb_model = embedding_model.unwrap_or(DEFAULT_EMBEDDING_MODEL);
    let main_repo = Repository::new(pool.clone());

    let batch_size = 32;
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());

    for batch in chunks.chunks(batch_size) {
        let texts: Vec<String> = batch.iter().map(|c| c.content.clone()).collect();
        let embeddings = embedder::embed(&texts, emb_model, &main_repo).await?;
        all_embeddings.extend(embeddings);
    }

    // 4. Store chunks with embeddings
    for (i, chunk) in chunks.iter().enumerate() {
        let embedding_bytes = retriever::encode_embedding(&all_embeddings[i]);
        let chunk_insert = ChunkInsert {
            id: uuid::Uuid::new_v4().to_string(),
            doc_id: doc_id.to_string(),
            kb_id: kb_id.to_string(),
            chunk_index: i as i64,
            content: chunk.content.clone(),
            token_count: chunk.token_count as i64,
            embedding: embedding_bytes,
            embedding_dim: all_embeddings[i].len() as i64,
            metadata: serde_json::to_string(&chunk.metadata).unwrap_or_else(|_| "{}".to_string()),
            created_at: now_iso(),
        };
        repo.create_chunk(&chunk_insert)
            .await
            .map_err(|e| e.to_string())?;
    }

    // 5. Update document and KB counts
    repo.update_document_counts(doc_id, total_chunks, total_tokens)
        .await
        .map_err(|e| e.to_string())?;
    repo.update_document_status(doc_id, "ready", None)
        .await
        .map_err(|e| e.to_string())?;
    repo.update_kb_counts(kb_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Reindex a document (delete old chunks, reprocess)
pub async fn reindex_document(
    pool: &SqlitePool,
    app: &AppHandle,
    doc_id: &str,
) -> Result<(), String> {
    let repo = KbRepository::new(pool.clone());
    let doc = repo.get_document(doc_id).await.map_err(|e| e.to_string())?;

    // Delete existing chunks
    repo.delete_chunks_by_doc(doc_id).await.map_err(|e| e.to_string())?;

    // Read file content from path
    let content = if let Some(path) = &doc.file_path {
        std::fs::read(path).map_err(|e| format!("Failed to read file: {}", e))?
    } else {
        return Err("No file path to reindex".to_string());
    };

    // Get KB for embedding model
    let kb = repo.get_kb(&doc.kb_id).await.map_err(|e| e.to_string())?;

    process_document(
        pool,
        app,
        &doc.kb_id,
        doc_id,
        &doc.filename,
        &content,
        kb.embedding_model.as_deref(),
    )
    .await
}
