pub mod budget;
pub mod code_parser;
pub mod embedder;
pub mod handlers;
pub mod import_guard;
pub mod importer;
pub mod index;
pub mod models;
pub mod ocr;
pub mod parser;
pub mod processor;
pub mod rag;
pub mod repository;
pub mod retriever;
pub mod routes;
pub mod splitter;
pub mod text;
pub mod upload;

/// 知识库的 Embedding 与 OCR 请求复用网关模型映射语义：规范化历史数据、
/// 排除已禁用映射，并支持数组目标按次抽样。
pub(crate) fn resolve_channel_model(channel: &crate::db::models::Channel, model: &str) -> String {
    let mapping = channel.active_model_mapping();
    let mut rng = rand::rng();
    crate::core::route_plan::resolve_upstream_model(&mapping, model, &mut rng)
}

#[cfg(test)]
mod model_mapping_tests {
    use super::*;
    use crate::db::models::Channel;

    fn channel(mapping: serde_json::Value, disabled: serde_json::Value) -> Channel {
        Channel {
            id: "knowledge-channel".into(),
            name: "Knowledge".into(),
            channel_type: "openai".into(),
            base_url: "https://example.test/v1".into(),
            api_key: "secret".into(),
            models: "[]".into(),
            status: 1,
            priority: 0,
            weight: 1,
            config: "{}".into(),
            model_mapping: mapping.to_string(),
            model_mapping_disabled: disabled.to_string(),
            timeout_secs: 60,
            protocol: Some("openai".into()),
            provider: Some("custom".into()),
            native_base_url: Some("https://example.test/v1".into()),
            native_endpoints: Some("[]".into()),
            preset_revision: None,
            identity_revision: 1,
            legacy_executor_override: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_test_at: None,
            last_test_ok: None,
            last_probe_at: None,
            last_probe_ok: None,
            probe_latency_ms: None,
            api_key_enabled: Some(1),
        }
    }

    #[test]
    fn knowledge_clients_use_normalized_active_mapping() {
        let enabled = channel(
            serde_json::json!({" alias ": " upstream "}),
            serde_json::json!([]),
        );
        assert_eq!(resolve_channel_model(&enabled, "alias"), "upstream");

        let disabled = channel(
            serde_json::json!({" alias ": " upstream "}),
            serde_json::json!([[" alias ", " upstream "]]),
        );
        assert_eq!(resolve_channel_model(&disabled, "alias"), "alias");
    }
}

use super::{Service, ServiceStatus};
use crate::server::router::SharedState;
use crate::AppState;
use async_trait::async_trait;
use axum::Router;
use std::sync::Arc;

pub struct KnowledgeService;

#[async_trait]
impl Service for KnowledgeService {
    fn id(&self) -> &'static str {
        "knowledge"
    }
    fn name(&self) -> &'static str {
        "RAG"
    }
    fn description(&self) -> &'static str {
        "本地 RAG 知识库：创建私有知识库，上传文档自动向量化并构建 HNSW 索引，通过 MCP 协议对外提供检索和 RAG 问答工具，支持任意 AI Agent 对接"
    }

    async fn status(&self, state: &Arc<AppState>) -> ServiceStatus {
        let pool = &state.db.pool;
        let kb_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_knowledge_bases")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
        let doc_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_documents")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
        let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_chunks")
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

pub mod model_client;
