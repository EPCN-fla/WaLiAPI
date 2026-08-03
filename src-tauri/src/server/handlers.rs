use super::router::SharedState;
use crate::adaptor::{get_adaptor, ProxyRequest};
use crate::core::dispatcher::Dispatcher;
use crate::core::proxy;
use crate::db::repository::Repository;
use crate::protocol;
use crate::security;
use axum::{
    body::Body,
    extract::{OriginalUri, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use futures_util::StreamExt;

pub async fn handle_chat_completions(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body);
    let json: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(j) => j,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)).into_response(),
    };

    let is_stream = json
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let auth_header = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let api_key = auth_header.strip_prefix("Bearer ").unwrap_or("").trim();

    if api_key.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing API key").into_response();
    }

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));
    let key_record = match repo.get_api_key_by_key(api_key).await {
        Ok(k) => k,
        Err(_) => return (StatusCode::UNAUTHORIZED, "Invalid API key").into_response(),
    };

    if key_record.quota_limit > 0 && key_record.quota_used >= key_record.quota_limit {
        return (StatusCode::TOO_MANY_REQUESTS, "Quota exceeded").into_response();
    }

    // Extract Wali-Trace-Id from request headers
    let trace_id = headers
        .get("Wali-Trace-Id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Store full request body for logging (no truncation — let frontend handle display)
    let request_body_str = serde_json::to_string(&json).unwrap_or_default();

    if is_stream {
        handle_stream(
            shared,
            json,
            key_record.id,
            key_record.name,
            request_body_str,
            trace_id,
        )
        .await
    } else {
        match proxy::handle_request(
            &repo,
            &shared.app,
            &key_record.id,
            &key_record.name,
            json,
            false,
            Some(request_body_str),
            trace_id,
        )
        .await
        {
            Ok(result) => (StatusCode::OK, Json(result.body)).into_response(),
            Err((code, msg)) => {
                let err_body = serde_json::json!({
                    "error": { "message": msg, "type": "upstream_error", "code": code }
                });
                (
                    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(err_body),
                )
                    .into_response()
            }
        }
    }
}

/// Parse token usage from an SSE chunk's data line.
/// Looks for `usage` field in the JSON payload of `data: {...}` lines.
fn parse_usage_from_chunk(text: &str) -> Option<(i64, i64, i64)> {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue;
        }
        let data_str = trimmed.trim_start_matches("data:").trim();
        if data_str == "[DONE]" || data_str.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data_str) {
            if let Some(usage) = json.get("usage") {
                let prompt = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let completion = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let total = usage
                    .get("total_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if total > 0 || prompt > 0 || completion > 0 {
                    return Some((prompt, completion, total));
                }
            }
        }
    }
    None
}

async fn handle_stream(
    shared: SharedState,
    json: serde_json::Value,
    api_key_id: String,
    api_key_name: String,
    request_body: String,
    trace_id: Option<String>,
) -> Response {
    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let security_settings = security::get_security_settings(&shared.app);
    let security_result = security::scan_request(&json, &security_settings);

    // Real redaction: if redact mode is active, sanitize the request body before forwarding
    let (forward_json, was_redacted) =
        if matches!(security_result.action, security::SecurityAction::Redact)
            || security_settings.redact_secrets
        {
            security::redact_request_body(&json, &security_settings)
        } else {
            (json.clone(), false)
        };
    let mut security_result = security_result;
    if was_redacted {
        security_result.sanitized = true;
    }

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));

    if matches!(security_result.action, security::SecurityAction::Block) {
        let log = crate::db::models::RequestLog {
            response_choices: None,
            id: crate::utils::id::new_id(),
            seq: None,
            api_key_id: Some(api_key_id),
            api_key_name: Some(api_key_name),
            channel_id: None,
            channel_name: None,
            model: model.clone(),
            upstream_model: None,
            mode: "chat".to_string(),
            status_code: 451,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            duration_ms: 0,
            error_message: security_result.blocked_reason.clone(),
            is_stream: 1,
            is_retry: 0,
            created_at: crate::utils::time::now_iso(),
            request_body: Some(request_body),
            risk_level: security_result.risk_level.as_str().to_string(),
            risk_score: security_result.risk_score as i64,
            risk_summary: Some(security_result.summary.clone()),
            security_action: security_result.action.as_str().to_string(),
            sanitized: if security_result.sanitized { 1 } else { 0 },
            blocked_reason: security_result.blocked_reason.clone(),
            trace_id: trace_id.clone(),
        };
        let log_id = log.id.clone();
        if let Err(e) = repo.create_log(&log).await {
            eprintln!("[WARN] create_log failed: {}", e);
        }
        if let Err(e) = repo
            .create_security_findings(
                &log_id,
                &security_result.findings,
                security_result.action.as_str(),
            )
            .await
        {
            eprintln!("[WARN] create_security_findings failed: {}", e);
        }
        let err_body = serde_json::json!({"error": {"message": security_result.summary, "type": "security_blocked", "code": "security.blocked"}});
        return (StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, Json(err_body)).into_response();
    }
    let channels = match repo.get_enabled_channels().await {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "No channels available").into_response()
        }
    };

    let selected_channels = Dispatcher::select_channels(&channels, &model);
    if selected_channels.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "No channel for model").into_response();
    }

    let request = ProxyRequest {
        model: model.clone(),
        body: forward_json.clone(),
        stream: true,
    };

    let (retry_enabled, retry_times) = proxy::get_retry_settings(&shared.app);
    let max_attempts = if retry_enabled {
        (retry_times.max(0) as usize + 1).min(selected_channels.len())
    } else {
        1
    };

    let mut last_error = None;

    for (attempt, channel) in selected_channels.into_iter().take(max_attempts).enumerate() {
        let config = Dispatcher::channel_to_config(&channel);
        let adaptor = get_adaptor(&channel.channel_type);

        // Compute the actual upstream model after mapping
        let upstream_model = resolve_mapped_model(&config.model_mapping, &model);

        match adaptor.forward_stream(&request, &config).await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body_str = resp.text().await.unwrap_or_default();
                    last_error = Some(format!("{}: {}", channel.name, body_str));
                    continue;
                }

                let start = std::time::Instant::now();
                let channel_id = channel.id.clone();
                let channel_name = channel.name.clone();
                let repo_clone = repo.clone();
                let api_key_id_clone = api_key_id.clone();
                let api_key_name_clone = api_key_name.clone();
                let model_clone = model.clone();
                let upstream_model_clone = upstream_model.clone();
                let request_body_clone = request_body.clone();
                let security_result_clone = security_result.clone();
                let trace_id_clone = trace_id.clone();
                let is_retry = if attempt > 0 { 1 } else { 0 };

                // ── Raw byte passthrough with usage parsing ───────────────
                // Forward upstream SSE bytes directly as the response body.
                // While passing through, scan data lines for `usage` to record
                // token consumption in the log.
                let upstream_stream = resp.bytes_stream();

                let passthrough_stream = async_stream::stream! {
                    tokio::pin!(upstream_stream);

                    // Accumulate token usage and response content from SSE chunks
                    let mut usage_prompt: i64 = 0;
                    let mut usage_completion: i64 = 0;
                    let mut usage_total: i64 = 0;
                    let mut had_error = false;
                    let mut accumulated_content = String::new();
                    let mut accumulated_reasoning = String::new();
                    let mut response_role: Option<String> = None;
                    let mut finish_reason: Option<String> = None;
                    // Accumulate tool_calls by index (streaming chunks may contain partial tool_calls)
                    let mut tool_calls_map: std::collections::BTreeMap<i64, serde_json::Value> = std::collections::BTreeMap::new();

                    while let Some(chunk_result) = upstream_stream.next().await {
                        match chunk_result {
                            Ok(bytes) => {
                                // Try to parse usage and content from this chunk
                                if let Ok(text) = std::str::from_utf8(&bytes) {
                                    if let Some((p, c, t)) = parse_usage_from_chunk(text) {
                                        usage_prompt = p;
                                        usage_completion = c;
                                        usage_total = t;
                                    }
                                    // Accumulate delta content from SSE chunks
                                    for line in text.lines() {
                                        let trimmed = line.trim();
                                        if !trimmed.starts_with("data:") {
                                            continue;
                                        }
                                        let data_str = trimmed.trim_start_matches("data:").trim();
                                        if data_str == "[DONE]" || data_str.is_empty() {
                                            continue;
                                        }
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data_str) {
                                            if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                                                if let Some(choice) = choices.first() {
                                                    if let Some(delta) = choice.get("delta") {
                                                        // Accumulate regular content
                                                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                                            accumulated_content.push_str(content);
                                                        }
                                                        // Accumulate reasoning/thinking content (DeepSeek R1, OpenAI o1/o3, etc.)
                                                        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                                                            accumulated_reasoning.push_str(reasoning);
                                                        }
                                                        if response_role.is_none() {
                                                            if let Some(role) = delta.get("role").and_then(|r| r.as_str()) {
                                                                response_role = Some(role.to_string());
                                                            }
                                                        }
                                                        // Accumulate tool_calls by index
                                                        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                                                            for tc in tcs {
                                                                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                                                                let entry = tool_calls_map.entry(idx).or_insert_with(|| {
                                                                    serde_json::json!({
                                                                        "id": "",
                                                                        "type": "function",
                                                                        "function": {
                                                                            "name": "",
                                                                            "arguments": ""
                                                                        }
                                                                    })
                                                                });
                                                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                                                    if !id.is_empty() {
                                                                        entry["id"] = serde_json::json!(id);
                                                                    }
                                                                }
                                                                if let Some(t) = tc.get("type").and_then(|v| v.as_str()) {
                                                                    if !t.is_empty() {
                                                                        entry["type"] = serde_json::json!(t);
                                                                    }
                                                                }
                                                                if let Some(func) = tc.get("function") {
                                                                    if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                                                        if !name.is_empty() {
                                                                            entry["function"]["name"] = serde_json::json!(name);
                                                                        }
                                                                    }
                                                                    if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                                                        let existing = entry["function"]["arguments"].as_str().unwrap_or("");
                                                                        entry["function"]["arguments"] = serde_json::json!(format!("{}{}", existing, args));
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if finish_reason.is_none() {
                                                        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                                                            if !reason.is_empty() && reason != "null" {
                                                                finish_reason = Some(reason.to_string());
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                yield Ok::<_, std::io::Error>(bytes);
                            }
                            Err(e) => {
                                had_error = true;
                                let err_chunk = format!(
                                    "data: {{\"error\":{{\"message\":\"Stream connection interrupted: {}\",\"type\":\"server_error\"}}}}\n\n",
                                    e
                                );
                                yield Ok::<_, std::io::Error>(err_chunk.into_bytes().into());
                                yield Ok::<_, std::io::Error>(b"data: [DONE]\n\n".to_vec().into());
                                break;
                            }
                        }
                    }

                    // Build response_choices from accumulated streaming content
                    let has_content = !accumulated_content.is_empty() || !accumulated_reasoning.is_empty() || !tool_calls_map.is_empty();
                    let response_choices = if has_content {
                        let mut message = serde_json::json!({
                            "role": response_role.unwrap_or_else(|| "assistant".to_string()),
                        });
                        // Only include content if there is any
                        if !accumulated_content.is_empty() {
                            message["content"] = serde_json::json!(accumulated_content);
                        }
                        // Include reasoning_content if present
                        if !accumulated_reasoning.is_empty() {
                            message["reasoning_content"] = serde_json::json!(accumulated_reasoning);
                        }
                        // Include tool_calls if present
                        if !tool_calls_map.is_empty() {
                            let tcs: Vec<serde_json::Value> = tool_calls_map.into_values().collect();
                            message["tool_calls"] = serde_json::json!(tcs);
                        }
                        Some(serde_json::to_string(&vec![serde_json::json!({
                            "index": 0,
                            "message": message,
                            "finish_reason": finish_reason,
                        })]).unwrap_or_default())
                    } else {
                        None
                    };

                    // Log after stream completes
                    let quota_to_add = usage_total;
                    let key_id_for_quota = api_key_id_clone.clone();
                    let log = crate::db::models::RequestLog {
                        id: crate::utils::id::new_id(),
                        seq: None,
                        api_key_id: Some(api_key_id_clone),
                        api_key_name: Some(api_key_name_clone),
                        channel_id: Some(channel_id),
                        channel_name: Some(channel_name),
                        model: model_clone.clone(),
                        upstream_model: Some(upstream_model_clone),
                        mode: "chat".to_string(),
                        status_code: if had_error { 502 } else { 200 },
                        prompt_tokens: usage_prompt,
                        completion_tokens: usage_completion,
                        total_tokens: usage_total,
                        duration_ms: start.elapsed().as_millis() as i64,
                        error_message: if had_error { Some("Stream interrupted".to_string()) } else { None },
                        is_stream: 1,
                        is_retry,
                        created_at: crate::utils::time::now_iso(),
                        request_body: Some(request_body_clone),
                        response_choices,
                        risk_level: security_result_clone.risk_level.as_str().to_string(),
                        risk_score: security_result_clone.risk_score as i64,
                        risk_summary: Some(security_result_clone.summary.clone()),
                        security_action: security_result_clone.action.as_str().to_string(),
                        sanitized: if security_result_clone.sanitized { 1 } else { 0 },
                        blocked_reason: security_result_clone.blocked_reason.clone(),
                        trace_id: trace_id_clone,
                    };
                    let log_id = log.id.clone();
                    if let Err(e) = repo_clone.create_log(&log).await { eprintln!("[WARN] create_log failed: {}", e); }
                    if let Err(e) = repo_clone.create_security_findings(&log_id, &security_result_clone.findings, security_result_clone.action.as_str()).await { eprintln!("[WARN] create_security_findings failed: {}", e); }

                    // Increment quota if we got token counts
                    if quota_to_add > 0 {
                        if let Err(e) = repo_clone.increment_quota(&key_id_for_quota, quota_to_add).await { eprintln!("[WARN] increment_quota failed: {}", e); }
                    }
                };

                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(passthrough_stream))
                    .unwrap();
            }
            Err(e) => {
                let error_message = e.to_string();
                let log = crate::db::models::RequestLog {
                    id: crate::utils::id::new_id(),
                    seq: None,
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: Some(api_key_name.clone()),
                    channel_id: Some(channel.id.clone()),
                    channel_name: Some(channel.name.clone()),
                    model: model.clone(),
                    upstream_model: Some(upstream_model.clone()),
                    mode: "chat".to_string(),
                    status_code: 502,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    duration_ms: 0,
                    error_message: Some(error_message.clone()),
                    is_stream: 1,
                    is_retry: if attempt > 0 { 1 } else { 0 },
                    created_at: crate::utils::time::now_iso(),
                    request_body: Some(request_body.clone()),
                    response_choices: None,
                    risk_level: security_result.risk_level.as_str().to_string(),
                    risk_score: security_result.risk_score as i64,
                    risk_summary: Some(security_result.summary.clone()),
                    security_action: security_result.action.as_str().to_string(),
                    sanitized: if security_result.sanitized { 1 } else { 0 },
                    blocked_reason: security_result.blocked_reason.clone(),
                    trace_id: trace_id.clone(),
                };
                let log_id = log.id.clone();
                if let Err(e) = repo.create_log(&log).await {
                    eprintln!("[WARN] create_log failed: {}", e);
                }
                if let Err(e) = repo
                    .create_security_findings(
                        &log_id,
                        &security_result.findings,
                        security_result.action.as_str(),
                    )
                    .await
                {
                    eprintln!("[WARN] create_security_findings failed: {}", e);
                }
                last_error = Some(format!("{}: {}", channel.name, error_message));
            }
        }
    }

    let err_body = serde_json::json!({
        "error": {
            "message": format!(
                "All stream channels failed for model {} after {} attempt(s): {}",
                model,
                max_attempts,
                last_error.unwrap_or_else(|| "unknown upstream error".to_string())
            ),
            "type": "upstream_error"
        }
    });
    (StatusCode::BAD_GATEWAY, Json(err_body)).into_response()
}

