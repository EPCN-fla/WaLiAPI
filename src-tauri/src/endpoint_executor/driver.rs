//! Streaming plan driver (T06).
//!
//! Ties the T05 [`RoutePlan`] / [`AttemptFlow`] to the transport executors and
//! the pure [`StreamPumpCore`] commit barrier, and writes the RequestLog +
//! quota accounting on the facade path (T05 handoff #3).
//!
//! Flow per attempt:
//! ```text
//! connect → dispatch_stream_executor → (2xx) → read+buffer first SSE record →
//! validate → FirstFrameBufferedAndValidated → commit_downstream →
//! begin_streaming → pump bytes → complete | abort
//! ```
//! Pre-commit failures (connect / first-frame invalid / 4xx-5xx) run back through
//! [`AttemptFlow`] so the next candidate may be tried; post-commit errors only
//! emit a protocol-representable error, never a retry.

use crate::core::attempt::{
    build_prepared_attempt, AttemptFailure, AttemptFlow, FailureClass, FlowStep, PreparedAttempt,
};
use crate::core::channel_identity::{resolve_channel_identity, ChannelIdentityRow};
use crate::core::route_plan::{RouteCandidate, RoutePlan};
use crate::db::models::{ApiKey, Channel, RequestLog};
use crate::db::repository::Repository;
use crate::endpoint_executor::sse::{decoder_for, SseMode, StreamPumpCore};
use crate::endpoint_executor::{
    dispatch_auth_account_executor, dispatch_auth_account_stream_executor, dispatch_executor,
    dispatch_stream_executor, StreamAttemptResult, UpstreamStream,
};
use crate::security::gate::AuditedRequest;
use crate::utils;
use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rand::SeedableRng;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Map an attempt to its streaming transform mode.
///
/// The label names the REQUEST encoding direction (`chat_to_messages_v1` =
/// downstream Chat request encoded as an upstream Messages request), but the
/// SSE mode names the RESPONSE decoding direction (what the upstream SSE must
/// be transformed INTO for the downstream client).  The two are inverse, so the
/// mapping is deliberately crossed:
///   * `chat_to_messages_v1`  → upstream Messages SSE → downstream Chat SSE  (`MessagesToChat`)
///   * `messages_to_chat_v1`  → upstream Chat SSE     → downstream Messages SSE (`ChatToMessages`)
fn sse_mode_for(attempt: &PreparedAttempt) -> SseMode {
    if attempt.upstream_type == "auth_account" {
        return match attempt.codec_version.as_deref() {
            Some("chat_to_responses_v1") => SseMode::ResponsesToChat,
            Some("messages_to_responses_v1") => SseMode::ResponsesToMessages,
            _ => SseMode::Native,
        };
    }
    match attempt.codec_version.as_deref() {
        Some("chat_to_messages_v1") => SseMode::MessagesToChat,
        Some("messages_to_chat_v1") => SseMode::ChatToMessages,
        Some("responses_via_chat_v1") => SseMode::ResponsesViaChat,
        _ => SseMode::Native,
    }
}

fn candidate_lookup(plan: &RoutePlan) -> HashMap<String, RouteCandidate> {
    plan.groups
        .iter()
        .flat_map(|group| group.candidates.iter())
        .map(|candidate| {
            (
                candidate.candidate.id().to_owned(),
                candidate.candidate.clone(),
            )
        })
        .collect()
}

fn missing_candidate_failure(candidate_id: &str) -> AttemptFailure {
    AttemptFailure {
        failure_class: FailureClass::CallerTerminal,
        message: format!("route plan candidate lookup failed for {candidate_id}"),
        status_code: Some(500),
        retry_after: None,
    }
}

fn plan_error_response(status: u16, message: impl Into<String>) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (
        code,
        axum::Json(json!({
            "error": { "message": message.into(), "type": "route_plan_error", "code": code.as_u16() }
        })),
    )
        .into_response()
}

/// Candidate context retained for failures that exhaust a streaming plan before
/// the downstream stream commits. `FlowStep::Halt` only carries the terminal
/// error, so the driver must retain this separately for observability.
#[derive(Clone)]
struct StreamFailureMeta {
    channel_id: String,
    channel_name: String,
    upstream_type: String,
    route_group: String,
    upstream_protocol: String,
    upstream_endpoint: String,
    upstream_model: String,
    provider: String,
    identity_revision: i64,
    codec_version: Option<String>,
}

/// Run a NON-STREAM plan to a complete Response, writing RequestLog + quota.
///
/// `safe_headers` are the already-filtered request headers to forward.
///
/// All 8 parameters are distinct immutable inputs threaded from the T06 handler
/// seam; factoring them into a struct would ripple through `handlers.rs` (a
/// frozen interface) for no functional gain, so the lint is scoped here.
#[allow(clippy::too_many_arguments)]
pub async fn route_plan_response(
    plan: RoutePlan,
    audited: &AuditedRequest,
    key: &ApiKey,
    safe_headers: &[(String, String)],
    mode: &str,
    repo: &Arc<Repository>,
    sanitized_log_body: &str,
    trace_id: Option<String>,
) -> Response {
    let auth_service = Arc::new(crate::auth_provider::service::AuthService::new(
        repo.clone(),
        crate::auth_provider::ProviderRegistry::new(),
    ));
    route_plan_response_with_auth_service(
        plan,
        audited,
        key,
        safe_headers,
        mode,
        repo,
        sanitized_log_body,
        trace_id,
        auth_service,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn route_plan_response_with_auth_service(
    plan: RoutePlan,
    audited: &AuditedRequest,
    key: &ApiKey,
    safe_headers: &[(String, String)],
    mode: &str,
    repo: &Arc<Repository>,
    sanitized_log_body: &str,
    trace_id: Option<String>,
    auth_service: Arc<crate::auth_provider::service::AuthService>,
) -> Response {
    let lookup = candidate_lookup(&plan);
    let endpoint = plan.endpoint;
    let query = audited.envelope.query.clone();
    let started = Instant::now();
    let execution = crate::core::plan_executor::execute_plan(
        plan,
        audited,
        rand::rngs::StdRng::from_os_rng(),
        |attempt| {
            let safe = safe_headers.to_vec();
            let query = query.clone();
            let candidate = lookup.get(&attempt.channel_id).cloned();
            let auth_service = auth_service.clone();
            // Clone the attempt so the returned future does not borrow it
            // (execute_plan requires a `'static`-capable executor future).
            let attempt = attempt.clone();
            async move {
                match candidate {
                    Some(RouteCandidate::Channel { channel, identity }) => {
                        dispatch_executor(
                            endpoint,
                            &attempt,
                            &channel,
                            &identity,
                            &safe,
                            query.as_deref(),
                        )
                        .await
                    }
                    Some(RouteCandidate::AuthAccount(_)) => {
                        dispatch_auth_account_executor(endpoint, &attempt, &auth_service, &safe)
                            .await
                    }
                    None => crate::core::attempt::AttemptResult::Failure(
                        missing_candidate_failure(&attempt.channel_id),
                    ),
                }
            }
        },
    )
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;

    write_non_stream_log(
        repo,
        key,
        audited,
        mode,
        &execution,
        duration_ms,
        sanitized_log_body,
        trace_id,
    )
    .await;

    let code = StatusCode::from_u16(execution.status).unwrap_or(StatusCode::BAD_GATEWAY);
    // T06 M-2: forward safely-passthrough upstream response headers (e.g.
    // anthropic-ratelimit-*) on the non-stream facade path.
    let mut builder = axum::response::Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in &execution.response_headers {
        if name.eq_ignore_ascii_case("content-type")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Body::from(
            serde_json::to_string(&execution.body).unwrap_or_default(),
        ))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": {"message": "response build failed"}})),
            )
                .into_response()
        })
}

