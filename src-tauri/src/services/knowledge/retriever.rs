use super::models::SearchResult;
use super::repository::KbRepository;
use super::index::HnswIndex;
use sqlx::SqlitePool;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};

/// Default HNSW parameters
const DEFAULT_M: usize = 16;
const DEFAULT_EF_CONSTRUCTION: usize = 200;
const DEFAULT_EF_SEARCH: usize = 50;

/// Get the index file path for a KB.
fn index_path(kb_id: &str) -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("./data"))
        .join("waliapi")
        .join("hnsw_indexes");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("kb_{}.hnsw", kb_id))
}

/// Try to load the HNSW index for a KB. Returns None if not built.
fn load_index(kb_id: &str) -> Option<HnswIndex> {
    let path = index_path(kb_id);
    if path.exists() {
        match HnswIndex::load(&path) {
            Ok(index) if index.initialized && !index.is_empty() => Some(index),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("Failed to load HNSW index for KB {}: {}", kb_id, e);
                None
            }
        }
    } else {
        None
    }
}

/// Search knowledge base by query embedding.
/// Uses HNSW index if available, falls back to linear scan.
pub async fn search(
    pool: &SqlitePool,
    kb_id: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());

    // Try HNSW index first
    if let Some(index) = load_index(kb_id) {
        if index.dim == query_embedding.len() {
            tracing::debug!("Using HNSW index for KB {} ({} nodes)", kb_id, index.len());
            let hnsw_results = index.search(query_embedding, top_k);

            if !hnsw_results.is_empty() {
                // Fetch chunk data from DB by external IDs
                let mut results = Vec::with_capacity(hnsw_results.len());
                for r in hnsw_results {
                    // The external ID is the chunk's position in the build set.
                    // We need to map it back to the actual chunk_id.
                    // The index stores external_id = sequential position, so we
                    // need to load chunks and map by position.
                    // Actually, the index stores the chunk's sequential position as ID.
                    // Let's load all chunks and index by position.
                    // For efficiency, we'll batch-load.
                    // But since we already have the results, let's load chunks once.
                    // This is handled below in the fallback-style mapping.
                    results.push((r.id, r.score));
                }

                // Load chunks for mapping
                let chunks = repo
                    .get_chunks_by_kb(kb_id)
                    .await
                    .map_err(|e| format!("Failed to load chunks: {}", e))?;

                // Map position -> chunk data
                let mapped: Vec<SearchResult> = results
                    .iter()
                    .filter_map(|(pos, score)| {
                        let idx = *pos;
                        if idx < chunks.len() {
                            let (id, content, metadata, _emb, filename, doc_id) = &chunks[idx];
                            let meta: serde_json::Value =
                                serde_json::from_str(metadata).unwrap_or(serde_json::json!({}));
                            Some(SearchResult {
                                chunk_id: id.clone(),
                                doc_id: doc_id.clone(),
                                filename: filename.clone(),
                                content: content.clone(),
                                score: *score,
                                metadata: meta,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                if !mapped.is_empty() {
                    return Ok(mapped);
                }

                tracing::warn!("HNSW index returned results but mapping failed, falling back to linear scan");
            }
        } else {
            tracing::warn!(
                "HNSW index dim ({}) != query dim ({}) for KB {}, falling back to linear scan",
                index.dim, query_embedding.len(), kb_id
            );
        }
    }

    // Fallback: linear scan
    linear_search(pool, kb_id, query_embedding, top_k).await
}

/// Linear scan search (original implementation).
async fn linear_search(
    pool: &SqlitePool,
    kb_id: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());

    let chunks = repo
        .get_chunks_by_kb(kb_id)
        .await
        .map_err(|e| format!("Failed to load chunks: {}", e))?;

    if chunks.is_empty() {
        return Ok(vec![]);
    }

    let query_dim = query_embedding.len();

    let mut scored: Vec<(f32, usize)> = Vec::with_capacity(chunks.len());
    let mut dim_mismatches = 0;

    for (i, (_, _, _, emb, _, _)) in chunks.iter().enumerate() {
        let vector = decode_embedding(emb);
        if vector.len() != query_dim {
            dim_mismatches += 1;
            continue;
        }
        let score = cosine_similarity(query_embedding, &vector);
        scored.push((score, i));
    }

    if dim_mismatches > 0 {
        tracing::warn!(
            "Skipped {} chunks with mismatched embedding dimensions (expected {}) in KB {}",
            dim_mismatches, query_dim, kb_id
        );
    }

    if scored.is_empty() {
        return Ok(vec![]);
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

    let results = scored
        .into_iter()
        .filter_map(|(score, i)| {
            let (id, content, metadata, _emb, filename, doc_id) = &chunks[i];
            let meta: serde_json::Value = serde_json::from_str(metadata).unwrap_or(serde_json::json!({}));
            Some(SearchResult {
                chunk_id: id.clone(),
                doc_id: doc_id.clone(),
                filename: filename.clone(),
                content: content.clone(),
                score,
                metadata: meta,
            })
        })
        .collect();

    Ok(results)
}

/// Search across all knowledge bases.
/// If mcp_only is true, only search KBs with mcp_enabled = 1.
pub async fn search_all(
    pool: &SqlitePool,
    query_embedding: &[f32],
    top_k: usize,
    mcp_only: bool,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());

    let kbs = repo
        .get_all_kbs()
        .await
        .map_err(|e| format!("Failed to get KBs: {}", e))?;

    let active_kbs: Vec<_> = kbs
        .iter()
        .filter(|kb| kb.status == 1 && (!mcp_only || kb.mcp_enabled == 1))
        .collect();

    if active_kbs.is_empty() {
        return Ok(vec![]);
    }

    let mut all_results = Vec::new();
    for kb in &active_kbs {
        if let Ok(results) = search(pool, &kb.id, query_embedding, top_k).await {
            all_results.extend(results);
        }
    }

    all_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all_results.truncate(top_k);

    Ok(all_results)
}

/// Get the embedding dimension for a KB by checking the first valid chunk.
pub async fn detect_embedding_dim(pool: &SqlitePool, kb_id: &str) -> Result<Option<usize>, String> {
    let repo = KbRepository::new(pool.clone());
    let chunks = repo
        .get_chunks_by_kb(kb_id)
        .await
        .map_err(|e| format!("Failed to load chunks: {}", e))?;

    for (_, _, _, emb, _, _) in &chunks {
        let vector = decode_embedding(emb);
        if !vector.is_empty() {
            return Ok(Some(vector.len()));
        }
    }

    Ok(None)
}

// ════════════════════════════════════════════════════════
// Index management
// ════════════════════════════════════════════════════════

/// Build HNSW index for a KB from all its chunks.
/// Emits `kb-index-progress` Tauri events with percentage.
pub async fn build_index(pool: &SqlitePool, kb_id: &str, app: &AppHandle) -> Result<(), String> {
    let repo = KbRepository::new(pool.clone());

    let chunks = repo
        .get_chunks_by_kb(kb_id)
        .await
        .map_err(|e| format!("Failed to load chunks: {}", e))?;

    if chunks.is_empty() {
        return Err("No chunks to index".to_string());
    }

    // Build (position, vector) pairs
    let mut items: Vec<(usize, Vec<f32>)> = Vec::with_capacity(chunks.len());
    let mut dim = 0;

    for (i, (_, _, _, emb, _, _)) in chunks.iter().enumerate() {
        let vector = decode_embedding(emb);
        if !vector.is_empty() {
            if dim == 0 {
                dim = vector.len();
            }
            if vector.len() == dim {
                items.push((i, vector));
            }
        }
    }

    if items.is_empty() {
        return Err("No valid embeddings found".to_string());
    }

    // Create and build the index on blocking thread pool

    // Emit initial progress with total count
    let total_items = items.len();
    let _ = app.emit("kb-index-progress", serde_json::json!({
        "kb_id": kb_id,
        "status": "building",
        "progress": 0,
        "current": 0,
        "total": total_items,
        "message": format!("准备构建索引：{} 个切片，维度 {}", total_items, dim)
    }));

    let app_clone = app.clone();
    let kb_id_clone = kb_id.to_string();

    // CPU-intensive build runs on blocking thread pool to avoid starving async runtime
    let index = tokio::task::spawn_blocking(move || {
        let mut index = HnswIndex::new(dim, DEFAULT_M, DEFAULT_EF_CONSTRUCTION, DEFAULT_EF_SEARCH);
        index.build_with_progress(&items, |current, total| {
            let pct = if total > 0 { current * 100 / total } else { 100 };
            let _ = app_clone.emit("kb-index-progress", serde_json::json!({
                "kb_id": &kb_id_clone,
                "status": "building",
                "progress": pct,
                "current": current,
                "total": total,
                "message": format!("构建中 {}/{} ({}%)", current, total, pct)
            }));
        });
        index
    })
    .await
    .map_err(|e| format!("Build task panicked: {}", e))?;

    let index = index;

    // Save to file
    let path = index_path(kb_id);
    index
        .save(&path)
        .map_err(|e| format!("Failed to save index: {}", e))?;

    // Update DB metadata
    repo.upsert_index_meta(kb_id, dim as i64, total_items as i64, Some(path.to_str().unwrap_or("")), "ready")
        .await
        .map_err(|e| format!("Failed to update index meta: {}", e))?;

    repo.update_kb_index_status(kb_id, "ready")
        .await
        .map_err(|e| format!("Failed to update KB index status: {}", e))?;

    tracing::info!(
        "HNSW index built for KB {}: {} nodes, dim {}, saved to {:?}",
        kb_id, total_items, dim, path
    );

    Ok(())
}

/// Drop the HNSW index for a KB.
pub async fn drop_index(pool: &SqlitePool, kb_id: &str) -> Result<(), String> {
    let repo = KbRepository::new(pool.clone());

    // Delete index file
    let path = index_path(kb_id);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| format!("Failed to remove index file: {}", e))?;
    }

    // Update DB metadata
    repo.upsert_index_meta(kb_id, 0, 0, None, "none")
        .await
        .map_err(|e| format!("Failed to update index meta: {}", e))?;

    repo.update_kb_index_status(kb_id, "none")
        .await
        .map_err(|e| format!("Failed to update KB index status: {}", e))?;

    tracing::info!("HNSW index dropped for KB {}", kb_id);

    Ok(())
}

/// Get index metadata from DB.
pub async fn get_index_status(pool: &SqlitePool, kb_id: &str) -> Result<Option<super::models::KbIndexMeta>, String> {
    let repo = KbRepository::new(pool.clone());
    repo.get_index_meta(kb_id)
        .await
        .map_err(|e| format!("Failed to get index meta: {}", e))
}

// ════════════════════════════════════════════════════════
// FTS5 hybrid search
// ════════════════════════════════════════════════════════

/// FTS5 full-text search
async fn fts5_search(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    let fts_query = build_fts_query(query);

    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT c.id, c.content, c.metadata, d.filename, c.doc_id \
         FROM kb_chunks_fts fts \
         JOIN kb_chunks c ON fts.chunk_id = c.id \
         JOIN kb_documents d ON c.doc_id = d.id \
         WHERE c.kb_id = ? AND d.status = 'ready' AND kb_chunks_fts MATCH ? \
         ORDER BY rank \
         LIMIT ?"
    )
    .bind(kb_id)
    .bind(&fts_query)
    .bind(top_k as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("FTS5 search failed: {}", e))?;

    let results = rows.into_iter().enumerate().map(|(idx, (id, content, metadata, filename, doc_id))| {
        let score = 1.0 / (1.0 + idx as f32 * 0.1);
        let meta: serde_json::Value = serde_json::from_str(&metadata).unwrap_or_default();
        SearchResult {
            chunk_id: id,
            doc_id,
            filename,
            content,
            score,
            metadata: meta,
        }
    }).collect();

    Ok(results)
}