// ─── Anthropic Messages API: POST /v1/messages ─────────────────────────────
// Accepts Anthropic-format requests and proxies to upstream channels.
// For Claude-type channels: forward natively (Anthropic format).
// For other channels: convert Anthropic → OpenAI → upstream → OpenAI → Anthropic.

fn anthropic_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({"type":"error", "error":{"type":kind, "message":message.into()}})),
    )
        .into_response()
}

/// Resolve a model name through the mapping: supports both single string and array of strings.
/// If mapped to an array, picks a random model (load balancing).
/// Returns the original model if no mapping exists.
fn resolve_mapped_model(mapping: &serde_json::Value, model: &str) -> String {
    if let Some(mapped) = mapping.get(model) {
        if let Some(s) = mapped.as_str() {
            return s.to_string();
        } else if let Some(arr) = mapped.as_array() {
            let models: Vec<String> = arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if !models.is_empty() {
                let idx = rand::Rng::random_range(&mut rand::rng(), 0..models.len());
                return models[idx].clone();
            }
        }
    }
    model.to_string()
}

fn mapped_anthropic_body(
    body: &serde_json::Value,
    mapping: &serde_json::Value,
) -> serde_json::Value {
    let mut body = body.clone();
    if let Some(model) = body.get("model").and_then(|value| value.as_str()) {
        let resolved = resolve_mapped_model(mapping, model);
        if &resolved != model {
            body["model"] = serde_json::Value::String(resolved);
        }
    }
    body
}

fn is_native_anthropic_channel(channel_type: &str) -> bool {
    channel_type == "claude"
}

fn is_unsafe_proxy_header(name: &str) -> bool {
    // RFC 9110 hop-by-hop fields and credentials belonging to the *client*
    // must never cross the gateway boundary.  Everything else is deliberately
    // forwarded so future Anthropic end-to-end headers keep working.
    matches!(name,
        "authorization" | "proxy-authorization" | "x-api-key" | "cookie" | "set-cookie"
            | "host" | "connection" | "keep-alive" | "proxy-authenticate" | "te"
            | "trailer" | "transfer-encoding" | "upgrade" | "content-length" | "content-type"
            | "expect" | "wali-trace-id"
    )
}

fn forwarded_anthropic_headers(
    headers: &HeaderMap,
) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            (!is_unsafe_proxy_header(name.as_str())).then(|| (name.clone(), value.clone()))
        })
        .collect()
}

fn valuable_anthropic_response_headers(
    headers: &reqwest::header::HeaderMap,
) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            (!is_unsafe_proxy_header(name.as_str())).then(|| (name.clone(), value.clone()))
        })
        .collect()
}

async fn native_anthropic_request(
    config: &crate::adaptor::ChannelConfig,
    headers: &HeaderMap,
    body: &serde_json::Value,
    count_tokens: bool,
    query: Option<&str>,
) -> Result<reqwest::Response, reqwest::Error> {
    let path = if count_tokens {
        "messages/count_tokens"
    } else {
        "messages"
    };
    let url = native_anthropic_url(config, path, query);
    let mut request = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
        .post(url)
        .header("x-api-key", &config.api_key)
        .header("content-type", "application/json");
    for (name, value) in forwarded_anthropic_headers(headers) {
        request = request.header(name, value);
    }
    request
        .json(&mapped_anthropic_body(body, &config.model_mapping))
        .send()
        .await
}

fn native_anthropic_url(config: &crate::adaptor::ChannelConfig, path: &str, query: Option<&str>) -> String {
    let mut url = format!("{}/{}", config.base_url.trim_end_matches('/'), path);
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    url
}

async fn openai_messages_request(
    config: &crate::adaptor::ChannelConfig,
    body: &serde_json::Value,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
        .post(url)
        .bearer_auth(&config.api_key)
        .header("content-type", "application/json")
        .json(&crate::adaptor::openai::apply_model_mapping(
            body,
            &config.model_mapping,
        ))
        .send()
        .await
}

#[derive(Clone)]
struct StreamLogContext {
    repo: std::sync::Arc<Repository>,
    key: crate::db::models::ApiKey,
    channel: crate::db::models::Channel,
    model: String,
    request: serde_json::Value,
    security: security::SecurityScanResult,
    is_stream: bool,
}

const MAX_NATIVE_SSE_RECORD_BYTES: usize = 64 * 1024;

/// Incrementally extracts the cumulative usage fields from a native Anthropic
/// SSE stream.  It deliberately retains at most one bounded record rather
/// than every byte forwarded to the client.
#[derive(Default)]
struct NativeSseUsageParser {
    pending: Vec<u8>,
    input: Option<i64>,
    output: Option<i64>,
    stopped: bool,
    malformed_or_oversized: bool,
}

impl NativeSseUsageParser {
    fn feed(&mut self, bytes: &[u8]) {
        if self.malformed_or_oversized { return; }
        self.pending.extend_from_slice(bytes);
        while let Some(end) = sse_record_end(&self.pending) {
            let record: Vec<u8> = self.pending.drain(..end).collect();
            self.consume_record(&record);
        }
        if self.pending.len() > MAX_NATIVE_SSE_RECORD_BYTES {
            self.pending.clear();
            self.malformed_or_oversized = true;
        }
    }

