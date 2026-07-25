pub mod models;
pub mod repository;
pub mod processor;
pub mod parser;
pub mod splitter;
pub mod embedder;
pub mod retriever;
pub mod rag;
pub mod handlers;
pub mod routes;

use async_trait::async_trait;
use axum::Router;
use std::sync::Arc;
use crate::AppState;
use crate::server::router::SharedState;
use super::{Service, ServiceStatus};

pub struct KnowledgeService;

#[async_trait]
impl Service for KnowledgeService {
    fn id(&self) -> &'static str { "knowledge" }
    fn name(&self) -> &'static str { "知识库" }
    fn description(&self) -> &'static str { "本地知识库：文件上传、文本切片、Embedding、向量检索、RAG 问答" }

    async fn status(&self, state: &Arc<AppState>) -> ServiceStatus {
        let pool = &state.db.pool;
        let kb_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_knowledge_bases")
            .fetch_one(pool).await.unwrap_or(0);
        let doc_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_documents")
            .fetch_one(pool).await.unwrap_or(0);
        let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_chunks")
            .fetch_one(pool).await.unwrap_or(0);

        ServiceStatus {
            id: self.id().to_string(),
            name: self.name().to_string(),
            description: self.description().to_string(),
            enabled: true,
            running: true,
            stats: serde_json::json!({
                "knowledge_bases": kb_count,
                "documents": doc_count,
                "chunks": chunk_count,
            }),
        }
    }

    fn routes(&self, state: Arc<AppState>) -> Router<SharedState> {
        routes::create_router(state)
    }
}
