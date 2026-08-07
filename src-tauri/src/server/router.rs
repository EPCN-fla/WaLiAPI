use super::handlers::*;
use crate::AppState;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tauri::AppHandle;
use tower_http::cors::{Any, CorsLayer};

pub fn create_router(app: AppHandle, state: Arc<AppState>) -> Router {
    let shared = SharedState {
        app: app.clone(),
        state: state.clone(),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    // Service registry — merge all service routes
    let registry = crate::services::ServiceRegistry::new();
    let service_router = registry.merge_routes(state.clone());

    Router::new()
        // OpenAI Chat Completions
        .route("/v1/chat/completions", post(handle_chat_completions))
        // OpenAI Completions (legacy)
        .route("/v1/completions", post(handle_completions))
        // OpenAI Responses API
        .route("/v1/responses", post(handle_responses))
        // OpenAI Embeddings
        .route("/v1/embeddings", post(handle_embeddings))
        // OpenAI Models
        .route("/v1/models", get(handle_list_models))
        // OpenAI Images
        .route("/v1/images/generations", post(handle_images))
        // OpenAI Audio
        .route(
            "/v1/audio/transcriptions",
            post(handle_audio_transcriptions),
        )
        .route("/v1/audio/speech", post(handle_audio_speech))
        // Anthropic Messages API
        .route(
            "/v1/messages",
            post(handle_messages).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .route(
            "/v1/messages/count_tokens",
            post(handle_messages_count_tokens).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        // Health check
        .route("/health", get(handle_health))
        // Service routes (Knowledge Base, MCP, etc.)
        .merge(service_router)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(cors)
        .with_state(shared)
}

#[derive(Clone)]
pub struct SharedState {
    pub app: AppHandle,
    pub state: Arc<AppState>,
}