    fn consume_record(&mut self, record: &[u8]) {
        let Ok(text) = std::str::from_utf8(record) else { return; };
        let data = text.lines()
            .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:").map(|value| value.trim_start()))
            .collect::<Vec<_>>().join("\n");
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else { return; };
        match value.get("type").and_then(|value| value.as_str()) {
            Some("message_start") => self.input = value.pointer("/message/usage").map(anthropic_input_usage),
            Some("message_delta") => self.output = value.pointer("/usage/output_tokens").and_then(|value| value.as_i64()),
            Some("message_stop") => self.stopped = true,
            _ => {}
        }
    }

    fn finish(self) -> Option<(i64, i64)> {
        (!self.malformed_or_oversized && self.stopped)
            .then(|| (self.input.unwrap_or(0), self.output.unwrap_or(0)))
    }
}

fn sse_record_end(input: &[u8]) -> Option<usize> {
    let crlf = input.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4);
    let lf = input.windows(2).position(|window| window == b"\n\n").map(|index| index + 2);
    match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(crlf.min(lf)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

fn native_usage(bytes: &[u8], is_sse: bool) -> Option<(i64, i64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !is_sse {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let usage = value.get("usage")?;
        return Some((anthropic_input_usage(usage), usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0)));
    }
    let mut parser = NativeSseUsageParser::default();
    parser.feed(text.as_bytes());
    parser.finish()
}

fn anthropic_input_usage(usage: &serde_json::Value) -> i64 {
    usage.get("input_tokens").and_then(|value| value.as_i64()).unwrap_or(0)
        + usage.get("cache_creation_input_tokens").and_then(|value| value.as_i64()).unwrap_or(0)
        + usage.get("cache_read_input_tokens").and_then(|value| value.as_i64()).unwrap_or(0)
}

fn native_response(response: reqwest::Response, accounting: Option<StreamLogContext>) -> Response {
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = valuable_anthropic_response_headers(response.headers());
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type.as_ref().and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("text/event-stream"));
    let upstream = response.bytes_stream();
    let stream = async_stream::stream! {
        tokio::pin!(upstream);
        let mut usage_parser = NativeSseUsageParser::default();
        // A non-streaming Messages response is a small JSON object in normal
        // operation.  Keep a hard cap for accounting so a malicious upstream
        // can never turn the proxy into an unbounded collector.
        let mut non_sse_observed = Vec::new();
        let mut completed = true;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    if is_sse {
                        usage_parser.feed(&bytes);
                    } else if non_sse_observed.len().saturating_add(bytes.len()) <= MAX_NATIVE_SSE_RECORD_BYTES {
                        non_sse_observed.extend_from_slice(&bytes);
                    }
                    yield Ok::<_, std::io::Error>(bytes);
                }
                Err(error) => { completed = false; yield Err::<bytes::Bytes, _>(std::io::Error::other(error)); break; }
            }
        }
        if let Some(context) = accounting {
            if completed {
                let usage = if is_sse { usage_parser.finish() } else { native_usage(&non_sse_observed, false) };
                if let Some(usage) = usage {
                    record_anthropic_success(context.repo, &context.key, &context.channel, &context.model, &context.request, &context.security, context.is_stream, Some(usage)).await;
                }
            }
        }
    };
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
        anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "Unable to proxy native Anthropic response",
        )
    })
}

struct StoredNativeError {
    status: StatusCode,
    content_type: Option<axum::http::HeaderValue>,
    headers: Vec<(axum::http::HeaderName, axum::http::HeaderValue)>,
    body: bytes::Bytes,
}

async fn store_native_error(response: reqwest::Response) -> StoredNativeError {
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let headers = valuable_anthropic_response_headers(response.headers());
    let body = response.bytes().await.unwrap_or_default();
    StoredNativeError { status, content_type, headers, body }
}

fn stored_native_response(error: StoredNativeError) -> Response {
    let mut builder = Response::builder().status(error.status);
    if let Some(content_type) = error.content_type { builder = builder.header(header::CONTENT_TYPE, content_type); }
    for (name, value) in error.headers { builder = builder.header(name, value); }
    builder.body(Body::from(error.body)).unwrap_or_else(|_| anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "Unable to proxy native Anthropic response"))
}

fn retryable_upstream_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 529) || status.is_server_error()
}

fn openai_error_response(status: StatusCode, message: &str, headers: &reqwest::header::HeaderMap) -> Response {
    // Upstream credentials belong to the gateway. Do not report an upstream
    // 401/403 as though the Claude Code caller supplied a bad local key.
    let (downstream_status, kind) = match status.as_u16() {
        429 => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
        400 | 404 | 408 | 409 | 422 => (status, "invalid_request_error"),
        _ => (StatusCode::BAD_GATEWAY, "api_error"),
    };
    let mut response = anthropic_error(downstream_status, kind, message);
    if let Some(retry_after) = headers.get("retry-after") {
        response.headers_mut().insert(header::RETRY_AFTER, retry_after.clone());
    }
    response
}

fn sanitized_anthropic_log_body(request: &serde_json::Value) -> (Option<String>, bool) {
    let redacted = security::redact::redact_json_for_logging(request);
    let sanitized = redacted != *request;
    (serde_json::to_string(&redacted).ok(), sanitized)
}

async fn record_anthropic_outcome(
    repo: std::sync::Arc<Repository>,
    key: &crate::db::models::ApiKey,
    channel: Option<&crate::db::models::Channel>,
    model: &str,
    request: &serde_json::Value,
    security_result: &security::SecurityScanResult,
    is_stream: bool,
    status_code: i64,
    error_message: Option<String>,
    usage: Option<(i64, i64)>,
) {
    let (prompt_tokens, completion_tokens) = usage.unwrap_or((0, 0));
    let total_tokens = prompt_tokens + completion_tokens;
    let (request_body, log_sanitized) = sanitized_anthropic_log_body(request);
    let log = crate::db::models::RequestLog {
        id: crate::utils::id::new_id(), seq: None, api_key_id: Some(key.id.clone()), api_key_name: Some(key.name.clone()),
        channel_id: channel.map(|channel| channel.id.clone()), channel_name: channel.map(|channel| channel.name.clone()), model: model.to_string(), upstream_model: None,
        mode: "anthropic".to_string(), status_code, prompt_tokens, completion_tokens, total_tokens, duration_ms: 0,
        error_message, is_stream: i64::from(is_stream), is_retry: 0, created_at: crate::utils::time::now_iso(),
        request_body, response_choices: None, risk_level: security_result.risk_level.as_str().to_string(), risk_score: security_result.risk_score as i64,
        risk_summary: Some(security_result.summary.clone()), security_action: security_result.action.as_str().to_string(), sanitized: i64::from(log_sanitized || security_result.sanitized), blocked_reason: security_result.blocked_reason.clone(), trace_id: None,
    };
    let log_id = log.id.clone();
    let _ = repo.create_log(&log).await;
    let _ = repo.create_security_findings(&log_id, &security_result.findings, security_result.action.as_str()).await;
    if total_tokens > 0 { let _ = repo.increment_quota(&key.id, total_tokens).await; }
}

async fn record_anthropic_success(repo: std::sync::Arc<Repository>, key: &crate::db::models::ApiKey, channel: &crate::db::models::Channel, model: &str, request: &serde_json::Value, security_result: &security::SecurityScanResult, is_stream: bool, usage: Option<(i64, i64)>) {
    record_anthropic_outcome(repo, key, Some(channel), model, request, security_result, is_stream, 200, None, usage).await;
}

