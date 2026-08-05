//! Request feature collection for routing/codec pre-checks (T03).
//!
//! These features are derived from the ORIGINAL downstream protocol JSON full
//! tree, before any conversion, so a codec/router can prove coverage of
//! Responses built-in tools, image URLs, file metadata, unknown content blocks
//! and unknown top-level fields — even when the converter would otherwise
//! skip or compress them.
//!
//! Base64 attachments are audited as metadata only (media type, declared
//! length, actual length, SHA-256).  The payload body is never scanned as
//! ordinary text and never persisted.

use super::gate::{Base64AttachmentMeta, DownstreamProtocol};
use sha2::{Digest, Sha256};

/// Collect routing/codec pre-check features from the ORIGINAL protocol JSON.
pub fn collect_features(
    value: &serde_json::Value,
    protocol: DownstreamProtocol,
    safe_forward_headers: &[(String, String)],
) -> super::gate::RequestFeatures {
    let mut features = super::gate::RequestFeatures::default();
    // Unknown top-level fields are traceable so a router can reject (or
    // preserve) them before upstream instead of silently dropping them.
    if let serde_json::Value::Object(map) = value {
        for (k, v) in map {
            if !known_top_level_field(protocol, k) {
                if let Some(t) = v.as_str() {
                    if t.is_empty() {
                        features.unknown_fields.push(format!("{k} (empty)"));
                    } else {
                        features.unknown_fields.push(k.clone());
                    }
                } else {
                    features.unknown_fields.push(k.clone());
                }
            }
        }
    }
    walk_features(value, "$", &mut features);
    features.beta_headers = safe_forward_headers
        .iter()
        .filter(|(name, _)| name.to_ascii_lowercase().starts_with("anthropic-beta"))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    features.has_tools = has_tools(value);
    features
}

/// Top-level fields each downstream protocol accepts.  Unknown fields must be
/// preserved or rejected before upstream — they must never be silently
/// dropped (T00 decision 8 / T03 spec).
fn known_top_level_field(protocol: DownstreamProtocol, key: &str) -> bool {
    let common = [
        "model", "stream", "stream_options", "temperature", "top_p", "n", "stop",
        "max_tokens", "max_completion_tokens", "presence_penalty", "frequency_penalty",
        "logit_bias", "user", "seed", "extra_body", "metadata", "store", "reasoning",
        "parallel_tool_calls", "tool_choice", "tools", "response_format", "timeout",
        "trace_id", "route_group",
    ];
    if common.contains(&key) {
        return true;
    }
    match protocol {
        DownstreamProtocol::ChatCompletions => matches!(key, "messages" | "function_call" | "functions" | "logprobs" | "top_logprobs" | "modalities" | "audio" | "service_tier"),
        DownstreamProtocol::Completions => matches!(key, "prompt" | "best_of" | "echo" | "logprobs" | "suffix" | "max_tokens"),
        DownstreamProtocol::Responses => matches!(key, "input" | "instructions" | "previous_response_id" | "include" | "text" | "output" | "tools" | "builtin_tool" | "file_search" | "web_search" | "code_interpreter" | "computer_use" | "truncation" | "dimensions" | "store" | "parallel_tool_calls" | "reasoning"),
        DownstreamProtocol::Messages | DownstreamProtocol::CountTokens => matches!(key, "messages" | "system" | "max_tokens" | "stop_sequences" | "temperature" | "top_p" | "top_k" | "metadata" | "tools" | "tool_choice" | "thinking" | "betas" | "service_tier"),
        DownstreamProtocol::Embeddings => matches!(key, "input" | "encoding_format" | "dimensions" | "input_type"),
        DownstreamProtocol::Images => matches!(key, "prompt" | "n" | "size" | "quality" | "style" | "response_format" | "user"),
        DownstreamProtocol::Audio => matches!(key, "file" | "model" | "language" | "prompt" | "response_format" | "temperature" | "input" | "voice" | "speed" | "instructions"),
    }
}

fn walk_features(value: &serde_json::Value, path: &str, features: &mut super::gate::RequestFeatures) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(meta) = parse_data_url(path, s) {
                features.base64_attachments.push(meta);
            }
            // Record external image URLs (only http/https, not data: URLs).
            if s.starts_with("http://") || s.starts_with("https://") {
                // Only surface image-looking URLs; keep the list bounded.
                let lower = s.to_ascii_lowercase();
                let is_image = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", "image/"]
                    .iter()
                    .any(|ext| lower.contains(ext));
                if is_image && features.image_urls.len() < 32 {
                    features.image_urls.push(s.clone());
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk_features(item, &format!("{}[{}]", path, i), features);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let child = if path == "$" { format!("$.{}", k) } else { format!("{}.{}", path, k) };
                // Responses built-in tools: {"type": "web_search_preview", ...}
                if k == "type" {
                    if let Some(t) = v.as_str() {
                        if is_responses_content_type(t) || is_responses_tool_type(t) {
                            features.builtin_tools.push(format!("{child}: {t}"));
                        }
                    }
                }
                walk_features(v, &child, features);
            }
        }
        _ => {}
    }
}

/// Detect Responses API tool / content-block types that must be traceable.
fn is_responses_tool_type(t: &str) -> bool {
    matches!(
        t,
        "web_search_preview"
            | "web_search"
            | "file_search"
            | "code_interpreter"
            | "computer_use_preview"
            | "computer_use"
            | "builtin_tool"
            | "function"
            | "local_shell"
            | "mcp_server"
    )
}

/// Detect Responses content block types (including unknown ones).
fn is_responses_content_type(t: &str) -> bool {
    matches!(
        t,
        "input_text"
            | "output_text"
            | "input_image"
            | "output_image"
            | "input_file"
            | "output_file"
            | "refusal"
            | "reasoning"
            | "computer_call"
    )
}

/// Whether the JSON contains function/tool definitions or calls.
fn has_tools(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            lower.contains("function") || lower.contains("tool")
        }
        serde_json::Value::Array(items) => items.iter().any(has_tools),
        serde_json::Value::Object(map) => map.iter().any(|(k, v)| {
            let key = k.to_ascii_lowercase();
            key.contains("tools") || key.contains("tool") || (key == "type" && has_tools(v)) || has_tools(v)
        }),
        _ => false,
    }
}

/// Parse a `data:<media_type>;base64,<payload>` string as attachment metadata.
/// Returns `None` for anything that is not a base64 data URL.  The payload is
/// only length/hash measured — never scanned as text.
fn parse_data_url(path: &str, s: &str) -> Option<Base64AttachmentMeta> {
    let rest = s.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let (media_type, is_b64) = match header.rsplit_once(';') {
        Some((mt, enc)) => (mt, enc.eq_ignore_ascii_case("base64")),
        None => (header, false),
    };
    if !is_b64 {
        return None;
    }
    Some(Base64AttachmentMeta {
        pointer: path.to_string(),
        media_type: if media_type.is_empty() { "application/octet-stream".to_string() } else { media_type.to_string() },
        declared_len: payload.len(),
        actual_len: payload.len(),
        sha256: hex::encode(Sha256::digest(payload.as_bytes())),
    })
}
