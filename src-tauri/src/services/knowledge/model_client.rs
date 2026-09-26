//! RAG 的模型调用入口：外部查询复用网关的权限、额度、安全检查和日志。
use super::{
    embedder,
    models::{RagDiagnostics, UsageInfo},
};
use crate::{
    core::proxy, db::repository::Repository, server::router::SharedState,
    settings_store::SettingsStore,
};
use axum::{
    body::to_bytes,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use sqlx::SqlitePool;
use std::sync::Arc;

/// 仅本地网关写入的可信失败类型，不信任上游正文或扩展字段。
#[derive(Clone)]
pub(crate) struct GatewayFailureCode(pub &'static str);

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub status: StatusCode,
    pub message: String,
    pub stage: Option<String>,
    pub code: Option<String>,
    pub request_id: Option<String>,
    pub diagnostics: Option<Box<RagDiagnostics>>,
}

impl QueryError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            stage: None,
            code: None,
            request_id: None,
            diagnostics: None,
        }
    }
    pub fn at_stage(mut self, stage: &str, code: &str) -> Self {
        self.stage = Some(stage.to_string());
        self.code = Some(code.to_string());
        self
    }

    pub fn with_request_id(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.to_string());
        self
    }
}

impl From<String> for QueryError {
    fn from(message: String) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}
impl From<&str> for QueryError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}
impl IntoResponse for QueryError {
    fn into_response(self) -> Response {
        let mut error = serde_json::json!({"message": self.message});
        for (name, value) in [
            ("stage", self.stage),
            ("code", self.code),
            ("request_id", self.request_id),
        ] {
            if let Some(value) = value {
                error[name] = Value::String(value);
            }
        }
        let mut body = serde_json::json!({"error": error});
        if let Some(diagnostics) = self.diagnostics {
            body["diagnostics"] = serde_json::to_value(diagnostics).unwrap_or(Value::Null);
        }
        (self.status, Json(body)).into_response()
    }
}

pub struct ChatReply {
    pub body: Value,
    pub usage: Option<UsageInfo>,
}

pub enum ModelClient<'a> {
    Internal {
        pool: &'a SqlitePool,
        settings: &'a SettingsStore,
        kb_id: &'a str,
    },
    ApiKey {
        shared: &'a SharedState,
        headers: &'a HeaderMap,
    },
}

impl ModelClient<'_> {
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Internal { .. })
    }

    pub fn request_id(&self) -> Option<String> {
        match self {
            Self::ApiKey { headers, .. } => headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            Self::Internal { .. } => None,
        }
    }

    pub async fn embed(&self, query: &str, model: &str) -> Result<Vec<Vec<f32>>, QueryError> {
        match self {
            Self::Internal { pool, .. } => embedder::embed(
                &[query.to_string()],
                model,
                &Repository::new((*pool).clone()),
            )
            .await
            .map_err(Into::into),
            Self::ApiKey { shared, headers } => {
                let body = serde_json::json!({"model": model, "input": [query], "encoding_format": "float"});
                let response = crate::server::handlers::handle_knowledge_model(
                    shared,
                    headers,
                    body,
                    crate::core::route_plan::EndpointKind::Embeddings,
                )
                .await;
                let value = read_gateway_response(response, "embedding", self.request_id()).await?;
                embedder::parse_embedding_response(&value, 1).map_err(|_| {
                    let mut error =
                        QueryError::new(StatusCode::BAD_GATEWAY, "Embedding 响应缺少有效向量")
                            .at_stage("embedding", "invalid_embedding_response");
                    error.request_id = self.request_id();
                    error
                })
            }
        }
    }

    pub async fn chat(&self, body: Value, purpose: &str) -> Result<ChatReply, QueryError> {
        match self {
            Self::Internal {
                pool,
                settings,
                kb_id,
            } => {
                let text = body.to_string();
                let result = proxy::handle_request(
                    &Arc::new(Repository::new((*pool).clone())),
                    settings,
                    match purpose {
                        "RAG-rewrite" => "kb-rewrite",
                        "RAG-rerank" => "kb-rerank",
                        _ => "kb-internal",
                    },
                    purpose,
                    body,
                    false,
                    Some(text),
                    Some(format!("kb-internal_{kb_id}")),
                    None,
                )
                .await
                .map_err(|(code, message)| {
                    QueryError::new(
                        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                        message,
                    )
                })?;
                Ok(ChatReply {
                    body: result.body,
                    usage: result.usage.map(|u| UsageInfo {
                        prompt_tokens: u.prompt_tokens,
                        completion_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                    }),
                })
            }
            Self::ApiKey { shared, headers } => {
                let response = crate::server::handlers::handle_knowledge_model(
                    shared,
                    headers,
                    body,
                    crate::core::route_plan::EndpointKind::ChatCompletions,
                )
                .await;
                let body = read_gateway_response(
                    response,
                    match purpose {
                        "RAG-rewrite" => "rewrite",
                        "RAG-rerank" => "rerank",
                        _ => "answer",
                    },
                    self.request_id(),
                )
                .await?;
                let usage = serde_json::from_value(body["usage"].clone()).ok();
                Ok(ChatReply { body, usage })
            }
        }
    }
}