/// Anthropic Messages compatibility endpoint.
///
/// Channel selection is performed before any format conversion. Claude channels
/// receive the original request and native response bytes; every other channel
/// is the explicit OpenAI Chat Completions compatibility path.
pub async fn handle_messages(
    State(shared): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid JSON: {error}"),
            )
        }
    };
    let api_key = match protocol::extract_api_key(&headers) {
        Some(key) => key,
        None => {
            return anthropic_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Missing API key",
            )
        }
    };
    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));
    let key = match repo.get_api_key_by_key(&api_key).await {
        Ok(key) => key,
        Err(_) => {
            return anthropic_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Invalid API key",
            )
        }
    };
    if key.quota_limit > 0 && key.quota_used >= key.quota_limit {
        return anthropic_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "Quota exceeded",
        );
    }
    let model = match json
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
    {
        Some(model) => model.to_string(),
        None => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model is required",
            )
        }
    };
    let stream = json
        .get("stream")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let security_settings = security::get_security_settings(&shared.app);
    let mut security_result = security::scan_request(&json, &security_settings);
    if matches!(security_result.action, security::SecurityAction::Block) {
        record_anthropic_outcome(repo.clone(), &key, None, &model, &json, &security_result, stream, 451, security_result.blocked_reason.clone(), None).await;
        return anthropic_error(StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, "api_error", security_result.summary);
    }
    let (forward_json, was_redacted) = if matches!(security_result.action, security::SecurityAction::Redact) || security_settings.redact_secrets {
        security::redact_request_body(&json, &security_settings)
    } else { (json.clone(), false) };
    security_result.sanitized |= was_redacted;
    let channels = match repo.get_enabled_channels().await {
        Ok(channels) => channels,
        Err(_) => {
            return anthropic_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "No channels available",
            )
        }
    };
    let mut selected = Dispatcher::select_channels(&channels, &model);
    // A native channel preserves all current and future Anthropic features, so
    // prefer it before entering the intentionally smaller Chat codec.
    selected.sort_by_key(|channel| !is_native_anthropic_channel(&channel.channel_type));
    if selected.is_empty() {
        return anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            format!("No channel for model: {model}"),
        );
    }
    let (retry_enabled, retry_times) = proxy::get_retry_settings(&shared.app);
    let max_attempts = if retry_enabled {
        (retry_times.max(0) as usize + 1).min(selected.len())
    } else {
        1
    };
    let mut last_error = "unknown upstream error".to_string();
    let mut last_native_error = None;
    let mut last_openai_error: Option<(StatusCode, String, reqwest::header::HeaderMap)> = None;
    let mut upstream_attempts = 0usize;

    for channel in selected {
        let config = Dispatcher::channel_to_config(&channel);
        if is_native_anthropic_channel(&channel.channel_type) {
            if upstream_attempts >= max_attempts { break; }
            upstream_attempts += 1;
            match native_anthropic_request(&config, &headers, &forward_json, false, uri.query()).await {
                Ok(response) if response.status().is_success() => {
                    return native_response(response, Some(StreamLogContext { repo: repo.clone(), key: key.clone(), channel: channel.clone(), model: model.clone(), request: json.clone(), security: security_result.clone(), is_stream: stream }))
                },
                Ok(response) => {
                    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                    if !retryable_upstream_status(status) {
                        record_anthropic_outcome(repo.clone(), &key, Some(&channel), &model, &json, &security_result, stream, status.as_u16() as i64, Some(format!("Native upstream returned HTTP {status}")), None).await;
                        return native_response(response, None);
                    }
                    last_error = format!("{}: HTTP {}", channel.name, status);
                    last_native_error = Some(store_native_error(response).await);
                }
                Err(error) => {
                    last_error = format!("{}: {error}", channel.name);
                    record_anthropic_outcome(repo.clone(), &key, Some(&channel), &model, &json, &security_result, stream, 502, Some(last_error.clone()), None).await;
                },
            }
            continue;
        }

        let openai_body = match protocol::anthropic_to_openai(&forward_json) {
            Ok(value) => value,
            Err(message) => {
                last_error = format!("{}: incompatible with OpenAI Chat Completions: {message}", channel.name);
                continue;
            }
        };
        if upstream_attempts >= max_attempts { break; }
        upstream_attempts += 1;
        match openai_messages_request(&config, &openai_body).await {
            Ok(response) if response.status().is_success() && stream => {
                return openai_sse_response(response, &model, StreamLogContext { repo: repo.clone(), key: key.clone(), channel: channel.clone(), model: model.clone(), request: json.clone(), security: security_result.clone(), is_stream: true })
            }
            Ok(response) if response.status().is_success() => {
                let body: serde_json::Value = match response.json().await {
                    Ok(value) => value,
                    Err(error) => {
                        last_error = format!("{}: {error}", channel.name);
                        // A successful HTTP response that cannot be decoded is
                        // not an upstream attempt for failover purposes.
                        upstream_attempts = upstream_attempts.saturating_sub(1);
                        continue;
                    }
                };
                return match protocol::openai_to_anthropic(&body, &model) {
                    Ok(value) => {
                        let usage = Some((body.pointer("/usage/prompt_tokens").and_then(|value| value.as_i64()).unwrap_or(0), body.pointer("/usage/completion_tokens").and_then(|value| value.as_i64()).unwrap_or(0)));
                        record_anthropic_success(repo.clone(), &key, &channel, &model, &json, &security_result, false, usage).await;
                        (StatusCode::OK, Json(value)).into_response()
                    },
                    Err(message) => {
                        // A 200 transport response is not a usable channel if
                        // its tool arguments/content cannot satisfy Messages.
                        last_error = format!("{}: conversion failed: {message}", channel.name);
                        record_anthropic_outcome(repo.clone(), &key, Some(&channel), &model, &json, &security_result, false, 502, Some(message), None).await;
                        upstream_attempts = upstream_attempts.saturating_sub(1);
                        continue;
                    },
                };
            }
            Ok(response) => {
                let status = StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let response_headers = response.headers().clone();
                let upstream: serde_json::Value =
                    response.json().await.unwrap_or(serde_json::Value::Null);
                let message = upstream
                    .pointer("/error/message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("OpenAI Chat Completions upstream rejected the request");
                last_error = format!("{}: {message}", channel.name);
                if !retryable_upstream_status(status) {
                    record_anthropic_outcome(repo.clone(), &key, Some(&channel), &model, &json, &security_result, stream, status.as_u16() as i64, Some(last_error.clone()), None).await;
                    return openai_error_response(status, message, &response_headers);
                }
                last_openai_error = Some((status, message.to_string(), response_headers));
            }
            Err(error) => {
                last_error = format!("{}: {error}", channel.name);
                record_anthropic_outcome(repo.clone(), &key, Some(&channel), &model, &json, &security_result, stream, 502, Some(last_error.clone()), None).await;
            },
        }
    }
    if let Some((status, message, headers)) = last_openai_error {
        return openai_error_response(status, &message, &headers);
    }
    if let Some(response) = last_native_error { return stored_native_response(response); }
    if last_error.contains("incompatible with OpenAI Chat Completions") {
        record_anthropic_outcome(repo.clone(), &key, None, &model, &json, &security_result, stream, 400, Some(last_error.clone()), None).await;
        return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", last_error);
    }
    record_anthropic_outcome(repo.clone(), &key, None, &model, &json, &security_result, stream, 502, Some(last_error.clone()), None).await;
    anthropic_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("All channels failed for model {model}: {last_error}"),
    )
}

fn openai_sse_response(response: reqwest::Response, model: &str, accounting: StreamLogContext) -> Response {
    let model = model.to_string();
    let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let upstream = response.bytes_stream();
    let stream = async_stream::stream! {
        tokio::pin!(upstream);
        let mut state = crate::protocol::anthropic::AnthropicStreamState::default();
        let mut failed = false;
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => match state.feed(&bytes, &model, &message_id) {
                    Ok(events) => for event in events { yield Ok::<_, std::io::Error>(bytes::Bytes::from(event.into_bytes())); },
                    Err(message) => {
                        failed = true;
                        record_anthropic_outcome(accounting.repo.clone(), &accounting.key, Some(&accounting.channel), &accounting.model, &accounting.request, &accounting.security, true, 502, Some(format!("OpenAI stream conversion failed: {message}")), None).await;
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":{}}}}}\n\n", serde_json::to_string(&message).unwrap()).into_bytes()));
                        break;
                    }
                },
                Err(error) => {
                    failed = true;
                    let message = format!("OpenAI stream interrupted: {error}");
                    record_anthropic_outcome(accounting.repo.clone(), &accounting.key, Some(&accounting.channel), &accounting.model, &accounting.request, &accounting.security, true, 502, Some(message.clone()), None).await;
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":{}}}}}\n\n", serde_json::to_string(&message).unwrap()).into_bytes()));
                    break;
                }
            }
        }
        if !failed {
            match state.finish(&model, &message_id) {
                Ok(events) => {
                    for event in events { yield Ok::<_, std::io::Error>(bytes::Bytes::from(event.into_bytes())); }
                    let usage = state.usage();
                    record_anthropic_success(accounting.repo, &accounting.key, &accounting.channel, &accounting.model, &accounting.request, &accounting.security, true, Some(usage)).await;
                },
                Err(message) => {
                    record_anthropic_outcome(accounting.repo.clone(), &accounting.key, Some(&accounting.channel), &accounting.model, &accounting.request, &accounting.security, true, 502, Some(format!("OpenAI stream conversion failed: {message}")), None).await;
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":{}}}}}\n\n", serde_json::to_string(&message).unwrap()).into_bytes()))
                },
            }
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

#[cfg(test)]
mod anthropic_handler_tests {
    use super::*;

    #[test]
    fn native_forwarding_keeps_anthropic_headers_and_only_maps_model() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert("anthropic-beta", "prompt-caching".parse().unwrap());
        headers.insert("x-api-key", "local-only-key".parse().unwrap());
        headers.insert("authorization", "Bearer caller-secret".parse().unwrap());
        headers.insert("cookie", "session=caller-secret".parse().unwrap());
        headers.insert("x-anthropic-future-feature", "on".parse().unwrap());
        let kept = forwarded_anthropic_headers(&headers);
        assert!(kept.iter().any(|(name, _)| name == "anthropic-version"));
        assert!(kept.iter().any(|(name, _)| name == "anthropic-beta"));
        assert!(kept.iter().any(|(name, _)| name == "x-anthropic-future-feature"));
        assert!(!kept.iter().any(|(name, _)| name == "x-api-key" || name == "authorization" || name == "cookie"));
        let body = serde_json::json!({"model":"public-model", "system":[{"type":"thinking"}], "messages":[]});
        let mapped =
            mapped_anthropic_body(&body, &serde_json::json!({"public-model":"upstream-model"}));
        assert_eq!(mapped["model"], "upstream-model");
        assert_eq!(mapped["system"], body["system"]);
        assert!(is_native_anthropic_channel("claude"));
    }

    #[test]
    fn mapped_anthropic_body_supports_array_mapping() {
        let body = serde_json::json!({"model":"auto", "messages":[]});
        let mapping = serde_json::json!({"auto":["model-a", "model-b"]});
        let mapped = mapped_anthropic_body(&body, &mapping);
        let result = mapped["model"].as_str().unwrap();
        assert!(result == "model-a" || result == "model-b");
    }

    #[tokio::test]
    async fn maps_openai_rate_limits_without_exposing_channel_auth_as_client_auth() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "17".parse().unwrap());
        let rate_limited = openai_error_response(StatusCode::TOO_MANY_REQUESTS, "slow down", &headers);
        assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rate_limited.headers()[header::RETRY_AFTER], "17");
        let rate_body = axum::body::to_bytes(rate_limited.into_body(), usize::MAX).await.unwrap();
        assert!(std::str::from_utf8(&rate_body).unwrap().contains("rate_limit_error"));

        let auth_failed = openai_error_response(StatusCode::UNAUTHORIZED, "upstream key rejected", &reqwest::header::HeaderMap::new());
        assert_eq!(auth_failed.status(), StatusCode::BAD_GATEWAY);
        let auth_body = axum::body::to_bytes(auth_failed.into_body(), usize::MAX).await.unwrap();
        assert!(std::str::from_utf8(&auth_body).unwrap().contains("api_error"));
    }

    #[test]
    fn reads_native_message_and_sse_usage_without_changing_payload() {
        assert_eq!(native_usage(br#"{"usage":{"input_tokens":12,"output_tokens":4}}"#, false), Some((12, 4)));
        let sse = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":3}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        assert_eq!(native_usage(sse, true), Some((17, 4)));
        assert_eq!(native_usage(&sse[..sse.len() - 48], true), None);

        let mut incremental = NativeSseUsageParser::default();
        for piece in sse.chunks(7) { incremental.feed(piece); }
        assert_eq!(incremental.finish(), Some((17, 4)));
        let mut oversized = NativeSseUsageParser::default();
        oversized.feed(&vec![b'x'; MAX_NATIVE_SSE_RECORD_BYTES + 1]);
        assert!(oversized.finish().is_none());
    }

    #[test]
    fn preserves_query_beta_for_both_native_message_paths() {
        let config = crate::adaptor::ChannelConfig {
            base_url: "https://upstream.example/v1/".to_string(), api_key: "key".to_string(), models: vec![], model_mapping: serde_json::json!({}), extra: serde_json::json!({}), timeout_secs: 60,
        };
        assert_eq!(native_anthropic_url(&config, "messages", Some("beta=true")), "https://upstream.example/v1/messages?beta=true");
        assert_eq!(native_anthropic_url(&config, "messages/count_tokens", Some("beta=true")), "https://upstream.example/v1/messages/count_tokens?beta=true");
    }

    #[test]
    fn always_redacts_log_body_even_when_forwarding_redaction_is_off() {
        let request = serde_json::json!({"messages":[{"role":"user","content":"sk-abcdefghijklmnopqrstuvwx123456"}]});
        let (body, sanitized) = sanitized_anthropic_log_body(&request);
        assert!(sanitized);
        assert!(!body.unwrap().contains("abcdefghijklmnopqrstuvwx"));
    }
}

/// Claude Code calls this endpoint while constructing context.  Exact counts
/// are only available from a native Anthropic channel; returning characters/4
/// would falsely advertise precision.
pub async fn handle_messages_count_tokens(
    State(shared): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid JSON: {error}"),
            )
        }
    };
    let api_key = match protocol::extract_api_key(&headers) {
        Some(key) => key,
        None => {
            return anthropic_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Missing API key",
            )
        }
    };
    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));
    let key = match repo.get_api_key_by_key(&api_key).await {
        Ok(key) => key,
        Err(_) => return anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", "Invalid API key"),
    };
    let model = match json.get("model").and_then(|value| value.as_str()) {
        Some(model) => model,
        None => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model is required",
            )
        }
    };
    let security_settings = security::get_security_settings(&shared.app);
    let security_result = security::scan_request(&json, &security_settings);
    if matches!(security_result.action, security::SecurityAction::Block) {
        record_anthropic_outcome(repo.clone(), &key, None, model, &json, &security_result, false, 451, security_result.blocked_reason.clone(), None).await;
        return anthropic_error(StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, "api_error", security_result.summary);
    }
    let channels = match repo.get_enabled_channels().await {
        Ok(channels) => channels,
        Err(_) => {
            return anthropic_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "No channels available",
            )
        }
    };
    let native_channels: Vec<_> = Dispatcher::select_channels(&channels, model).into_iter().filter(|channel| is_native_anthropic_channel(&channel.channel_type)).collect();
    let (retry_enabled, retry_times) = proxy::get_retry_settings(&shared.app);
    let max_attempts = if retry_enabled { (retry_times.max(0) as usize + 1).min(native_channels.len()) } else { native_channels.len().min(1) };
    let mut last_error = None;
    for channel in native_channels.into_iter().take(max_attempts) {
        let config = Dispatcher::channel_to_config(&channel);
        match native_anthropic_request(&config, &headers, &json, true, uri.query()).await {
            Ok(response) if response.status().is_success() => return native_response(response, None),
            Ok(response) => {
                let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                if !retryable_upstream_status(status) { return native_response(response, None); }
                last_error = Some(store_native_error(response).await);
            }
            Err(_) => continue,
        }
    }
    if let Some(response) = last_error { return stored_native_response(response); }
    record_anthropic_outcome(repo, &key, None, model, &json, &security_result, false, 501, Some("Exact Anthropic count_tokens is unavailable without a native Anthropic channel".to_string()), None).await;
    anthropic_error(StatusCode::NOT_IMPLEMENTED, "api_error", "Exact Anthropic count_tokens requires a native Anthropic Messages channel")
}