/// Build FTS5 query string from user query
fn build_fts_query(query: &str) -> String {
    let tokens: Vec<&str> = query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return query.to_string();
    }

    tokens.iter()
        .map(|t| format!("{}*", t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Hybrid search: vector + FTS5 weighted merge
pub async fn hybrid_search(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,
    query_embedding: &[f32],
    top_k: usize,
    vector_weight: f32,
    keyword_weight: f32,
) -> Result<Vec<SearchResult>, String> {
    let (vector_results, keyword_results) = tokio::join!(
        search(pool, kb_id, query_embedding, top_k * 2),
        fts5_search(pool, kb_id, query, top_k * 2),
    );

    let vector_results = vector_results.unwrap_or_default();
    let keyword_results = keyword_results.unwrap_or_default();

    // Merge & deduplicate with weighted scoring
    let mut merged: std::collections::HashMap<String, SearchResult> = std::collections::HashMap::new();
    let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    for r in &vector_results {
        scores.insert(r.chunk_id.clone(), r.score * vector_weight);
        merged.insert(r.chunk_id.clone(), r.clone());
    }

    for r in &keyword_results {
        let current = scores.entry(r.chunk_id.clone()).or_insert(0.0);
        *current += r.score * keyword_weight;
        merged.entry(r.chunk_id.clone()).or_insert_with(|| r.clone());
    }

    // Sort by weighted score descending
    let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_k);

    let final_results: Vec<SearchResult> = ranked.into_iter()
        .filter_map(|(id, score)| {
            merged.get(&id).map(|r| {
                let mut r = r.clone();
                r.score = score;
                r
            })
        })
        .collect();

    Ok(final_results)
}

// ════════════════════════════════════════════════════════
// Utility functions
// ════════════════════════════════════════════════════════

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    bincode::deserialize(blob).unwrap_or_default()
}

