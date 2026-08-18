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
        // App 生命周期即进程生命周期，泄漏一次以换取 'static 引用，
        // 使管理路由可以构造 State<'static, Arc<AppState>> 直接复用 Tauri command。
        app_static: Box::leak(Box::new(app.clone())),
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

    // Web 管理面板（/admin/api/*，自带会话鉴权）
    let admin = super::admin_routes::router(shared.clone());

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
        // Web 管理面板 API
        .nest("/admin/api", admin)
        // 内嵌 Web 静态资源（SPA fallback，须放在所有 API 路由之后）
        .merge(super::static_assets::static_router())
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(cors)
        .with_state(shared)
}

#[derive(Clone)]
pub struct SharedState {
    pub app: AppHandle,
    pub app_static: &'static AppHandle,
    pub state: Arc<AppState>,
}
