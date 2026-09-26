use crate::core::attempt::{AttemptFailure, AttemptResult, FailureClass};
use crate::core::route_plan::{plan_internal_embeddings, EndpointKind, RouteCandidate};
use crate::db::repository::Repository;
use crate::endpoint_executor::driver::{
    dispatch_channel_with_key_failover, record_channel_mode_outcome,
};
use crate::security::gate::{audit_envelope, DownstreamProtocol, RequestEnvelope};
use rand::SeedableRng;
use std::collections::HashMap;

/// 受信任的内部知识库身份调用 Embedding，不创建或借用外部 API Key。
/// 与外部网关复用模型/端点路由及执行器；外部请求的权限、额度和审计仍由网关处理。
pub async fn embed(
    texts: &[String],
    model: &str,
    repo: &Repository,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    // 查询和文档统一修复 PDF 部首字形，覆盖 REST、MCP、管理命令入口。
    let texts: Vec<String> = texts
        .iter()
        .map(|t| super::text::normalize_radicals(t))
        .collect();
    let body = serde_json::json!({
        "model": model,
        "input": texts,
        "encoding_format": "float"
    });
    let channels = repo
        .get_enabled_channels_for_mode(
            EndpointKind::Embeddings.as_str(),
            false,
            &crate::utils::time::now_iso(),
        )
        .await
        .map_err(|_| "读取 Embedding 渠道失败".to_string())?;
    let plan = plan_internal_embeddings(
        model,
        &channels,
        &body,
        &mut rand::rngs::StdRng::from_os_rng(),
    )
    .map_err(|error| match error {
        crate::core::route_plan::PlanError::NoEndpointSupported(..) => format!(
            "Embedding 渠道未声明 embeddings 能力 (HTTP 501)，请检查模型 {model} 的渠道端点配置"
        ),
        _ => format!(
            "Embedding 路由不可用 (HTTP {}): {}",
            error.http_status(),
            error.message()
        ),
    })?;
    let lookup: HashMap<_, _> = plan
        .groups
        .iter()
        .flat_map(|group| &group.candidates)
        .map(|candidate| {
            (
                candidate.candidate.id().to_string(),
                candidate.candidate.clone(),
            )
        })
        .collect();
    // 内部文档处理没有外部 Key 的安全策略；使用 gate 构造规范 envelope，
    // 保留原有内部审计语义，不把内部请求伪装成经过外部权限校验的请求。
    let audited = audit_envelope(
        RequestEnvelope {
            downstream_protocol: DownstreamProtocol::Embeddings,
            endpoint: "internal://knowledge/embeddings".to_string(),
            original_json: body,
            safe_forward_headers: vec![],
            query: None,
            model: model.to_string(),
            stream: false,
            trace_id: Some(format!("kb-internal_{}", uuid::Uuid::new_v4())),
        },
        &crate::security::SecuritySettings::default(),
        None,
        vec![],
    )
    .map_err(|_| "无法构造内部 Embedding 请求".to_string())?;
    let expected_count = texts.len();
    let execution = crate::core::plan_executor::execute_plan(
        plan,
        &audited,
        rand::rngs::StdRng::from_os_rng(),
        |attempt| {
            let candidate = lookup.get(&attempt.channel_id).cloned();
            let attempt = attempt.clone();
            async move {
                let Some(RouteCandidate::Channel { channel, identity }) = candidate else {
                    return AttemptResult::Failure(AttemptFailure {
                        failure_class: FailureClass::UpstreamProtocolError,
                        message: "内部 Embedding 渠道不存在".to_string(),
                        status_code: Some(502),
                        retry_after: None,
                    });
                };
                let mut result = dispatch_channel_with_key_failover(
                    EndpointKind::Embeddings,
                    &attempt,
                    &channel,
                    &identity,
                    &[],
                    None,
                    repo,
                )
                .await;
                // 只有整批向量有效才算成功；坏响应允许按网关预算换下一渠道。
                if let AttemptResult::Success(success) = &result {
                    if let Err(message) = parse_embedding_response(&success.body, expected_count) {
                        result = AttemptResult::Failure(AttemptFailure {
                            failure_class: FailureClass::UpstreamProtocolError,
                            message,
                            status_code: Some(502),
                            retry_after: None,
                        });
                    }
                }
                record_channel_mode_outcome(
                    repo,
                    &channel.id,
                    EndpointKind::Embeddings.as_str(),
                    false,
                    &result,
                )
                .await;
                result
            }
        },
    )
    .await;
    if execution.last_failure.is_some() || !(200..300).contains(&execution.status) {
        // 上游错误正文可能包含凭据或用户文本，只反馈稳定类别与状态。
        return Err(format!(
            "Embedding 请求失败 (HTTP {}, {})，请检查渠道配置或稍后重试",
            execution.status,
            execution
                .last_failure
                .as_ref()
                .map(|failure| failure.failure_class.as_str())
                .unwrap_or("upstream_error")
        ));
    }
    let embeddings = parse_embedding_response(&execution.body, expected_count)?;
    tracing::info!(
        caller = "knowledge_internal",
        channel_id = execution.channel_id.as_deref().unwrap_or_default(),
        model,
        texts = expected_count,
        dim = embeddings[0].len(),
        "Embedding success"
    );
    Ok(embeddings)
}

/// 按响应索引还原输入顺序，整批校验后才允许调用方写入文档切片。
/// 兼容完全不提供 index 的旧渠道；一旦提供索引就必须完整且唯一。
pub(crate) fn parse_embedding_response(
    response: &serde_json::Value,
    expected_count: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let data = response
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or("Invalid embedding response: missing data array")?;
    if data.len() != expected_count {
        return Err(format!(
            "Embedding count mismatch: expected {}, got {}",
            expected_count,
            data.len()
        ));
    }

    let indexed = data.iter().any(|item| item.get("index").is_some());
    let mut embeddings = vec![Vec::new(); expected_count];
    let mut dimension = None;
    for (position, item) in data.iter().enumerate() {
        let index = if indexed {
            item.get("index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .filter(|&index| index < expected_count)
                .ok_or_else(|| format!("Invalid embedding index at item {position}"))?
        } else {
            position
        };
        if !embeddings[index].is_empty() {
            return Err(format!("Duplicate embedding index: {index}"));
        }
        let embedding: Vec<f32> =
            serde_json::from_value(item.get("embedding").cloned().unwrap_or_default())
                .map_err(|_| format!("Embedding item {position} is not a float vector"))?;
        if embedding.is_empty() || embedding.iter().any(|value| !value.is_finite()) {
            return Err(format!("Invalid embedding vector at item {position}"));
        }
        if dimension.is_some_and(|dim| dim != embedding.len()) {
            return Err(format!(
                "Inconsistent embedding dimensions at item {position}"
            ));
        }
        dimension = Some(embedding.len());
        embeddings[index] = embedding;
    }
    Ok(embeddings)
}