pub async fn handle_messages_legacy(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body);
    let json: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(j) => j,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)).into_response(),
    };

    let is_stream = json
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // Extract API key from x-api-key header or Authorization Bearer
    let api_key = match protocol::extract_api_key(&headers) {
        Some(k) => k,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "type": "error",
                    "error": {"type": "authentication_error", "message": "Missing API key"}
                })),
            )
                .into_response();
        }
    };

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));
    let key_record = match repo.get_api_key_by_key(&api_key).await {
        Ok(k) => k,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "type": "error",
                    "error": {"type": "authentication_error", "message": "Invalid API key"}
                })),
            )
                .into_response()
        }
    };

    if key_record.quota_limit > 0 && key_record.quota_used >= key_record.quota_limit {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "Quota exceeded"}
            })),
        )
            .into_response();
    }

    let trace_id = headers
        .get("Wali-Trace-Id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let request_body_str = serde_json::to_string(&json).unwrap_or_default();
    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // Convert Anthropic request to OpenAI format for internal proxy
    let openai_body = match protocol::anthropic_to_openai(&json) {
        Ok(value) => value,
        Err(message) => {
            return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", message)
        }
    };

    if is_stream {
        handle_messages_stream(
            shared,
            openai_body,
            model,
            key_record.id,
            key_record.name,
            request_body_str,
            trace_id,
        )
        .await
    } else {
        match proxy::handle_request(
            &repo,
            &shared.app,
            &key_record.id,
            &key_record.name,
            openai_body,
            false,
            Some(request_body_str),
            trace_id,
        )
        .await
        {
            Ok(result) => {
                // Convert OpenAI response back to Anthropic format
                match protocol::openai_to_anthropic(&result.body, &model) {
                    Ok(anthropic_resp) => (StatusCode::OK, Json(anthropic_resp)).into_response(),
                    Err(message) => anthropic_error(StatusCode::BAD_GATEWAY, "api_error", message),
                }
            }
            Err((code, msg)) => {
                let err_body = serde_json::json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": msg}
                });
                (
                    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(err_body),
                )
                    .into_response()
            }
        }
    }
}

/// Optional Anthropic endpoint used by Claude Code for context estimation.
pub async fn handle_messages_count_tokens_legacy(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "type": "error", "error": {"type": "invalid_request_error", "message": format!("Invalid JSON: {}", error)}
        }))).into_response(),
    };
    let api_key = match protocol::extract_api_key(&headers) {
        Some(key) => key,
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({
            "type": "error", "error": {"type": "authentication_error", "message": "Missing API key"}
        }))).into_response(),
    };
    let repo = Repository::new(shared.state.db.pool.clone());
    if repo.get_api_key_by_key(&api_key).await.is_err() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({
            "type": "error", "error": {"type": "authentication_error", "message": "Invalid API key"}
        }))).into_response();
    }
    Json(serde_json::json!({"input_tokens": protocol::estimate_anthropic_input_tokens(&json)}))
        .into_response()
}