/// Encode embedding to BLOB for storage.
pub fn encode_embedding(vec: &[f32]) -> Vec<u8> {
    bincode::serialize(vec).unwrap_or_default()
}

// ════════════════════════════════════════════════════════
// Token estimation utilities
// ════════════════════════════════════════════════════════

/// Estimate token count: ~4 chars/token for ASCII, ~2 chars/token for CJK.
pub fn estimate_tokens(text: &str) -> usize {
    let ascii_chars = text.chars().filter(|c| c.is_ascii()).count();
    let non_ascii_chars = text.chars().filter(|c| !c.is_ascii()).count();
    (ascii_chars / 4) + (non_ascii_chars / 2) + 1
}

/// Get model context window limit.
pub fn get_model_context_limit(model: &str) -> usize {
    let m = model.to_lowercase();
    if m.contains("gpt-4o") {
        128_000
    } else if m.contains("gpt-4") {
        8_192
    } else if m.contains("gpt-3.5") {
        16_385
    } else if m.contains("claude-3")
        || m.contains("claude-sonnet")
        || m.contains("claude-opus")
        || m.contains("claude-haiku")
    {
        200_000
    } else if m.contains("gemini") {
        1_000_000
    } else if m.contains("deepseek") {
        64_000
    } else if m.contains("qwen") {
        32_000
    } else if m.contains("llama") {
        8_192
    } else if m.contains("mistral") || m.contains("mixtral") {
        32_000
    } else {
        8_192
    }
}
