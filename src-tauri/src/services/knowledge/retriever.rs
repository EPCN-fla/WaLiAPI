use super::models::{SearchResult};
use super::repository::KbRepository;
use sqlx::SqlitePool;

/// Search knowledge base by query embedding
pub async fn search(
    pool: &SqlitePool,
    kb_id: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());

    // Load all chunks with embeddings for this KB
    let chunks = repo
        .get_chunks_by_kb(kb_id)
        .await
        .map_err(|e| format!("Failed to load chunks: {}", e))?;

    if chunks.is_empty() {
        return Ok(vec![]);
    }

    // Calculate cosine similarity for each chunk
    let mut scored: Vec<(f32, usize)> = chunks
        .iter()
        .enumerate()
        .map(|(i, (_, _, _, emb, _, _))| {
            let vector = decode_embedding(emb);
            let score = cosine_similarity(query_embedding, &vector);
            (score, i)
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

    // Build results
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

/// Search across all knowledge bases
/// If mcp_only is true, only search KBs with mcp_enabled = 1
pub async fn search_all(
    pool: &SqlitePool,
    query_embedding: &[f32],
    top_k: usize,
    mcp_only: bool,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());

    // Get all active KBs
    let kbs = repo
        .get_all_kbs()
        .await
        .map_err(|e| format!("Failed to get KBs: {}", e))?;

    let active_kbs: Vec<_> = kbs.iter().filter(|kb| {
        kb.status == 1 && (!mcp_only || kb.mcp_enabled == 1)
    }).collect();

    if active_kbs.is_empty() {
        return Ok(vec![]);
    }

    // Search each KB and merge results
    let mut all_results = Vec::new();
    for kb in &active_kbs {
        if let Ok(results) = search(pool, &kb.id, query_embedding, top_k).await {
            all_results.extend(results);
        }
    }

    // Sort and truncate
    all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    all_results.truncate(top_k);

    Ok(all_results)
}

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
    // Stored as bincode-serialized Vec<f32>
    bincode::deserialize(blob).unwrap_or_default()
}

/// Encode embedding to BLOB for storage
pub fn encode_embedding(vec: &[f32]) -> Vec<u8> {
    bincode::serialize(vec).unwrap_or_default()
}