/// Stream handler for Anthropic Messages API.
/// Converts OpenAI SSE stream to Anthropic SSE events.
async fn handle_messages_stream(
    shared: SharedState,
    openai_body: serde_json::Value,
    model: String,
    api_key_id: String,
    api_key_name: String,
    request_body: String,
    trace_id: Option<String>,
) -> Response {
    let security_settings = security::get_security_settings(&shared.app);
    let security_result = security::scan_request(&openai_body, &security_settings);

    let (forward_json, was_redacted) =
        if matches!(security_result.action, security::SecurityAction::Redact)
            || security_settings.redact_secrets
        {
            security::redact_request_body(&openai_body, &security_settings)
        } else {
            (openai_body.clone(), false)
        };
    let mut security_result = security_result;
    if was_redacted {
        security_result.sanitized = true;
    }

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));

    if matches!(security_result.action, security::SecurityAction::Block) {
        let log = crate::db::models::RequestLog {
            response_choices: None,
            id: crate::utils::id::new_id(),
            seq: None,
            api_key_id: Some(api_key_id.clone()),
            api_key_name: Some(api_key_name.clone()),
            channel_id: None,
            channel_name: None,
            model: model.clone(),
            upstream_model: None,
            mode: "anthropic".to_string(),
            status_code: 451,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            duration_ms: 0,
            error_message: security_result.blocked_reason.clone(),
            is_stream: 1,
            is_retry: 0,
            created_at: crate::utils::time::now_iso(),
            request_body: Some(request_body),
            risk_level: security_result.risk_level.as_str().to_string(),
            risk_score: security_result.risk_score as i64,
            risk_summary: Some(security_result.summary.clone()),
            security_action: security_result.action.as_str().to_string(),
            sanitized: if security_result.sanitized { 1 } else { 0 },
            blocked_reason: security_result.blocked_reason.clone(),
            trace_id: trace_id.clone(),
        };
        let log_id = log.id.clone();
        if let Err(e) = repo.create_log(&log).await {
            eprintln!("[WARN] create_log failed: {}", e);
        }
        if let Err(e) = repo
            .create_security_findings(
                &log_id,
                &security_result.findings,
                security_result.action.as_str(),
            )
            .await
        {
            eprintln!("[WARN] create_security_findings failed: {}", e);
        }
        let err_body = serde_json::json!({"type": "error", "error": {"type": "api_error", "message": security_result.summary}});
        return (StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, Json(err_body)).into_response();
    }

    let channels = match repo.get_enabled_channels().await {
        Ok(c) => c,
        Err(_) => return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "type": "error", "error": {"type": "api_error", "message": "No channels available"}
            })),
        )
            .into_response(),
    };

    let selected_channels = Dispatcher::select_channels(&channels, &model);
    if selected_channels.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "type": "error", "error": {"type": "api_error", "message": format!("No channel for model: {}", model)}
        }))).into_response();
    }

    let request = ProxyRequest {
        model: model.clone(),
        body: forward_json.clone(),
        stream: true,
    };
    let (retry_enabled, retry_times) = proxy::get_retry_settings(&shared.app);
    let max_attempts = if retry_enabled {
        (retry_times.max(0) as usize + 1).min(selected_channels.len())
    } else {
        1
    };

    let mut last_error = None;

    for (attempt, channel) in selected_channels.into_iter().take(max_attempts).enumerate() {
        let config = Dispatcher::channel_to_config(&channel);
        let adaptor = get_adaptor(&channel.channel_type);
        let upstream_model = resolve_mapped_model(&config.model_mapping, &model);

        match adaptor.forward_stream(&request, &config).await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body_str = resp.text().await.unwrap_or_default();
                    last_error = Some(format!("{}: {}", channel.name, body_str));
                    continue;
                }

                let start = std::time::Instant::now();
                let channel_id = channel.id.clone();
                let channel_name = channel.name.clone();
                let repo_clone = repo.clone();
                let api_key_id_clone = api_key_id.clone();
                let api_key_name_clone = api_key_name.clone();
                let model_clone = model.clone();
                let upstream_model_clone = upstream_model.clone();
                let request_body_clone = request_body.clone();
                let security_result_clone = security_result.clone();
                let trace_id_clone = trace_id.clone();
                let is_retry = if attempt > 0 { 1 } else { 0 };

                let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
                let upstream_stream = resp.bytes_stream();

                let passthrough_stream = async_stream::stream! {
                    tokio::pin!(upstream_stream);

                    let mut state = crate::protocol::anthropic::AnthropicStreamState::default();
                    let mut usage_prompt: i64 = 0;
                    let mut usage_completion: i64 = 0;
                    let mut usage_total: i64 = 0;
                    let mut had_error = false;
                    let mut accumulated_content = String::new();

                    while let Some(chunk_result) = upstream_stream.next().await {
                        match chunk_result {
                            Ok(bytes) => {
                                if let Ok(text) = std::str::from_utf8(&bytes) {
                                    if let Some((p, c, t)) = crate::protocol::anthropic::parse_usage_from_sse_chunk(text) {
                                        usage_prompt = p;
                                        usage_completion = c;
                                        usage_total = t;
                                    }
                                    // Accumulate content for logging
                                    for line in text.lines() {
                                        let trimmed = line.trim();
                                        if !trimmed.starts_with("data:") { continue; }
                                        let data_str = trimmed.trim_start_matches("data:").trim();
                                        if data_str == "[DONE]" || data_str.is_empty() { continue; }
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data_str) {
                                            if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                                                if let Some(choice) = choices.first() {
                                                    if let Some(delta) = choice.get("delta") {
                                                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                                            accumulated_content.push_str(content);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Convert OpenAI SSE → Anthropic SSE events
                                    let events = crate::protocol::anthropic::convert_openai_sse_to_anthropic(
                                        text, &model_clone, &message_id, &mut state
                                    );
                                    for event in events {
                                        yield Ok::<_, std::io::Error>(event.into_bytes().into());
                                    }
                                } else {
                                    yield Ok::<_, std::io::Error>(bytes);
                                }
                            }
                            Err(e) => {
                                had_error = true;
                                let err_event = format!(
                                    "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"Stream interrupted: {}\"}}}}\n\n",
                                    e
                                );
                                yield Ok::<_, std::io::Error>(err_event.into_bytes().into());
                                break;
                            }
                        }
                    }

                    // Build response_choices for logging
                    let response_choices = if !accumulated_content.is_empty() {
                        Some(serde_json::to_string(&vec![serde_json::json!({
                            "index": 0,
                            "message": {"role": "assistant", "content": accumulated_content},
                            "finish_reason": "stop",
                        })]).unwrap_or_default())
                    } else { None };

                    let log = crate::db::models::RequestLog {
                        id: crate::utils::id::new_id(),
                        seq: None,
                        api_key_id: Some(api_key_id_clone.clone()),
                        api_key_name: Some(api_key_name_clone.clone()),
                        channel_id: Some(channel_id),
                        channel_name: Some(channel_name),
                        model: model_clone.clone(),
                        upstream_model: Some(upstream_model_clone),
                        mode: "anthropic".to_string(),
                        status_code: if had_error { 502 } else { 200 },
                        prompt_tokens: usage_prompt,
                        completion_tokens: usage_completion,
                        total_tokens: usage_total,
                        duration_ms: start.elapsed().as_millis() as i64,
                        error_message: if had_error { Some("Stream interrupted".to_string()) } else { None },
                        is_stream: 1,
                        is_retry,
                        created_at: crate::utils::time::now_iso(),
                        request_body: Some(request_body_clone),
                        response_choices,
                        risk_level: security_result_clone.risk_level.as_str().to_string(),
                        risk_score: security_result_clone.risk_score as i64,
                        risk_summary: Some(security_result_clone.summary.clone()),
                        security_action: security_result_clone.action.as_str().to_string(),
                        sanitized: if security_result_clone.sanitized { 1 } else { 0 },
                        blocked_reason: security_result_clone.blocked_reason.clone(),
                        trace_id: trace_id_clone,
                    };
                    let log_id = log.id.clone();
                    if let Err(e) = repo_clone.create_log(&log).await { eprintln!("[WARN] create_log failed: {}", e); }
                    if let Err(e) = repo_clone.create_security_findings(&log_id, &security_result_clone.findings, security_result_clone.action.as_str()).await { eprintln!("[WARN] create_security_findings failed: {}", e); }
                    if usage_total > 0 {
                        if let Err(e) = repo_clone.increment_quota(&api_key_id_clone, usage_total).await { eprintln!("[WARN] increment_quota failed: {}", e); }
                    }
                };

                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(passthrough_stream))
                    .unwrap();
            }
            Err(e) => {
                let error_message = e.to_string();
                let log = crate::db::models::RequestLog {
                    id: crate::utils::id::new_id(),
                    seq: None,
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: Some(api_key_name.clone()),
                    channel_id: Some(channel.id.clone()),
                    channel_name: Some(channel.name.clone()),
                    model: model.clone(),
                    upstream_model: Some(upstream_model.clone()),
                    mode: "anthropic".to_string(),
                    status_code: 502,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    duration_ms: 0,
                    error_message: Some(error_message.clone()),
                    is_stream: 1,
                    is_retry: if attempt > 0 { 1 } else { 0 },
                    created_at: crate::utils::time::now_iso(),
                    request_body: Some(request_body.clone()),
                    response_choices: None,
                    risk_level: security_result.risk_level.as_str().to_string(),
                    risk_score: security_result.risk_score as i64,
                    risk_summary: Some(security_result.summary.clone()),
                    security_action: security_result.action.as_str().to_string(),
                    sanitized: if security_result.sanitized { 1 } else { 0 },
                    blocked_reason: security_result.blocked_reason.clone(),
                    trace_id: trace_id.clone(),
                };
                let log_id = log.id.clone();
                if let Err(e) = repo.create_log(&log).await {
                    eprintln!("[WARN] create_log failed: {}", e);
                }
                if let Err(e) = repo
                    .create_security_findings(
                        &log_id,
                        &security_result.findings,
                        security_result.action.as_str(),
                    )
                    .await
                {
                    eprintln!("[WARN] create_security_findings failed: {}", e);
                }
                last_error = Some(format!("{}: {}", channel.name, error_message));
            }
        }
    }

    let err_body = serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": format!("All channels failed for model {} after {} attempt(s): {}", model, max_attempts, last_error.unwrap_or_else(|| "unknown".to_string()))}
    });
    (StatusCode::BAD_GATEWAY, Json(err_body)).into_response()
}

// ─── OpenAI Responses API: POST /v1/responses ────────────────────────────────
// Accepts Responses API format and proxies to upstream channels via Chat Completions.
// Converts: Responses input → OpenAI messages → upstream → OpenAI response → Responses output.

pub async fn handle_responses(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body);
    let json: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(j) => j,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)).into_response(),
    };

    let is_stream = json
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let api_key = match protocol::extract_api_key(&headers) {
        Some(k) => k,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": {"message": "Missing API key", "type": "authentication_error"}
                })),
            )
                .into_response()
        }
    };

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));
    let key_record = match repo.get_api_key_by_key(&api_key).await {
        Ok(k) => k,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": {"message": "Invalid API key", "type": "authentication_error"}
                })),
            )
                .into_response()
        }
    };

    if key_record.quota_limit > 0 && key_record.quota_used >= key_record.quota_limit {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": {"message": "Quota exceeded", "type": "rate_limit_error"}
            })),
        )
            .into_response();
    }

    let trace_id = headers
        .get("Wali-Trace-Id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let request_body_str = serde_json::to_string(&json).unwrap_or_default();
    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // Convert Responses API request to OpenAI Chat Completions format
    let openai_body = protocol::responses_to_openai(&json);

    if is_stream {
        handle_responses_stream(
            shared,
            openai_body,
            model,
            key_record.id,
            key_record.name,
            request_body_str,
            trace_id,
        )
        .await
    } else {
        match proxy::handle_request(
            &repo,
            &shared.app,
            &key_record.id,
            &key_record.name,
            openai_body,
            false,
            Some(request_body_str),
            trace_id,
        )
        .await
        {
            Ok(result) => {
                // Convert OpenAI response to Responses API format
                let responses_resp = protocol::openai_to_responses(&result.body, &model);
                (StatusCode::OK, Json(responses_resp)).into_response()
            }
            Err((code, msg)) => {
                let err_body = serde_json::json!({
                    "error": {"message": msg, "type": "upstream_error", "code": code}
                });
                (
                    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(err_body),
                )
                    .into_response()
            }
        }
    }
}