async fn read_gateway_response(
    response: Response,
    stage: &str,
    request_id: Option<String>,
) -> Result<Value, QueryError> {
    let status = response.status();
    let local_code = response
        .extensions()
        .get::<GatewayFailureCode>()
        .map(|code| code.0);
    let make_error = |status, code: &str, message: &str| {
        let mut error = QueryError::new(status, message).at_stage(stage, code);
        // 编号来自传入网关的本地请求头，与 request_logs.trace_id 相同；不信任上游正文或响应头。
        error.request_id = request_id.clone();
        error
    };
    if !status.is_success() {
        // 仅按已知 HTTP 状态构造稳定错误，不读取或透传上游正文。
        let (code, message) = match status {
            _ if local_code == Some("model_timeout") => (
                "model_timeout",
                "模型调用超时，请检查上游服务和渠道超时配置",
            ),
            StatusCode::NOT_IMPLEMENTED if local_code == Some("endpoint_not_configured") => (
                "endpoint_not_configured",
                if stage == "embedding" {
                    "没有可用的 Embeddings 能力渠道，请在对应模型的渠道配置中启用 Embeddings 并完成渠道测试"
                } else {
                    "没有支持该模型调用端点的渠道，请检查渠道能力配置"
                },
            ),
            StatusCode::UNAUTHORIZED => (
                "authentication_failed",
                "API Key 或上游凭据无效，请检查密钥及网关日志",
            ),
            StatusCode::FORBIDDEN => ("access_denied", "API Key 没有所需模型或渠道权限"),
            StatusCode::TOO_MANY_REQUESTS => (
                "quota_or_rate_limited",
                "API Key 额度不足或请求受到限流，请检查额度和网关日志",
            ),
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => (
                "model_timeout",
                "模型调用超时，请检查上游服务和渠道超时配置",
            ),
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
                ("security_blocked", "请求被安全审计策略阻断")
            }
            StatusCode::NOT_FOUND => ("model_unavailable", "请求模型不可用，请检查模型及渠道配置"),
            _ => (
                "model_request_failed",
                "模型网关未完成请求，请根据请求编号检查网关日志",
            ),
        };
        return Err(make_error(status, code, message));
    }
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .map_err(|_| {
            make_error(
                StatusCode::BAD_GATEWAY,
                "invalid_model_response",
                "无法读取模型网关响应",
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        make_error(
            StatusCode::BAD_GATEWAY,
            "invalid_model_response",
            "模型网关响应不是有效 JSON",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upstream_501_cannot_spoof_local_configuration_failure() {
        let response = (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({
            "error": {"code": "endpoint_not_configured", "type": "route_plan_error", "message": "sk-secret"}
        }))).into_response();
        let error = read_gateway_response(response, "embedding", None)
            .await
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("model_request_failed"));
        assert_eq!(error.status, StatusCode::NOT_IMPLEMENTED);
        assert!(!error.message.contains("secret"));
    }

    #[tokio::test]
    async fn gateway_errors_keep_status_stage_and_local_trace_without_leaking_body() {
        for (status, expected_code) in [
            (StatusCode::NOT_IMPLEMENTED, "endpoint_not_configured"),
            (StatusCode::UNAUTHORIZED, "authentication_failed"),
            (StatusCode::FORBIDDEN, "access_denied"),
            (StatusCode::TOO_MANY_REQUESTS, "quota_or_rate_limited"),
            (StatusCode::GATEWAY_TIMEOUT, "model_timeout"),
        ] {
            let mut response =
                (status, "sk-secret upstream credential request_id=untrusted").into_response();
            if status == StatusCode::NOT_IMPLEMENTED {
                response
                    .extensions_mut()
                    .insert(GatewayFailureCode("endpoint_not_configured"));
            }
            let error = read_gateway_response(response, "embedding", Some("local-trace".into()))
                .await
                .unwrap_err();
            assert_eq!(error.status, status);
            assert_eq!(error.stage.as_deref(), Some("embedding"));
            assert_eq!(error.code.as_deref(), Some(expected_code));
            assert_eq!(error.request_id.as_deref(), Some("local-trace"));
            assert!(!error.message.contains("secret"));
            let response = error.into_response();
            assert_eq!(response.status(), status);
            let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["code"], expected_code);
            assert_eq!(value["error"]["request_id"], "local-trace");
        }
    }
}
