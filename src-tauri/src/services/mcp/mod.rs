pub mod handlers;

use async_trait::async_trait;
use axum::Router;
use std::sync::Arc;
use crate::AppState;
use crate::server::router::SharedState;
use super::{Service, ServiceStatus};

pub struct McpService;

#[async_trait]
impl Service for McpService {
    fn id(&self) -> &'static str { "mcp" }
    fn name(&self) -> &'static str { "MCP Server" }
    fn description(&self) -> &'static str { "Model Context Protocol Server，对外暴露知识库工具（支持创建/更新/删除知识库、上传/删除文档、导入源、构建索引、搜索、RAG问答）" }

    async fn status(&self, state: &Arc<AppState>) -> ServiceStatus {
        let pool = &state.db.pool;
        let kb_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_knowledge_bases WHERE status = 1")
            .fetch_one(pool).await.unwrap_or(0);

        ServiceStatus {
            id: self.id().to_string(),
            name: self.name().to_string(),
            description: self.description().to_string(),
            enabled: true,
            running: true,
            stats: serde_json::json!({
                "available_knowledge_bases": kb_count,
                "tools": ["search_knowledge_base", "list_knowledge_bases", "read_document", "ask_knowledge_base", "get_knowledge_base_stats", "create_knowledge_base", "update_knowledge_base", "delete_knowledge_base", "upload_document", "delete_document", "list_documents", "build_index", "import_source"],
            }),
        }
    }

    fn routes(&self, _state: Arc<AppState>) -> Router<SharedState> {
        Router::new()
            .route("/mcp", axum::routing::post(handlers::handle_mcp))
            .route("/mcp/sse", axum::routing::get(handlers::handle_mcp_sse))
    }
}