/// Write the RequestLog + quota for a non-stream facade execution.
#[allow(clippy::too_many_arguments)]
async fn write_non_stream_log(
    repo: &Arc<Repository>,
    key: &ApiKey,
    audited: &AuditedRequest,
    mode: &str,
    execution: &crate::core::plan_executor::PlanExecution,
    duration_ms: u64,
    sanitized_log_body: &str,
    trace_id: Option<String>,
) {
    let usage = execution.usage.as_ref().map(|u| {
        (
            u.prompt_tokens as i64,
            u.completion_tokens as i64,
            u.total_tokens as i64,
        )
    });
    let (mut prompt, mut completion, mut total) = usage.unwrap_or((0, 0, 0));

    // Fallback: estimate tokens locally when upstream didn't return usage.
    // Only estimate for successful (2xx) responses — errors have no real content.
    if total == 0 && prompt == 0 && completion == 0 && execution.status >= 200 && execution.status < 300 {
        let req_body: serde_json::Value = serde_json::from_str(sanitized_log_body).unwrap_or(serde_json::Value::Null);
        let resp_text = super::estimate_usage::extract_response_text(&execution.body);
        let (p, c, t) = super::estimate_usage::estimate_usage(&req_body, Some(&resp_text), &audited.envelope.model);
        prompt = p;
        completion = c;
        total = t;
        if total > 0 {
            eprintln!("[INFO] token usage estimated (upstream didn't return usage): prompt={}, completion={}, total={}", prompt, completion, total);
        }
    }

    let is_retry = execution.attempts > 1;
    let last_failure = execution.last_failure.as_ref();
    let log = RequestLog {
        id: utils::id::new_id(),
        seq: None,
        api_key_id: Some(key.id.clone()),
        api_key_name: Some(key.name.clone()),
        channel_id: execution.channel_id.clone(),
        channel_name: execution.channel_name.clone(),
        model: audited.envelope.model.clone(),
        upstream_model: execution.upstream_model.clone(),
        mode: mode.to_string(),
        status_code: execution.status as i64,
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        duration_ms: duration_ms as i64,
        error_message: last_failure.map(|f| f.message.clone()),
        is_stream: 0,
        is_retry: i64::from(is_retry),
        created_at: utils::time::now_iso(),
        request_body: Some(sanitized_log_body.to_string()),
        response_choices: None,
        risk_level: audited.audit_result.risk_level.as_str().to_string(),
        risk_score: audited.audit_result.risk_score as i64,
        risk_summary: Some(audited.audit_result.summary.clone()),
        security_action: audited.audit_result.action.as_str().to_string(),
        sanitized: i64::from(audited.audit_result.sanitized),
        blocked_reason: audited.audit_result.blocked_reason.clone(),
        trace_id: trace_id.clone(),
        // T09 observability fields we have on the facade path.  provider /
        // identity_revision / codec_version come from `PlanExecution`, which
        // the executor captures from the SAME PreparedAttempt + ChannelIdentity
        // that produced the request body (design 11.4).
        downstream_protocol: Some(audited.envelope.downstream_protocol.as_str().to_string()),
        downstream_endpoint: Some(audited.envelope.endpoint.clone()),
        route_group: execution.route_group.clone(),
        upstream_protocol: execution.upstream_protocol.clone(),
        upstream_endpoint: execution.upstream_endpoint.clone(),
        provider: execution.provider.clone(),
        codec_version: execution.codec_version.clone(),
        failure_class: last_failure.map(|f| f.failure_class.as_str().to_string()),
        identity_revision: execution.identity_revision,
        client_cancelled: Some(0),
        stream_committed: Some(0),
        upstream_type: execution
            .upstream_type
            .clone()
            .unwrap_or_else(|| "channel".to_string()),
    };
    let log_id = log.id.clone();
    if let Err(e) = repo.create_log(&log).await {
        eprintln!("[WARN] create_log failed: {}", e);
    }
    if let Err(e) = repo
        .create_security_findings(
            &log_id,
            &audited.audit_result.findings,
            audited.audit_result.action.as_str(),
        )
        .await
    {
        eprintln!("[WARN] create_security_findings failed: {}", e);
    }
    if total > 0 {
        if let Err(e) = repo.increment_quota(&key.id, total).await {
            eprintln!("[WARN] increment_quota failed: {}", e);
        }
    }
}

