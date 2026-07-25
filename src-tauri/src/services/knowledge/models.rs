use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct KbKnowledgeBase {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: i64,
    pub doc_count: i64,
    pub chunk_count: i64,
    pub total_tokens: i64,
    pub embedding_model: Option<String>,
    pub embedding_channel_id: Option<String>,
    pub mcp_enabled: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateKbInput {
    pub name: String,
    pub description: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_channel_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateKbInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_channel_id: Option<String>,
    pub status: Option<i64>,
    pub mcp_enabled: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct KbDocument {
    pub id: String,
    pub kb_id: String,
    pub filename: String,
    pub file_path: Option<String>,
    pub file_type: String,
    pub file_size: i64,
    pub content_hash: String,
    pub chunk_count: i64,
    pub token_count: i64,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadDocInput {
    pub filename: String,
    pub file_path: Option<String>,
    pub content: String, // base64 encoded
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct KbChunk {
    pub id: String,
    pub doc_id: String,
    pub kb_id: String,
    pub chunk_index: i64,
    pub content: String,
    pub token_count: i64,
    pub metadata: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub chunk_id: String,
    pub doc_id: String,
    pub filename: String,
    pub content: String,
    pub score: f32,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagAnswer {
    pub answer: String,
    pub sources: Vec<SourceInfo>,
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    pub filename: String,
    pub score: f32,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct KbTask {
    pub id: String,
    pub kb_id: String,
    pub doc_id: Option<String>,
    pub task_type: String,
    pub status: String,
    pub progress: i64,
    pub total_items: i64,
    pub done_items: i64,
    pub error_message: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}