/// Stream handler for Responses API.
/// Converts OpenAI SSE stream to Responses API SSE events.
async fn handle_responses_stream(
    shared: SharedState,
    openai_body: serde_json::Value,
    model: String,
    api_key_id: String,
    api_key_name: String,
    request_body: String,
    trace_id: Option<String>,
) -> Response {
    let security_settings = security::get_security_settings(&shared.app);
    let security_result = security::scan_request(&openai_body, &security_settings);

    let (forward_json, was_redacted) =
        if matches!(security_result.action, security::SecurityAction::Redact)
            || security_settings.redact_secrets
        {
            security::redact_request_body(&openai_body, &security_settings)
        } else {
            (openai_body.clone(), false)
        };
    let mut security_result = security_result;
    if was_redacted {
        security_result.sanitized = true;
    }

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));

    if matches!(security_result.action, security::SecurityAction::Block) {
        let log = crate::db::models::RequestLog {
            response_choices: None,
            id: crate::utils::id::new_id(),
            seq: None,
            api_key_id: Some(api_key_id.clone()),
            api_key_name: Some(api_key_name.clone()),
            channel_id: None,
            channel_name: None,
            model: model.clone(),
            upstream_model: None,
            mode: "responses".to_string(),
            status_code: 451,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            duration_ms: 0,
            error_message: security_result.blocked_reason.clone(),
            is_stream: 1,
            is_retry: 0,
            created_at: crate::utils::time::now_iso(),
            request_body: Some(request_body),
            risk_level: security_result.risk_level.as_str().to_string(),
            risk_score: security_result.risk_score as i64,
            risk_summary: Some(security_result.summary.clone()),
            security_action: security_result.action.as_str().to_string(),
            sanitized: if security_result.sanitized { 1 } else { 0 },
            blocked_reason: security_result.blocked_reason.clone(),
            trace_id: trace_id.clone(),
        };
        let log_id = log.id.clone();
        if let Err(e) = repo.create_log(&log).await {
            eprintln!("[WARN] create_log failed: {}", e);
        }
        if let Err(e) = repo
            .create_security_findings(
                &log_id,
                &security_result.findings,
                security_result.action.as_str(),
            )
            .await
        {
            eprintln!("[WARN] create_security_findings failed: {}", e);
        }
        let err_body = serde_json::json!({"error": {"message": security_result.summary, "type": "security_blocked"}});
        return (StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, Json(err_body)).into_response();
    }

    let channels = match repo.get_enabled_channels().await {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "No channels available").into_response()
        }
    };

    let selected_channels = Dispatcher::select_channels(&channels, &model);
    if selected_channels.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "No channel for model").into_response();
    }

    let request = ProxyRequest {
        model: model.clone(),
        body: forward_json.clone(),
        stream: true,
    };
    let (retry_enabled, retry_times) = proxy::get_retry_settings(&shared.app);
    let max_attempts = if retry_enabled {
        (retry_times.max(0) as usize + 1).min(selected_channels.len())
    } else {
        1
    };

    let mut last_error = None;

    for (attempt, channel) in selected_channels.into_iter().take(max_attempts).enumerate() {
        let config = Dispatcher::channel_to_config(&channel);
        let adaptor = get_adaptor(&channel.channel_type);
        let upstream_model = resolve_mapped_model(&config.model_mapping, &model);

        match adaptor.forward_stream(&request, &config).await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body_str = resp.text().await.unwrap_or_default();
                    last_error = Some(format!("{}: {}", channel.name, body_str));
                    continue;
                }

                let start = std::time::Instant::now();
                let channel_id = channel.id.clone();
                let channel_name = channel.name.clone();
                let repo_clone = repo.clone();
                let api_key_id_clone = api_key_id.clone();
                let api_key_name_clone = api_key_name.clone();
                let model_clone = model.clone();
                let upstream_model_clone = upstream_model.clone();
                let request_body_clone = request_body.clone();
                let security_result_clone = security_result.clone();
                let trace_id_clone = trace_id.clone();
                let is_retry = if attempt > 0 { 1 } else { 0 };

                let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
                let upstream_stream = resp.bytes_stream();

                let passthrough_stream = async_stream::stream! {
                    tokio::pin!(upstream_stream);

                    // Emit response.created event
                    let created = crate::protocol::responses::create_response_created_event(&model_clone, &response_id);
                    yield Ok::<_, std::io::Error>(created.into_bytes().into());

                    let mut usage_prompt: i64 = 0;
                    let mut usage_completion: i64 = 0;
                    let mut usage_total: i64 = 0;
                    let mut had_error = false;
                    let mut stream_state = crate::protocol::responses::StreamState::default();
                    let mut accumulated_content = String::new();
                    let mut accumulated_reasoning = String::new();

                    while let Some(chunk_result) = upstream_stream.next().await {
                        match chunk_result {
                            Ok(bytes) => {
                                if let Ok(text) = std::str::from_utf8(&bytes) {
                                    if let Some((p, c, t)) = crate::protocol::responses::parse_usage_from_sse_chunk(text) {
                                        usage_prompt = p;
                                        usage_completion = c;
                                        usage_total = t;
                                    }
                                    // Accumulate content for logging
                                    for line in text.lines() {
                                        let trimmed = line.trim();
                                        if !trimmed.starts_with("data:") { continue; }
                                        let data_str = trimmed.trim_start_matches("data:").trim();
                                        if data_str == "[DONE]" || data_str.is_empty() { continue; }
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data_str) {
                                            if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                                                if let Some(choice) = choices.first() {
                                                    if let Some(delta) = choice.get("delta") {
                                                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                                            accumulated_content.push_str(content);
                                                        }
                                                        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                                                            accumulated_reasoning.push_str(reasoning);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Convert OpenAI SSE → Responses SSE events
                                    let events = crate::protocol::responses::convert_openai_sse_to_responses(
                                        text, &model_clone, &response_id, &accumulated_content,
                                        &mut stream_state,
                                    );
                                    for event in events {
                                        yield Ok::<_, std::io::Error>(event.into_bytes().into());
                                    }
                                } else {
                                    yield Ok::<_, std::io::Error>(bytes);
                                }
                            }
                            Err(e) => {
                                had_error = true;
                                let err_event = format!(
                                    "event: response.failed\ndata: {{\"type\":\"response.failed\",\"response_id\":\"{}\",\"error\":{{\"message\":\"Stream interrupted: {}\"}}}}\n\n",
                                    response_id, e
                                );
                                yield Ok::<_, std::io::Error>(err_event.into_bytes().into());
                                break;
                            }
                        }
                    }

                    // Stream ended. Emit final response.completed with usage.
                    // (convert_openai_sse_to_responses sends everything up to output_item.done,
                    // but NOT response.completed — that's sent here with usage from the final chunk)
                    if !had_error {
                        let synth_events = crate::protocol::responses::create_synthetic_completed_events(
                            &model_clone,
                            &response_id,
                            &accumulated_content,
                            &stream_state,
                            usage_prompt,
                            usage_completion,
                        );
                        for ev in synth_events {
                            yield Ok::<_, std::io::Error>(ev.into_bytes().into());
                        }
                        // Emit [DONE] after response.completed
                        yield Ok::<_, std::io::Error>(b"data: [DONE]\n\n".to_vec().into());
                    }

                    // Build response_choices for logging
                    let response_choices = if !accumulated_content.is_empty() || !accumulated_reasoning.is_empty() {
                        let mut msg = serde_json::json!({"role": "assistant"});
                        if !accumulated_content.is_empty() {
                            msg["content"] = serde_json::json!(accumulated_content);
                        }
                        if !accumulated_reasoning.is_empty() {
                            msg["reasoning_content"] = serde_json::json!(accumulated_reasoning);
                        }
                        Some(serde_json::to_string(&vec![serde_json::json!({
                            "index": 0,
                            "message": msg,
                            "finish_reason": "stop",
                        })]).unwrap_or_default())
                    } else { None };

                    let log = crate::db::models::RequestLog {
                        id: crate::utils::id::new_id(),
                        seq: None,
                        api_key_id: Some(api_key_id_clone.clone()),
                        api_key_name: Some(api_key_name_clone.clone()),
                        channel_id: Some(channel_id),
                        channel_name: Some(channel_name),
                        model: model_clone.clone(),
                        upstream_model: Some(upstream_model_clone),
                        mode: "responses".to_string(),
                        status_code: if had_error { 502 } else { 200 },
                        prompt_tokens: usage_prompt,
                        completion_tokens: usage_completion,
                        total_tokens: usage_total,
                        duration_ms: start.elapsed().as_millis() as i64,
                        error_message: if had_error { Some("Stream interrupted".to_string()) } else { None },
                        is_stream: 1,
                        is_retry,
                        created_at: crate::utils::time::now_iso(),
                        request_body: Some(request_body_clone),
                        response_choices,
                        risk_level: security_result_clone.risk_level.as_str().to_string(),
                        risk_score: security_result_clone.risk_score as i64,
                        risk_summary: Some(security_result_clone.summary.clone()),
                        security_action: security_result_clone.action.as_str().to_string(),
                        sanitized: if security_result_clone.sanitized { 1 } else { 0 },
                        blocked_reason: security_result_clone.blocked_reason.clone(),
                        trace_id: trace_id_clone,
                    };
                    let log_id = log.id.clone();
                    if let Err(e) = repo_clone.create_log(&log).await { eprintln!("[WARN] create_log failed: {}", e); }
                    if let Err(e) = repo_clone.create_security_findings(&log_id, &security_result_clone.findings, security_result_clone.action.as_str()).await { eprintln!("[WARN] create_security_findings failed: {}", e); }
                    if usage_total > 0 {
                        if let Err(e) = repo_clone.increment_quota(&api_key_id_clone, usage_total).await { eprintln!("[WARN] increment_quota failed: {}", e); }
                    }
                };

                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(passthrough_stream))
                    .unwrap();
            }
            Err(e) => {
                let error_message = e.to_string();
                let log = crate::db::models::RequestLog {
                    id: crate::utils::id::new_id(),
                    seq: None,
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: Some(api_key_name.clone()),
                    channel_id: Some(channel.id.clone()),
                    channel_name: Some(channel.name.clone()),
                    model: model.clone(),
                    upstream_model: Some(upstream_model.clone()),
                    mode: "responses".to_string(),
                    status_code: 502,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    duration_ms: 0,
                    error_message: Some(error_message.clone()),
                    is_stream: 1,
                    is_retry: if attempt > 0 { 1 } else { 0 },
                    created_at: crate::utils::time::now_iso(),
                    request_body: Some(request_body.clone()),
                    response_choices: None,
                    risk_level: security_result.risk_level.as_str().to_string(),
                    risk_score: security_result.risk_score as i64,
                    risk_summary: Some(security_result.summary.clone()),
                    security_action: security_result.action.as_str().to_string(),
                    sanitized: if security_result.sanitized { 1 } else { 0 },
                    blocked_reason: security_result.blocked_reason.clone(),
                    trace_id: trace_id.clone(),
                };
                let log_id = log.id.clone();
                if let Err(e) = repo.create_log(&log).await {
                    eprintln!("[WARN] create_log failed: {}", e);
                }
                if let Err(e) = repo
                    .create_security_findings(
                        &log_id,
                        &security_result.findings,
                        security_result.action.as_str(),
                    )
                    .await
                {
                    eprintln!("[WARN] create_security_findings failed: {}", e);
                }
                last_error = Some(format!("{}: {}", channel.name, error_message));
            }
        }
    }

    let err_body = serde_json::json!({
        "error": {
            "message": format!("All channels failed for model {} after {} attempt(s): {}", model, max_attempts, last_error.unwrap_or_else(|| "unknown".to_string())),
            "type": "upstream_error"
        }
    });
    (StatusCode::BAD_GATEWAY, Json(err_body)).into_response()
}

pub async fn handle_completions(State(_shared): State<SharedState>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "Not implemented yet").into_response()
}

