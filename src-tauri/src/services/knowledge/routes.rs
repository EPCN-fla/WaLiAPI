use axum::{Router, routing::{get, post}};
#[allow(unused_imports)]
use axum::routing::{delete as _delete, put as _put};
use std::sync::Arc;
use crate::AppState;
use crate::server::router::SharedState;
use super::handlers;

pub fn create_router(_state: Arc<AppState>) -> Router<SharedState> {
    Router::new()
        // Knowledge Base CRUD
        .route("/api/kb", get(handlers::list_knowledge_bases).post(handlers::create_knowledge_base))
        .route("/api/kb/{id}", get(handlers::get_knowledge_base).put(handlers::update_knowledge_base).delete(handlers::delete_knowledge_base))
        .route("/api/kb/{id}/stats", get(handlers::kb_stats))
        // Documents
        .route("/api/kb/{id}/documents", get(handlers::list_documents).post(handlers::upload_document))
        .route("/api/kb/{kb_id}/documents/{doc_id}", get(handlers::get_document).delete(handlers::delete_document))
        .route("/api/kb/{kb_id}/documents/{doc_id}/reindex", post(handlers::reindex_document))
        // Search & RAG
        .route("/api/kb/search", get(handlers::search))
        .route("/api/kb/ask", post(handlers::ask))
}
