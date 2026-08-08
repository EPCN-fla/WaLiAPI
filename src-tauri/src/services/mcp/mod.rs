pub mod handlers;

use super::{Service, ServiceStatus};
use crate::server::router::SharedState;
use crate::AppState;
use async_trait::async_trait;
use axum::Router;
use std::sync::Arc;

pub struct McpService;

#[async_trait]
impl Service for McpService {
    fn id(&self) -> &'static str {
        "mcp"
    }
    fn name(&self) -> &'static str {
        "MCP Server"
    }
    fn description(&self) -> &'static str {
        "Model Context Protocol Server，对外暴露 RAG 工具（支持创建/更新/删除 RAG、上传/删除文档、导入源、构建索引、搜索、RAG问答）"
    }

    async fn status(&self, state: &Arc<AppState>) -> ServiceStatus {
        let pool = &state.db.pool;
        let kb_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM kb_knowledge_bases WHERE status = 1")
                .fetch_one(pool)
                .await
                .unwrap_or(0);

        ServiceStatus {
            id: self.id().to_string(),
            name: self.name().to_string(),
            description: self.description().to_string(),
            enabled: true,
            running: true,
            stats: serde_json::json!({
                "available_knowledge_bases": kb_count,
                "tools": [
                {"name": "search_knowledge_base", "label": "语义搜索", "desc": "基于 HNSW 向量索引的语义搜索，返回匹配文本片段及相似度评分"},
                {"name": "list_knowledge_bases", "label": "列出 RAG", "desc": "列出所有已启用 MCP 的 RAG，含 ID、名称、文档数、切片数"},
                {"name": "read_document", "label": "读取文档", "desc": "读取 RAG 中指定文档的完整内容"},
                {"name": "ask_knowledge_base", "label": "RAG 问答", "desc": "基于 RAG 内容的智能问答，返回 AI 生成的回答及来源引用"},
                {"name": "get_knowledge_base_stats", "label": "RAG 统计", "desc": "获取 RAG 详细统计：文档数、切片数、Token 数、索引状态"},
                {"name": "create_knowledge_base", "label": "创建 RAG", "desc": "创建新 RAG，支持自定义名称、描述、向量模型"},
                {"name": "update_knowledge_base", "label": "更新 RAG", "desc": "更新 RAG 配置：名称、描述、分块大小、MCP 开关等"},
                {"name": "delete_knowledge_base", "label": "删除 RAG", "desc": "永久删除 RAG 及其所有文档、切片和索引"},
                {"name": "upload_document", "label": "上传文档", "desc": "上传文档到 RAG，自动解析→分块→向量化→索引"},
                {"name": "delete_document", "label": "删除文档", "desc": "删除 RAG 中的指定文档及其切片和向量"},
                {"name": "list_documents", "label": "文档列表", "desc": "列出 RAG 中所有文档及处理状态、切片数、Token 数"},
                {"name": "build_index", "label": "构建索引", "desc": "构建或重建 HNSW 向量索引，提升搜索性能"},
                {"name": "import_source", "label": "导入源", "desc": "从 Git 仓库、URL 或本地目录批量导入文档"}
            ],
            }),
        }
    }

    fn routes(&self, _state: Arc<AppState>) -> Router<SharedState> {
        Router::new()
            // Primary Streamable HTTP endpoint (POST = JSON-RPC, GET = SSE upgrade)
            .route(
                "/mcp",
                axum::routing::post(handlers::handle_mcp).get(handlers::handle_mcp_sse),
            )
            // Trailing-slash variant — some clients send /mcp/
            .route(
                "/mcp/",
                axum::routing::post(handlers::handle_mcp).get(handlers::handle_mcp_sse),
            )
            // Legacy SSE endpoint — keep for backwards compat
            .route(
                "/mcp/sse",
                axum::routing::get(handlers::handle_mcp_sse).post(handlers::handle_mcp),
            )
    }
}