pub async fn handle_embeddings(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body);
    let json: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(j) => j,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)).into_response(),
    };

    let api_key = match protocol::extract_api_key(&headers) {
        Some(k) => k,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": {"message": "Missing API key", "type": "authentication_error"}
                })),
            )
                .into_response()
        }
    };

    let repo = std::sync::Arc::new(Repository::new(shared.state.db.pool.clone()));
    let key_record = match repo.get_api_key_by_key(&api_key).await {
        Ok(k) => k,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": {"message": "Invalid API key", "type": "authentication_error"}
                })),
            )
                .into_response()
        }
    };

    if key_record.quota_limit > 0 && key_record.quota_used >= key_record.quota_limit {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": {"message": "Quota exceeded", "type": "rate_limit_error"}
            })),
        )
            .into_response();
    }

    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let trace_id = headers
        .get("Wali-Trace-Id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let request_body_str = serde_json::to_string(&json).unwrap_or_default();

    // Security scan
    let security_settings = security::get_security_settings(&shared.app);
    let security_result = security::scan_request(&json, &security_settings);

    if matches!(security_result.action, security::SecurityAction::Block) {
        let log = crate::db::models::RequestLog {
            response_choices: None,
            id: crate::utils::id::new_id(),
            seq: None,
            api_key_id: Some(key_record.id.clone()),
            api_key_name: Some(key_record.name.clone()),
            channel_id: None,
            channel_name: None,
            model: model.clone(),
            upstream_model: None,
            mode: "embedding".to_string(),
            status_code: 451,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            duration_ms: 0,
            error_message: security_result.blocked_reason.clone(),
            is_stream: 0,
            is_retry: 0,
            created_at: crate::utils::time::now_iso(),
            request_body: Some(request_body_str),
            risk_level: security_result.risk_level.as_str().to_string(),
            risk_score: security_result.risk_score as i64,
            risk_summary: Some(security_result.summary.clone()),
            security_action: security_result.action.as_str().to_string(),
            sanitized: if security_result.sanitized { 1 } else { 0 },
            blocked_reason: security_result.blocked_reason.clone(),
            trace_id: trace_id.clone(),
        };
        let log_id = log.id.clone();
        if let Err(e) = repo.create_log(&log).await {
            eprintln!("[WARN] create_log failed: {}", e);
        }
        if let Err(e) = repo
            .create_security_findings(
                &log_id,
                &security_result.findings,
                security_result.action.as_str(),
            )
            .await
        {
            eprintln!("[WARN] create_security_findings failed: {}", e);
        }
        return (
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS,
            Json(serde_json::json!({
                "error": {"message": security_result.summary, "type": "security_blocked"}
            })),
        )
            .into_response();
    }

    // Select channels
    let channels = match repo.get_enabled_channels().await {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "No channels available").into_response()
        }
    };

    let selected_channels = Dispatcher::select_channels(&channels, &model);
    if selected_channels.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "No channel for model").into_response();
    }

    let (retry_enabled, retry_times) = proxy::get_retry_settings(&shared.app);
    let max_attempts = if retry_enabled {
        (retry_times.max(0) as usize + 1).min(selected_channels.len())
    } else {
        1
    };

    let mut last_error = None;
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            selected_channels.first()
                .map(|ch| ch.timeout_secs.max(1) as u64)
                .unwrap_or(60)
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    for (attempt, channel) in selected_channels.into_iter().take(max_attempts).enumerate() {
        let config = Dispatcher::channel_to_config(&channel);
        let upstream_model = resolve_mapped_model(&config.model_mapping, &model);

        // Build upstream embedding request — send directly to /embeddings
        // (adaptor.forward() hard-codes /chat/completions which doesn't work for embeddings)
        let base_url = config.base_url.trim_end_matches('/');
        let embed_url = format!("{}/embeddings", base_url);
        let embed_body = serde_json::json!({
            "model": upstream_model,
            "input": json.get("input").cloned().unwrap_or(serde_json::Value::Null),
            "encoding_format": "float"
        });

        let result = client
            .post(&embed_url)
            .header("Authorization", format!("Bearer {}", config.api_key))
            .header("Content-Type", "application/json")
            .json(&embed_body)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                let resp_body: serde_json::Value =
                    resp.json().await.unwrap_or(serde_json::Value::Null);

                if !status.is_success() {
                    let error_message = format!(
                        "HTTP {}: {}",
                        status,
                        serde_json::to_string(&resp_body)
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    );
                    let log = crate::db::models::RequestLog {
                        id: crate::utils::id::new_id(),
                        seq: None,
                        api_key_id: Some(key_record.id.clone()),
                        api_key_name: Some(key_record.name.clone()),
                        channel_id: Some(channel.id.clone()),
                        channel_name: Some(channel.name.clone()),
                        model: model.clone(),
                        upstream_model: Some(upstream_model.clone()),
                        mode: "embedding".to_string(),
                        status_code: status.as_u16() as i64,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        duration_ms: start.elapsed().as_millis() as i64,
                        error_message: Some(error_message.clone()),
                        is_stream: 0,
                        is_retry: if attempt > 0 { 1 } else { 0 },
                        created_at: crate::utils::time::now_iso(),
                        request_body: Some(request_body_str.clone()),
                        response_choices: None,
                        risk_level: security_result.risk_level.as_str().to_string(),
                        risk_score: security_result.risk_score as i64,
                        risk_summary: Some(security_result.summary.clone()),
                        security_action: security_result.action.as_str().to_string(),
                        sanitized: if security_result.sanitized { 1 } else { 0 },
                        blocked_reason: security_result.blocked_reason.clone(),
                        trace_id: trace_id.clone(),
                    };
                    let log_id = log.id.clone();
                    if let Err(e) = repo.create_log(&log).await {
                        eprintln!("[WARN] create_log failed: {}", e);
                    }
                    if let Err(e) = repo
                        .create_security_findings(
                            &log_id,
                            &security_result.findings,
                            security_result.action.as_str(),
                        )
                        .await
                    {
                        eprintln!("[WARN] create_security_findings failed: {}", e);
                    }
                    last_error = Some(error_message);
                    continue;
                }

                // Extract usage from response
                let usage_total = resp_body
                    .get("usage")
                    .and_then(|u| u.get("total_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0) as i64;
                let usage_prompt = resp_body
                    .get("usage")
                    .and_then(|u| u.get("prompt_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0) as i64;

                let log = crate::db::models::RequestLog {
                    id: crate::utils::id::new_id(),
                    seq: None,
                    api_key_id: Some(key_record.id.clone()),
                    api_key_name: Some(key_record.name.clone()),
                    channel_id: Some(channel.id.clone()),
                    channel_name: Some(channel.name.clone()),
                    model: model.clone(),
                    upstream_model: Some(upstream_model.clone()),
                    mode: "embedding".to_string(),
                    status_code: status.as_u16() as i64,
                    prompt_tokens: usage_prompt,
                    completion_tokens: 0,
                    total_tokens: usage_total,
                    duration_ms: start.elapsed().as_millis() as i64,
                    error_message: None,
                    is_stream: 0,
                    is_retry: if attempt > 0 { 1 } else { 0 },
                    created_at: crate::utils::time::now_iso(),
                    request_body: Some(request_body_str.clone()),
                    response_choices: None,
                    risk_level: security_result.risk_level.as_str().to_string(),
                    risk_score: security_result.risk_score as i64,
                    risk_summary: Some(security_result.summary.clone()),
                    security_action: security_result.action.as_str().to_string(),
                    sanitized: if security_result.sanitized { 1 } else { 0 },
                    blocked_reason: security_result.blocked_reason.clone(),
                    trace_id: trace_id.clone(),
                };
                let log_id = log.id.clone();
                if let Err(e) = repo.create_log(&log).await {
                    eprintln!("[WARN] create_log failed: {}", e);
                }
                if let Err(e) = repo
                    .create_security_findings(
                        &log_id,
                        &security_result.findings,
                        security_result.action.as_str(),
                    )
                    .await
                {
                    eprintln!("[WARN] create_security_findings failed: {}", e);
                }
                if usage_total > 0 {
                    if let Err(e) = repo.increment_quota(&key_record.id, usage_total).await {
                        eprintln!("[WARN] increment_quota failed: {}", e);
                    }
                }

                return (StatusCode::OK, Json(resp_body)).into_response();
            }
            Err(e) => {
                let error_message = e.to_string();
                let log = crate::db::models::RequestLog {
                    id: crate::utils::id::new_id(),
                    seq: None,
                    api_key_id: Some(key_record.id.clone()),
                    api_key_name: Some(key_record.name.clone()),
                    channel_id: Some(channel.id.clone()),
                    channel_name: Some(channel.name.clone()),
                    model: model.clone(),
                    upstream_model: Some(upstream_model.clone()),
                    mode: "embedding".to_string(),
                    status_code: 502,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    duration_ms: start.elapsed().as_millis() as i64,
                    error_message: Some(error_message.clone()),
                    is_stream: 0,
                    is_retry: if attempt > 0 { 1 } else { 0 },
                    created_at: crate::utils::time::now_iso(),
                    request_body: Some(request_body_str.clone()),
                    response_choices: None,
                    risk_level: security_result.risk_level.as_str().to_string(),
                    risk_score: security_result.risk_score as i64,
                    risk_summary: Some(security_result.summary.clone()),
                    security_action: security_result.action.as_str().to_string(),
                    sanitized: if security_result.sanitized { 1 } else { 0 },
                    blocked_reason: security_result.blocked_reason.clone(),
                    trace_id: trace_id.clone(),
                };
                let log_id = log.id.clone();
                if let Err(e) = repo.create_log(&log).await {
                    eprintln!("[WARN] create_log failed: {}", e);
                }
                if let Err(e) = repo
                    .create_security_findings(
                        &log_id,
                        &security_result.findings,
                        security_result.action.as_str(),
                    )
                    .await
                {
                    eprintln!("[WARN] create_security_findings failed: {}", e);
                }
                last_error = Some(error_message);
            }
        }
    }

    let err_body = serde_json::json!({
        "error": {
            "message": format!("All channels failed for embedding model {} after {} attempt(s): {}", model, max_attempts, last_error.unwrap_or_else(|| "unknown".to_string())),
            "type": "upstream_error"
        }
    });
    (StatusCode::BAD_GATEWAY, Json(err_body)).into_response()
}

pub async fn handle_list_models(State(shared): State<SharedState>) -> Response {
    let repo = Repository::new(shared.state.db.pool.clone());
    match repo.get_enabled_channels().await {
        Ok(channels) => {
            let mut models: Vec<serde_json::Value> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for ch in &channels {
                let ch_models: Vec<String> = serde_json::from_str(&ch.models).unwrap_or_default();
                for m in ch_models {
                    if seen.insert(m.clone()) {
                        models.push(serde_json::json!({
                            "id": m, "object": "model",
                            "created": chrono::Utc::now().timestamp(),
                            "owned_by": ch.channel_type,
                        }));
                    }
                }
                // Also expose mapped model names (mapping keys)
                let mapping: serde_json::Value = serde_json::from_str(&ch.model_mapping)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                if let Some(obj) = mapping.as_object() {
                    for key in obj.keys() {
                        if seen.insert(key.clone()) {
                            models.push(serde_json::json!({
                                "id": key, "object": "model",
                                "created": chrono::Utc::now().timestamp(),
                                "owned_by": ch.channel_type,
                            }));
                        }
                    }
                }
            }
            Json(serde_json::json!({ "object": "list", "data": models })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response(),
    }
}

pub async fn handle_images(State(_shared): State<SharedState>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "Not implemented yet").into_response()
}

pub async fn handle_audio_transcriptions(State(_shared): State<SharedState>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "Not implemented yet").into_response()
}

pub async fn handle_audio_speech(State(_shared): State<SharedState>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "Not implemented yet").into_response()
}

pub async fn handle_health(State(shared): State<SharedState>) -> Response {
    let port = shared.state.server_port.read().await.clone();
    let running = shared
        .state
        .server_running
        .load(std::sync::atomic::Ordering::SeqCst);
    Json(serde_json::json!({
        "status": "ok",
        "running": running,
        "port": port,
        "url": format!("http://127.0.0.1:{}", port),
    }))
    .into_response()
}