/// Write a RequestLog for a streaming request that failed BEFORE any downstream
/// byte was committed (I-3: all-candidates-exhausted / CallerTerminal /
/// codec rejection / authorize_and_plan rejection).  This keeps failed
/// streaming requests visible in the observability layer, matching the
/// non-stream path's coverage.
#[allow(clippy::too_many_arguments)]
pub async fn write_stream_precommit_failure_log(
    repo: &Arc<Repository>,
    key: &ApiKey,
    audited: &AuditedRequest,
    mode: &str,
    is_stream: bool,
    status: u16,
    message: &str,
    sanitized_log_body: &str,
    trace_id: Option<&str>,
) {
    write_stream_precommit_failure_log_with_meta(
        repo,
        key,
        audited,
        mode,
        is_stream,
        status,
        message,
        sanitized_log_body,
        trace_id,
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn write_stream_precommit_failure_log_with_meta(
    repo: &Arc<Repository>,
    key: &ApiKey,
    audited: &AuditedRequest,
    mode: &str,
    is_stream: bool,
    status: u16,
    message: &str,
    sanitized_log_body: &str,
    trace_id: Option<&str>,
    last_attempt: Option<&StreamFailureMeta>,
) {
    let log = RequestLog {
        id: utils::id::new_id(),
        seq: None,
        api_key_id: Some(key.id.clone()),
        api_key_name: Some(key.name.clone()),
        channel_id: last_attempt.map(|meta| meta.channel_id.clone()),
        channel_name: last_attempt.map(|meta| meta.channel_name.clone()),
        model: audited.envelope.model.clone(),
        upstream_model: last_attempt.map(|meta| meta.upstream_model.clone()),
        mode: mode.to_string(),
        status_code: status as i64,
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        duration_ms: 0,
        error_message: Some(message.to_string()),
        is_stream: i64::from(is_stream),
        is_retry: 0,
        created_at: utils::time::now_iso(),
        request_body: Some(sanitized_log_body.to_string()),
        response_choices: None,
        risk_level: audited.audit_result.risk_level.as_str().to_string(),
        risk_score: audited.audit_result.risk_score as i64,
        risk_summary: Some(audited.audit_result.summary.clone()),
        security_action: audited.audit_result.action.as_str().to_string(),
        sanitized: i64::from(audited.audit_result.sanitized),
        blocked_reason: audited.audit_result.blocked_reason.clone(),
        trace_id: trace_id.map(|s| s.to_string()),
        // A planning rejection has no candidate context. Once a candidate was
        // selected, retain it so exhausted Auth Accounts are not logged as
        // legacy channels.
        downstream_protocol: Some(audited.envelope.downstream_protocol.as_str().to_string()),
        downstream_endpoint: Some(audited.envelope.endpoint.clone()),
        route_group: last_attempt.map(|meta| meta.route_group.clone()),
        upstream_protocol: last_attempt.map(|meta| meta.upstream_protocol.clone()),
        upstream_endpoint: last_attempt.map(|meta| meta.upstream_endpoint.clone()),
        provider: last_attempt.map(|meta| meta.provider.clone()),
        codec_version: last_attempt.and_then(|meta| meta.codec_version.clone()),
        failure_class: None,
        identity_revision: last_attempt.map(|meta| meta.identity_revision),
        client_cancelled: Some(0),
        stream_committed: Some(0),
        upstream_type: last_attempt
            .map(|meta| meta.upstream_type.clone())
            .unwrap_or_else(|| "channel".to_string()),
    };
    let log_id = log.id.clone();
    if let Err(e) = repo.create_log(&log).await {
        eprintln!("[WARN] create_log failed: {}", e);
    }
    if let Err(e) = repo
        .create_security_findings(
            &log_id,
            &audited.audit_result.findings,
            audited.audit_result.action.as_str(),
        )
        .await
    {
        eprintln!("[WARN] create_security_findings failed: {}", e);
    }
}

/// Run a STREAMING plan and return a committed Response.
///
/// The returned body stream drives the commit barrier, forwards raw / converted
/// SSE bytes, and writes the RequestLog + quota when the stream completes; a
/// client disconnect is recorded exactly once via `client_cancelled`.
///
/// All 8 parameters are distinct immutable inputs threaded from the T06 handler
/// seam; factoring them into a struct would ripple through `handlers.rs` (a
/// frozen interface), so the lint is scoped here.
#[allow(clippy::too_many_arguments)]
pub async fn route_stream_plan(
    plan: RoutePlan,
    audited: &AuditedRequest,
    key: &ApiKey,
    safe_headers: &[(String, String)],
    mode: &str,
    repo: &Arc<Repository>,
    sanitized_log_body: &str,
    trace_id: Option<String>,
) -> Response {
    let auth_service = Arc::new(crate::auth_provider::service::AuthService::new(
        repo.clone(),
        crate::auth_provider::ProviderRegistry::new(),
    ));
    route_stream_plan_with_auth_service(
        plan,
        audited,
        key,
        safe_headers,
        mode,
        repo,
        sanitized_log_body,
        trace_id,
        auth_service,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn route_stream_plan_with_auth_service(
    plan: RoutePlan,
    audited: &AuditedRequest,
    key: &ApiKey,
    safe_headers: &[(String, String)],
    mode: &str,
    repo: &Arc<Repository>,
    sanitized_log_body: &str,
    trace_id: Option<String>,
    auth_service: Arc<crate::auth_provider::service::AuthService>,
) -> Response {
    let lookup = candidate_lookup(&plan);
    let endpoint = plan.endpoint;
    let mut flow = AttemptFlow::new(plan);
    let mut last_attempt_meta: Option<StreamFailureMeta> = None;

    loop {
        match flow.next_step() {
            FlowStep::Execute {
                group_idx,
                candidate_idx,
                attempt_no,
            } => {
                let attempt = {
                    let plan = flow.plan();
                    let group = &plan.groups[group_idx];
                    let candidate = &group.candidates[candidate_idx];
                    last_attempt_meta = Some(StreamFailureMeta {
                        channel_id: candidate.candidate.id().to_string(),
                        channel_name: candidate.candidate.name().to_string(),
                        upstream_type: candidate.candidate.upstream_type().to_string(),
                        route_group: group.id.clone(),
                        upstream_protocol: candidate.upstream_protocol.as_str().to_string(),
                        upstream_endpoint: candidate.upstream_endpoint.clone(),
                        // A failed construction has no PreparedAttempt yet;
                        // use the requested model until a built attempt supplies
                        // its mapped upstream model below.
                        upstream_model: audited.envelope.model.clone(),
                        provider: candidate.candidate.provider(),
                        identity_revision: candidate.candidate.identity_revision(),
                        codec_version: None,
                    });
                    build_prepared_attempt(
                        audited,
                        group,
                        candidate,
                        &mut rand::rngs::StdRng::from_os_rng(),
                        attempt_no,
                    )
                };

                let attempt = match attempt {
                    Err(f) => {
                        flow.record_failure(&f);
                        if f.failure_class == FailureClass::CallerTerminal
                            || f.failure_class == FailureClass::CommittedStreamError
                        {
                            // I-3: terminal pre-commit outcome must be logged.
                            let status = f.status_code.unwrap_or(400);
                            write_stream_precommit_failure_log_with_meta(
                                repo,
                                key,
                                audited,
                                mode,
                                true,
                                status,
                                &f.message,
                                sanitized_log_body,
                                trace_id.as_deref(),
                                last_attempt_meta.as_ref(),
                            )
                            .await;
                            return plan_error_response(status, f.message);
                        }
                        continue;
                    }
                    Ok(a) => a,
                };
                if let Some(meta) = last_attempt_meta.as_mut() {
                    meta.upstream_model = attempt.upstream_model.clone();
                    meta.codec_version = attempt.codec_version.clone();
                }
                let candidate = lookup.get(&attempt.channel_id).cloned();
                let query = audited.envelope.query.clone();

                let dispatched = match candidate {
                    Some(RouteCandidate::Channel { channel, identity }) => {
                        dispatch_stream_executor(
                            endpoint,
                            &attempt,
                            &channel,
                            &identity,
                            safe_headers,
                            query.as_deref(),
                        )
                        .await
                    }
                    Some(RouteCandidate::AuthAccount(_)) => {
                        dispatch_auth_account_stream_executor(&attempt, &auth_service, safe_headers)
                            .await
                    }
                    None => {
                        StreamAttemptResult::Failure(missing_candidate_failure(&attempt.channel_id))
                    }
                };

                match dispatched {
                    StreamAttemptResult::Failure(f) => {
                        flow.record_failure(&f);
                        if f.failure_class == FailureClass::CallerTerminal
                            || f.failure_class == FailureClass::CommittedStreamError
                        {
                            // I-3: terminal pre-commit outcome must be logged.
                            let status = f.status_code.unwrap_or(400);
                            write_stream_precommit_failure_log_with_meta(
                                repo,
                                key,
                                audited,
                                mode,
                                true,
                                status,
                                &f.message,
                                sanitized_log_body,
                                trace_id.as_deref(),
                                last_attempt_meta.as_ref(),
                            )
                            .await;
                            return plan_error_response(status, f.message);
                        }
                        continue;
                    }
                    StreamAttemptResult::Connected(mut upstream) => {
                        // --- first-frame validation (commit barrier) ---
                        let (first_frame, carry) = match buffer_first_record(&mut upstream).await {
                            Some(x) => x,
                            None => {
                                // Empty / undecodable upstream: pre-commit failover.
                                flow.record_failure(&AttemptFailure {
                                    failure_class: FailureClass::UpstreamProtocolError,
                                    message:
                                        "upstream stream ended before a valid first SSE record"
                                            .to_string(),
                                    status_code: Some(502),
                                    retry_after: None,
                                });
                                continue;
                            }
                        };

                        let mut supervisor =
                            crate::core::stream_supervisor::StreamSupervisor::new();
                        if supervisor.begin_connect().is_err() {
                            unreachable!()
                        }
                        if supervisor.on_upstream_headers().is_err() {
                            unreachable!()
                        }
                        if supervisor.on_first_frame_validated().is_err() {
                            unreachable!()
                        }

                        let sse_mode = sse_mode_for(&attempt);
                        let decoder = decoder_for(sse_mode, &attempt.upstream_model, "");
                        // C-1: conversion modes encode the first record AND the
                        // carry (records 2..N of the same upstream chunk) through
                        // the decoder BEFORE commit; a codec rejection of any
                        // first-chunk record is a pre-commit failover (never
                        // committed, never raw).
                        let pump = match StreamPumpCore::new(
                            supervisor,
                            sse_mode,
                            decoder,
                            first_frame.clone(),
                            carry.clone(),
                            attempt.upstream_model.clone(),
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                flow.record_failure(&AttemptFailure {
                                    failure_class: FailureClass::UpstreamProtocolError,
                                    message: format!(
                                        "upstream first frame could not be converted ({}): {}",
                                        sse_mode.as_str(),
                                        e.message()
                                    ),
                                    status_code: Some(502),
                                    retry_after: None,
                                });
                                continue;
                            }
                        };

                        let channel_id = attempt.channel_id.clone();
                        let channel_name = attempt.channel_name.clone();
                        let key = key.clone();
                        let audited = audited.clone();
                        let repo = repo.clone();
                        let sanitized_log_body = sanitized_log_body.to_string();
                        let trace_id = trace_id.clone();
                        // I-2: the DOWNSTREAM mode drives error formatting and the
                        // T09 log `mode` field — never the SSE transform mode.
                        let downstream_mode = mode.to_string();
                        let model = audited.envelope.model.clone();
                        let upstream_model = attempt.upstream_model.clone();
                        let is_retry = attempt_no > 1;

                        // T09 (design 11.4): the observability context comes from
                        // the SAME PreparedAttempt + ChannelIdentity that produced
                        // the request body (single source of truth).
                        let (identity_provider, identity_revision) =
                            match lookup.get(&attempt.channel_id) {
                                Some(candidate) => {
                                    (candidate.provider(), candidate.identity_revision())
                                }
                                None => ("unknown".to_string(), 0),
                            };
                        let route_group = attempt.route_group.clone();
                        let codec_version = attempt.codec_version.clone();
                        let upstream_protocol = attempt.upstream_protocol.clone();
                        let upstream_endpoint = attempt.upstream_endpoint.clone();
                        let upstream_type = attempt.upstream_type.clone();

                        // Forward the upstream content-type + safe response
                        // headers (native passthrough fidelity; design 11.1).
                        let upstream_content_type = upstream.content_type.clone();
                        let upstream_safe_headers = upstream.headers.clone();

                        let body = stream_response_body(
                            pump,
                            upstream,
                            repo,
                            key,
                            audited,
                            model,
                            upstream_model,
                            downstream_mode,
                            is_retry,
                            sanitized_log_body,
                            trace_id,
                            channel_id,
                            channel_name,
                            identity_provider,
                            identity_revision,
                            route_group,
                            codec_version,
                            upstream_protocol,
                            upstream_endpoint,
                            upstream_type,
                        );
                        let mut builder = Response::builder()
                            .status(StatusCode::OK)
                            .header(
                                header::CONTENT_TYPE,
                                if sse_mode == SseMode::Native {
                                    upstream_content_type
                                } else {
                                    "text/event-stream".to_string()
                                },
                            )
                            .header(header::CACHE_CONTROL, "no-cache")
                            .header(header::CONNECTION, "keep-alive");
                        for (name, value) in upstream_safe_headers {
                            if name.eq_ignore_ascii_case("content-type")
                                || name.eq_ignore_ascii_case("content-length")
                            {
                                continue;
                            }
                            builder = builder.header(name, value);
                        }
                        return builder
                            .body(Body::from_stream(body))
                            .expect("valid SSE response");
                    }
                }
            }
            FlowStep::Halt { status, message } => {
                // I-3: streaming pre-commit terminal outcome must be logged.
                write_stream_precommit_failure_log_with_meta(
                    repo,
                    key,
                    audited,
                    mode,
                    true,
                    status,
                    &message,
                    sanitized_log_body,
                    trace_id.as_deref(),
                    last_attempt_meta.as_ref(),
                )
                .await;
                return plan_error_response(status, message);
            }
        }
    }
}

/// Read + validate the first complete SSE record.  Returns `(first_frame,
/// carry)` where `carry` are the bytes read beyond the first record.
async fn buffer_first_record(upstream: &mut UpstreamStream) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    loop {
        // Bound the first-frame buffer (a malicious upstream must not OOM us).
        if buffer.len() > 256 * 1024 {
            return None;
        }
        if let Some(end) = crate::endpoint_executor::sse::record_end(&buffer) {
            let record = buffer[..end].to_vec();
            if crate::endpoint_executor::sse::validate_native_first_record(&record).is_ok() {
                let carry = buffer[end..].to_vec();
                return Some((record, carry));
            }
            // A full record failed validation → pre-commit failover.
            return None;
        }
        match upstream.body.next().await {
            Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
            Some(Err(_)) => return None,
            None => return None,
        }
    }
}

/// Exactly-once streaming log finalizer (T00 decision 6).
///
/// The normal stream path sets `completed` and writes the log inline; if the
/// client disconnects mid-stream the async-stream is dropped, this guard's
/// `Drop` runs, and a spawned task records a `client_cancelled` log.  The
/// `client_cancelled` marker is therefore written exactly once per request.
#[derive(Clone)]
struct StreamLogFinalizer {
    repo: Arc<Repository>,
    key: ApiKey,
    audited: AuditedRequest,
    model: String,
    upstream_model: String,
    mode: String,
    is_retry: bool,
    sanitized_log_body: String,
    trace_id: Option<String>,
    channel_id: String,
    channel_name: String,
    identity_provider: String,
    identity_revision: i64,
    route_group: String,
    codec_version: Option<String>,
    upstream_protocol: String,
    upstream_endpoint: String,
    upstream_type: String,
    started: Instant,
    completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl StreamLogFinalizer {
    async fn write(
        &self,
        client_cancelled: bool,
        had_error: bool,
        error_message: Option<&str>,
        usage_prompt: i64,
        usage_completion: i64,
        usage_total: i64,
    ) {
        let duration_ms = self.started.elapsed().as_millis() as i64;
        let log = RequestLog {
            id: utils::id::new_id(),
            seq: None,
            api_key_id: Some(self.key.id.clone()),
            api_key_name: Some(self.key.name.clone()),
            channel_id: Some(self.channel_id.clone()),
            channel_name: Some(self.channel_name.clone()),
            model: self.model.clone(),
            upstream_model: Some(self.upstream_model.clone()),
            mode: self.mode.clone(),
            // M-3: a client-cancelled row is NOT a success — use 499 so the
            // observability layer distinguishes it from a completed 200.
            status_code: if client_cancelled {
                499
            } else if had_error {
                502
            } else {
                200
            },
            prompt_tokens: usage_prompt,
            completion_tokens: usage_completion,
            total_tokens: usage_total,
            duration_ms,
            error_message: error_message.map(|s| s.to_string()),
            is_stream: 1,
            is_retry: i64::from(self.is_retry),
            created_at: utils::time::now_iso(),
            request_body: Some(self.sanitized_log_body.clone()),
            response_choices: None,
            risk_level: self.audited.audit_result.risk_level.as_str().to_string(),
            risk_score: self.audited.audit_result.risk_score as i64,
            risk_summary: Some(self.audited.audit_result.summary.clone()),
            security_action: self.audited.audit_result.action.as_str().to_string(),
            sanitized: i64::from(self.audited.audit_result.sanitized),
            blocked_reason: self.audited.audit_result.blocked_reason.clone(),
            trace_id: self.trace_id.clone(),
            // T09 observability fields (single source: PreparedAttempt + identity).
            downstream_protocol: Some(
                self.audited
                    .envelope
                    .downstream_protocol
                    .as_str()
                    .to_string(),
            ),
            downstream_endpoint: Some(self.audited.envelope.endpoint.clone()),
            route_group: Some(self.route_group.clone()),
            upstream_protocol: Some(self.upstream_protocol.clone()),
            upstream_endpoint: Some(self.upstream_endpoint.clone()),
            provider: Some(self.identity_provider.clone()),
            codec_version: self.codec_version.clone(),
            failure_class: None,
            identity_revision: Some(self.identity_revision),
            client_cancelled: Some(i64::from(client_cancelled)),
            stream_committed: Some(1),
            upstream_type: self.upstream_type.clone(),
        };
        let log_id = log.id.clone();
        if let Err(e) = self.repo.create_log(&log).await {
            eprintln!("[WARN] create_log failed: {}", e);
        }
        if let Err(e) = self
            .repo
            .create_security_findings(
                &log_id,
                &self.audited.audit_result.findings,
                self.audited.audit_result.action.as_str(),
            )
            .await
        {
            eprintln!("[WARN] create_security_findings failed: {}", e);
        }
        if usage_total > 0 {
            if let Err(e) = self.repo.increment_quota(&self.key.id, usage_total).await {
                eprintln!("[WARN] increment_quota failed: {}", e);
            }
        }
    }
}

impl Drop for StreamLogFinalizer {
    fn drop(&mut self) {
        // The normal path sets `completed` before writing the log inline.  An
        // early drop (client disconnect) lands here: record client_cancelled
        // exactly once via a spawned task (we are in Drop, so no await).
        //
        // T10 integration fix: the `completed` flag MUST be set BEFORE spawning.
        // The spawned task writes the 499 row and then DROPS its cloned
        // finalizer at task end; without setting the flag here, that drop sees
        // `completed == false` and spawns ANOTHER task, recursively — an
        // unbounded chain of duplicate 499 rows (and eventual stack overflow /
        // process abort).  Setting the flag first makes the write exactly-once.
        if !self.completed.load(std::sync::atomic::Ordering::SeqCst) {
            self.completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let f = self.clone();
            tokio::spawn(async move {
                f.write(true, false, Some("client_cancelled"), 0, 0, 0)
                    .await;
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_response_body(
    mut pump: StreamPumpCore,
    upstream: UpstreamStream,
    repo: Arc<Repository>,
    key: ApiKey,
    audited: AuditedRequest,
    model: String,
    upstream_model: String,
    mode: String,
    is_retry: bool,
    sanitized_log_body: String,
    trace_id: Option<String>,
    channel_id: String,
    channel_name: String,
    // --- T09 observability context (single source: PreparedAttempt/identity) ---
    identity_provider: String,
    identity_revision: i64,
    route_group: String,
    codec_version: Option<String>,
    upstream_protocol: String,
    upstream_endpoint: String,
    upstream_type: String,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    let mode_for_error = mode.clone();
    let finalizer = StreamLogFinalizer {
        repo,
        key,
        audited,
        model,
        upstream_model,
        mode,
        is_retry,
        sanitized_log_body,
        trace_id,
        channel_id,
        channel_name,
        identity_provider,
        identity_revision,
        route_group,
        codec_version,
        upstream_protocol,
        upstream_endpoint,
        upstream_type,
        started: Instant::now(),
        completed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let completed = finalizer.completed.clone();

    async_stream::stream! {
        let mut had_error = false;
        let mut error_message: Option<String> = None;

        let upstream_bytes = upstream.body;
        tokio::pin!(upstream_bytes);

        // Emit the first frame.  The pump already encoded the first record AND
        // any carry bytes (records 2..N of the same upstream chunk) through the
        // decoder for conversion modes — so this is ONLY downstream-protocol
        // bytes, never raw upstream bytes.  Native passthrough preserves raw.
        match pump.start() {
            Ok(first) => {
                if !first.is_empty() {
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from(first));
                }
            }
            Err(e) => {
                had_error = true;
                error_message = Some(e.message().to_string());
            }
        }

        while !had_error {
            match upstream_bytes.next().await {
                Some(Ok(bytes)) => match pump.push(&bytes) {
                    Ok(out) => {
                        if !out.is_empty() {
                            yield Ok::<_, std::io::Error>(bytes::Bytes::from(out));
                        }
                    }
                    Err(e) => {
                        had_error = true;
                        error_message = Some(e.message().to_string());
                        break;
                    }
                },
                Some(Err(e)) => {
                    had_error = true;
                    error_message = Some(format!("stream interrupted: {e}"));
                    break;
                }
                None => break,
            }
        }

        // End-of-stream flush (exactly-once terminal markers).
        if !had_error {
            match pump.finish() {
                Ok(out) => {
                    if !out.is_empty() {
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from(out));
                    }
                }
                Err(e) => {
                    had_error = true;
                    error_message = Some(e.message().to_string());
                }
            }
        }

        // A downstream error before/after commit must produce a protocol
        // error event (never a retry, never a fake success).
        if had_error {
            let msg = error_message.clone().unwrap_or_else(|| "stream error".to_string());
            let ev = format_stream_error(&mode_for_error, &msg);
            yield Ok::<_, std::io::Error>(bytes::Bytes::from(ev));
        }

        let (mut usage_prompt, mut usage_completion, mut usage_total) = pump.usage();

        // Fallback: estimate tokens locally when upstream didn't return usage.
        // Only estimate for successful streams (no error).
        if usage_total == 0 && usage_prompt == 0 && usage_completion == 0 && !had_error {
            let req_body: serde_json::Value = serde_json::from_str(&finalizer.sanitized_log_body).unwrap_or(serde_json::Value::Null);
            let resp_text = pump.accumulated_content();
            let (p, c, t) = super::estimate_usage::estimate_usage(&req_body, Some(resp_text), &finalizer.model);
            usage_prompt = p;
            usage_completion = c;
            usage_total = t;
            if usage_total > 0 {
                eprintln!("[INFO] stream token usage estimated (upstream didn't return usage): prompt={}, completion={}, total={}", usage_prompt, usage_completion, usage_total);
            }
        }

        // Mark the request completed so the Drop finalizer does NOT write a
        // duplicate client_cancelled row, then write the normal log inline.
        completed.store(true, std::sync::atomic::Ordering::SeqCst);
        finalizer
            .write(false, had_error, error_message.as_deref(), usage_prompt, usage_completion, usage_total)
            .await;
    }
}

/// Format a post-commit stream error event in the DOWNSTREAM protocol (I-2).
/// `mode` is the downstream mode ("chat" / "anthropic" / "responses" /
/// "embedding" / "anthropic_count_tokens"), NOT the SSE transform mode.
fn format_stream_error(mode: &str, message: &str) -> String {
    let msg = message.replace('"', "\\\"");
    if mode == "responses" {
        format!(
            "event: response.failed\ndata: {{\"type\":\"response.failed\",\"error\":{{\"message\":\"{}\"}}}}\n\n",
            msg
        )
    } else if mode == "anthropic" || mode == "anthropic_count_tokens" {
        format!(
            "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"{}\"}}}}\n\n",
            msg
        )
    } else {
        format!(
            "data: {{\"error\":{{\"message\":\"{}\",\"type\":\"server_error\"}}}}\n\ndata: [DONE]\n\n",
            msg
        )
    }
}

/// Resolve a channel's identity row (used by the legacy flag-off paths that
/// still key off channel_type).
fn identity_for(channel: &Channel) -> crate::core::channel_identity::ChannelIdentity {
    resolve_channel_identity(&ChannelIdentityRow::from(channel))
}

/// Whether a channel is a native Anthropic Messages channel (identity-based,
/// NOT `type == "claude"` — the removed production-selection duty).
pub fn is_native_anthropic(channel: &Channel) -> bool {
    let id = identity_for(channel);
    id.protocol == "anthropic" && id.native_endpoints.iter().any(|e| e == "messages")
}

/// Whether a channel supports the Anthropic count_tokens endpoint.
pub fn supports_count_tokens(channel: &Channel) -> bool {
    let id = identity_for(channel);
    id.protocol == "anthropic" && id.native_endpoints.iter().any(|e| e == "count_tokens")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Channel;

    fn channel(protocol: Option<&str>, endpoints: &[&str]) -> Channel {
        Channel {
            id: "ch-1".into(),
            name: "t".into(),
            channel_type: "claude".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: "k".into(),
            models: "[\"m\"]".into(),
            status: 1,
            priority: 1,
            weight: 1,
            config: "{}".into(),
            model_mapping: "{}".into(),
            timeout_secs: 30,
            protocol: protocol.map(|s| s.to_string()),
            provider: Some("anthropic".into()),
            native_base_url: Some("https://api.anthropic.com".into()),
            native_endpoints: Some(serde_json::to_string(endpoints).unwrap()),
            preset_revision: Some("test".into()),
            identity_revision: 1,
            legacy_executor_override: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_test_at: None,
            last_test_ok: None,
        }
    }

    #[test]
    fn native_anthropic_is_identity_based_not_type_based() {
        // protocol=anthropic + messages → native, even though type is "claude".
        let c = channel(Some("anthropic"), &["messages"]);
        assert!(is_native_anthropic(&c));
        // An OpenAI channel (even with type "claude" impossible) is not native.
        let o = channel(Some("openai"), &["chat_completions"]);
        assert!(!is_native_anthropic(&o));
        // A claude-typed legacy row WITHOUT a declared messages capability is
        // not native (the type==claude heuristic is removed).
        let mut legacy = channel(None, &[]);
        legacy.identity_revision = 0;
        legacy.native_base_url = None;
        legacy.native_endpoints = None;
        legacy.channel_type = "openai".into();
        assert!(!is_native_anthropic(&legacy));
    }

    #[test]
    fn count_tokens_requires_declared_capability() {
        let with = channel(Some("anthropic"), &["messages", "count_tokens"]);
        assert!(supports_count_tokens(&with));
        let without = channel(Some("anthropic"), &["messages"]);
        assert!(!supports_count_tokens(&without));
    }

    /// T06 I-4 (leader adjudication): a legacy revision-0 `type == "claude"`
    /// row infers count_tokens from the resolver, so the flag-OFF count_tokens
    /// fallback still serves it (no-regression contract).
    #[test]
    fn legacy_claude_row_serves_count_tokens() {
        let mut legacy = channel(None, &[]);
        legacy.identity_revision = 0;
        legacy.native_base_url = None;
        legacy.native_endpoints = None;
        legacy.protocol = None;
        legacy.provider = None;
        legacy.channel_type = "claude".into();
        legacy.base_url = "https://api.anthropic.com/v1".into();
        let id = identity_for(&legacy);
        assert_eq!(id.protocol, "anthropic");
        assert!(
            id.native_endpoints.iter().any(|e| e == "count_tokens"),
            "legacy claude must infer count_tokens"
        );
        assert!(
            supports_count_tokens(&legacy),
            "flag-OFF count_tokens fallback must serve legacy claude"
        );
    }

    /// C-1 DRIVER-level regression (carry seam): `buffer_first_record` →
    /// `StreamPumpCore::new` → `start()` where the FIRST upstream chunk spans
    /// MULTIPLE records (message_start + a content_block_delta carry) in a
    /// conversion mode.  The downstream must receive ONLY codec-encoded bytes
    /// for records 1 AND 2 — never raw upstream protocol bytes, and the carry
    /// record must actually be decoded (its text present in the output).
    #[tokio::test]
    async fn driver_conversion_first_frame_never_raw_downstream() {
        // One chunk containing TWO Anthropic records (downstream Chat client).
        let raw = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"model\":\"up-model\",\"content\":[]}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"carried\"}}\n\n";
        let mut raw = raw.to_vec();
        let body = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from(
            std::mem::take(&mut raw),
        ))])
        .boxed();
        let mut upstream = UpstreamStream {
            content_type: "text/event-stream".to_string(),
            headers: vec![],
            body,
        };
        let (first_frame, carry) = buffer_first_record(&mut upstream).await.unwrap();
        assert!(
            !first_frame.is_empty(),
            "driver must buffer a real first record"
        );
        assert!(
            !carry.is_empty(),
            "the first chunk must span a carry record (the real C-1 seam)"
        );

        let mut sup = crate::core::stream_supervisor::StreamSupervisor::new();
        sup.begin_connect().unwrap();
        sup.on_upstream_headers().unwrap();
        sup.on_first_frame_validated().unwrap();
        let mode = SseMode::MessagesToChat;
        let decoder = decoder_for(mode, "up-model", "");
        let mut pump = StreamPumpCore::new(
            sup,
            mode,
            decoder,
            first_frame.clone(),
            carry.clone(),
            "up-model".to_string(),
        )
        .unwrap();

        let first_out = pump.start().unwrap();
        let text = String::from_utf8_lossy(&first_out);
        assert!(
            !text.contains("event: message_start") && !text.contains("event: content_block_delta"),
            "downstream Chat client must NEVER see raw Anthropic bytes (carry included): {text}"
        );
        assert!(
            text.contains("\"content\":\"carried\""),
            "the carry record (record 2) must be decoded into downstream output: {text}"
        );
        assert!(pump.committed());

        // A subsequent chunk converts normally.
        let delta = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        let out = pump.push(delta).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("\"content\":\"hi\""));
    }

    #[test]
    fn forward_headers_drops_credentials_and_hop_by_hop() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer sk".parse().unwrap());
        headers.insert("x-api-key", "sk".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert("anthropic-beta", "prompt-caching".parse().unwrap());
        headers.insert("cookie", "a=b".parse().unwrap());
        headers.insert("x-anthropic-future", "on".parse().unwrap());
        let safe = crate::endpoint_executor::safe_request_headers(&headers);
        assert!(safe.iter().any(|(k, _)| k == "anthropic-version"));
        assert!(safe.iter().any(|(k, _)| k == "anthropic-beta"));
        assert!(safe.iter().any(|(k, _)| k == "x-anthropic-future"));
        assert!(!safe
            .iter()
            .any(|(k, _)| k == "authorization" || k == "x-api-key" || k == "cookie"));
    }

    /// I-2: post-commit stream errors must be formatted in the DOWNSTREAM
    /// protocol (Anthropic Messages / Responses / OpenAI Chat), not in the
    /// SSE transform mode string.
    #[test]
    fn stream_error_format_uses_downstream_protocol() {
        // A Messages-downstream stream error → Anthropic `event: error`.
        let anthropic = format_stream_error("anthropic", "boom");
        assert!(anthropic.contains("event: error"));
        assert!(anthropic.contains("\"type\":\"error\""));
        assert!(!anthropic.contains("data: [DONE]"));

        // A Responses-downstream stream error → `event: response.failed`.
        let responses = format_stream_error("responses", "boom");
        assert!(responses.contains("event: response.failed"));
        assert!(responses.contains("\"type\":\"response.failed\""));

        // A Chat-downstream stream error → OpenAI `data:` error + [DONE].
        let chat = format_stream_error("chat", "boom");
        assert!(chat.contains("data: {\"error\""));
        assert!(chat.contains("data: [DONE]"));

        // The SSE transform mode string must NEVER leak into error formatting.
        assert!(!format_stream_error("chat", "x").contains("chat_to_messages_v1"));
    }

    #[test]
    fn auth_account_uses_responses_sse_modes_and_lookup_failure_is_terminal() {
        let base = PreparedAttempt {
            channel_id: "account-1".into(),
            channel_name: "Codex".into(),
            upstream_type: "auth_account".into(),
            route_group: "responses_g1_native".into(),
            upstream_protocol: "responses".into(),
            upstream_endpoint: "responses".into(),
            upstream_model: "m".into(),
            native_base_url: "ignored".into(),
            codec_version: None,
            encoded_body: json!({"model":"m"}),
            conversion_report: None,
            is_retry: false,
            attempt_no: 1,
        };
        assert_eq!(sse_mode_for(&base), SseMode::Native);

        let mut chat = base.clone();
        chat.codec_version = Some("chat_to_responses_v1".into());
        assert_eq!(sse_mode_for(&chat), SseMode::ResponsesToChat);

        let mut messages = base;
        messages.codec_version = Some("messages_to_responses_v1".into());
        assert_eq!(sse_mode_for(&messages), SseMode::ResponsesToMessages);

        let failure = missing_candidate_failure("gone");
        assert_eq!(failure.status_code, Some(500));
        assert_eq!(failure.failure_class, FailureClass::CallerTerminal);
        assert!(!failure.message.is_empty());
    }

    fn now() -> String {
        crate::utils::time::now_iso()
    }

    async fn fresh_db() -> sqlx::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn audited_request() -> AuditedRequest {
        use crate::security::gate::{DownstreamProtocol, RequestEnvelope, RequestFeatures};
        use crate::security::SecurityScanResult;
        AuditedRequest {
            envelope: RequestEnvelope {
                downstream_protocol: DownstreamProtocol::ChatCompletions,
                endpoint: "chat_completions".into(),
                original_json: json!({"model": "m", "messages": []}),
                safe_forward_headers: vec![],
                query: None,
                model: "m".into(),
                stream: true,
                trace_id: None,
            },
            forward_json: json!({"model": "m", "messages": []}),
            sanitized_log_json: json!({"model": "m", "messages": []}),
            body_hash: "h".into(),
            body_len: 0,
            audit_result: SecurityScanResult::default(),
            request_features: RequestFeatures::default(),
        }
    }

    fn api_key() -> ApiKey {
        ApiKey {
            id: "key-1".into(),
            name: "t".into(),
            key: "sk-test".into(),
            status: 1,
            allowed_models: "[]".into(),
            allowed_channels: "[]".into(),
            quota_limit: 0,
            quota_used: 0,
            expires_at: None,
            created_at: now(),
            updated_at: now(),
        }
    }

    /// C-1: the FULL `stream_response_body` emission seam.  The first upstream
    /// chunk contains MULTIPLE Anthropic records (message_start first record +
    /// content_block_delta carry record).  The downstream byte stream must
    /// contain ONLY codec-encoded Chat SSE (records 1 AND 2 decoded), never raw
    /// upstream Anthropic bytes.
    #[tokio::test]
    async fn stream_response_body_carry_is_decoded_not_raw() {
        let pool = fresh_db().await;
        let repo = Arc::new(Repository::new(pool));

        // One upstream chunk: two Anthropic records (downstream Chat client).
        let chunk = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"model\":\"up-model\",\"content\":[]}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"carried\"}}\n\n";
        let mut chunk = chunk.to_vec();
        let body = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from(
            std::mem::take(&mut chunk),
        ))])
        .boxed();
        let mut upstream = UpstreamStream {
            content_type: "text/event-stream".to_string(),
            headers: vec![],
            body,
        };
        let (first_frame, carry) = buffer_first_record(&mut upstream).await.unwrap();
        assert!(
            !carry.is_empty(),
            "carry must span a second record (the real seam)"
        );

        let mut sup = crate::core::stream_supervisor::StreamSupervisor::new();
        sup.begin_connect().unwrap();
        sup.on_upstream_headers().unwrap();
        sup.on_first_frame_validated().unwrap();
        let mode = SseMode::MessagesToChat;
        let pump = StreamPumpCore::new(
            sup,
            mode,
            decoder_for(mode, "up-model", ""),
            first_frame,
            carry,
            "up-model".to_string(),
        )
        .unwrap();

        let stream = stream_response_body(
            pump,
            upstream,
            repo,
            api_key(),
            audited_request(),
            "m".to_string(),
            "up-model".to_string(),
            "chat".to_string(),
            false,
            "{}".to_string(),
            None,
            "ch-1".to_string(),
            "ch".to_string(),
            "anthropic".to_string(),
            1,
            "messages_g1_native".to_string(),
            None,
            "anthropic".to_string(),
            "messages".to_string(),
            "channel".to_string(),
        );

        let mut bytes = Vec::new();
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("event: message_start") && !text.contains("event: content_block_delta"),
            "downstream Chat client must NEVER see raw Anthropic bytes via stream_response_body: {text}"
        );
        assert!(
            text.contains("\"content\":\"carried\""),
            "the carry record must be decoded and emitted by stream_response_body: {text}"
        );
    }
}
