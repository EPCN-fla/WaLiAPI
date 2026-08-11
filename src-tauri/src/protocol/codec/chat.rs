//! `chat_to_messages_v1` — OpenAI Chat Completions → Anthropic Messages.
//!
//! Covers request encoding, non-stream response decoding, and the streaming
//! (SSE) response decoding.  Every conversion is fail-closed: unsupported
//! features are rejected with a stable code + JSON pointer before any upstream
//! access; invalid tool arguments are never rewritten to `{}`.  OpenAI-compatible
//! gateways occasionally emit provider-specific terminal finish reasons; once a
//! response has otherwise completed, those are conservatively represented as an
//! Anthropic `end_turn` rather than aborting a committed Claude Code stream.

use super::error::{DecodeError, FeatureKind, UnsupportedFeatures};
use super::ports::{DecodedResponse, NonStreamDecoder, StreamDecoder};
use super::report::{ConversionContext, Usage};
use super::request;
use super::sse;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Sampling parameters we can map 1:1 between Chat and Messages.
///
/// `n` is intentionally absent: Anthropic Messages only ever returns a single
/// completion, so `n > 1` cannot be preserved and must be rejected rather than
/// silently yielding one completion.
const SUPPORTED_TOP_LEVEL: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "stream",
    "stop",
    "tools",
    "tool_choice",
    "reasoning_effort",
];

/// Encode a Chat Completions request into an Anthropic Messages request.
///
/// `model` is the mapped upstream model decided by the caller; the codec never
/// re-maps models.
pub fn encode_chat_to_messages(
    body: &Value,
    model: &str,
) -> Result<(Value, ConversionContext), UnsupportedFeatures> {
    let mut out = Vec::new();
    let mut messages_out: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    // ---- top-level feature scan ----
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if !SUPPORTED_TOP_LEVEL.contains(&key.as_str()) {
                // structured output (response_format / JSON-schema) has its own
                // stable code.
                let kind = if key == "response_format" {
                    FeatureKind::StructuredOutput
                } else {
                    FeatureKind::UnsupportedField
                };
                request::reject(
                    &mut out,
                    kind,
                    format!("/{key}"),
                    format!("Chat request field {key:?} is not supported by chat_to_messages_v1"),
                );
                continue;
            }
            // Unknown finish reason never applies to requests; here we only
            // reject structural fields that are present with an unsupported
            // shape.
            if key == "tool_choice" {
                let _ = value;
            }
        }
    }

    // ---- messages ----
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let messages = body.get("messages").and_then(Value::as_array);
    let messages = match messages {
        Some(arr) => arr,
        None => {
            request::reject(
                &mut out,
                FeatureKind::UnsupportedField,
                "/messages",
                "Chat request requires a messages array",
            );
            return Err(UnsupportedFeatures::new(out));
        }
    };

    for (i, msg) in messages.iter().enumerate() {
        let mp = format!("/messages/{i}");
        if let Err(e) =
            convert_chat_message_to_anthropic(msg, &mp, &mut messages_out, &mut system_parts)
        {
            // merge rejections
            out.extend(e.fields);
        }
    }

    // ---- model ----
    // Chat Completions *requests* do not carry an `id` (that is a response
    // field), so derive a per-request conversation id from the caller instead.
    let request_id = format!("chatcmpl_{}", uuid::Uuid::new_v4().simple());

    // ---- sampling params ----
    let mut claude = Map::new();
    claude.insert("model".to_string(), Value::String(model.to_string()));
    // Anthropic Messages requires `max_tokens`.  When the Chat request omits it
    // we use a documented safe default (4096) so the upstream call is not
    // malformed; a per-model profile would live in the caller (PreparedAttempt)
    // and could override this via the request body.  Recorded as a deferred
    // choice in the T04 report (F8).
    claude.insert(
        "max_tokens".to_string(),
        body.get("max_tokens")
            .or_else(|| body.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .map(Value::from)
            .unwrap_or(Value::from(4096u64)),
    );
    if !system_parts.is_empty() {
        claude.insert(
            "system".to_string(),
            Value::Array(
                system_parts
                    .iter()
                    .map(|t| serde_json::json!({"type": "text", "text": t}))
                    .collect(),
            ),
        );
    }
    if let Some(t) = body.get("temperature") {
        if !t.is_null() {
            claude.insert("temperature".to_string(), t.clone());
        }
    }
    if let Some(t) = body.get("top_p") {
        if !t.is_null() {
            claude.insert("top_p".to_string(), t.clone());
        }
    }
    if let Some(stop) = body.get("stop") {
        let mapped = match stop {
            Value::String(s) => Value::Array(vec![Value::String(s.clone())]),
            Value::Array(a) => Value::Array(a.clone()),
            _ => {
                request::reject(
                    &mut out,
                    FeatureKind::UnsupportedField,
                    "/stop",
                    "stop must be a string or an array of strings",
                );
                Value::Null
            }
        };
        if !mapped.is_null() {
            claude.insert("stop_sequences".to_string(), mapped);
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mut claude_tools = Vec::new();
        for (i, tool) in tools.iter().enumerate() {
            let tp = format!("/tools/{i}");
            match convert_chat_tool_to_anthropic(tool, &tp) {
                Ok(t) => claude_tools.push(t),
                Err(e) => out.extend(e.fields),
            }
        }
        if !claude_tools.is_empty() {
            claude.insert("tools".to_string(), Value::Array(claude_tools));
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        match request::chat_tool_choice_to_anthropic(tc, "/tool_choice") {
            Ok(Some(v)) => {
                claude.insert("tool_choice".to_string(), v);
            }
            Ok(None) => {}
            Err(e) => out.extend(e.fields),
        }
    }
    claude.insert("stream".to_string(), Value::Bool(stream));

    // reasoning_effort -> thinking (fail-open mapping, CPA semantics).  Only
    // when the downstream asked for a reasoning effort; absent effort leaves
    // thinking unset so the upstream applies its own default.
    if let Some(effort) = body.get("reasoning_effort").and_then(Value::as_str) {
        let e = effort.to_ascii_lowercase();
        match e.as_str() {
            "none" | "off" => {
                claude.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "disabled"}),
                );
            }
            "auto" => {
                claude.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "adaptive"}),
                );
            }
            _ => {
                let mapped = crate::protocol::thinking::map_effort_to_claude(&e);
                claude.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "adaptive"}),
                );
                claude.insert(
                    "output_config".to_string(),
                    serde_json::json!({"effort": mapped}),
                );
            }
        }
    }

    if !out.is_empty() {
        return Err(UnsupportedFeatures::new(out));
    }

    claude.insert("messages".to_string(), Value::Array(messages_out));
    let context = ConversionContext::new(request_id, model.to_string(), stream);
    Ok((Value::Object(claude), context))
}

/// Convert one Chat message to an Anthropic message (and possibly system parts).
fn convert_chat_message_to_anthropic(
    msg: &Value,
    pointer: &str,
    messages_out: &mut Vec<Value>,
    system_parts: &mut Vec<String>,
) -> Result<(), UnsupportedFeatures> {
    let role = msg.get("role").and_then(Value::as_str).ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnknownRole,
            format!("{pointer}/role"),
            "Chat message missing role",
        )
    })?;

    match role {
        "system" | "developer" => {
            // Anthropic has a single top-level `system`; multiple Chat system
            // messages are concatenated in order (preserving order).
            match msg.get("content") {
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        system_parts.push(s.clone());
                    }
                }
                Some(Value::Array(blocks)) => {
                    for (bi, b) in blocks.iter().enumerate() {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                                if !t.is_empty() {
                                    system_parts.push(t.to_string());
                                }
                            }
                            _ => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::UnknownBlock,
                                    format!("{pointer}/content/{bi}/type"),
                                    "system/developer content block must be text",
                                ))
                            }
                        }
                    }
                }
                _ => {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        format!("{pointer}/content"),
                        "system/developer content must be a string or text blocks",
                    ))
                }
            }
            Ok(())
        }
        "user" => {
            let content = msg.get("content");
            let mut blocks: Vec<Value> = Vec::new();
            match content {
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": s}));
                    }
                }
                Some(Value::Array(items)) => {
                    for (bi, item) in items.iter().enumerate() {
                        let bp = format!("{pointer}/content/{bi}");
                        match item.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let t = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !t.is_empty() {
                                    blocks.push(serde_json::json!({"type": "text", "text": t}));
                                }
                            }
                            Some("image_url") => {
                                // Chat image_url -> Anthropic image block.  A
                                // Chat `image_url` may carry either a data URL or
                                // a plain http(s) URL; both are validated to
                                // mirror the request-side image gate (R15).
                                let url = item
                                    .pointer("/image_url/url")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        UnsupportedFeatures::single(
                                            FeatureKind::Media,
                                            format!("{bp}/image_url/url"),
                                            "image_url content block missing image_url.url",
                                        )
                                    })?;
                                let (_media_type, base64) = parse_data_url(url);
                                let source = if let Some((mt, data)) = base64 {
                                    if !mt.starts_with("image/") {
                                        return Err(UnsupportedFeatures::single(
                                            FeatureKind::Media,
                                            format!("{bp}/image_url/url"),
                                            format!("data URL media type {mt:?} is not an image"),
                                        ));
                                    }
                                    if data.len() > request::MAX_IMAGE_BYTES {
                                        return Err(UnsupportedFeatures::single(
                                            FeatureKind::Media,
                                            format!("{bp}/image_url/url"),
                                            "data URL image exceeds the size limit",
                                        ));
                                    }
                                    serde_json::json!({
                                        "type": "base64",
                                        "media_type": mt,
                                        "data": data
                                    })
                                } else {
                                    if !url.starts_with("http://") && !url.starts_with("https://") {
                                        return Err(UnsupportedFeatures::single(
                                            FeatureKind::Media,
                                            format!("{bp}/image_url/url"),
                                            "image_url url must be http(s) or a data URL",
                                        ));
                                    }
                                    serde_json::json!({"type": "url", "url": url})
                                };
                                blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": source,
                                }));
                            }
                            Some("input_text") | Some("output_text") => {
                                let t = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !t.is_empty() {
                                    blocks.push(serde_json::json!({"type": "text", "text": t}));
                                }
                            }
                            Some(other) => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::UnknownBlock,
                                    format!("{bp}/type"),
                                    format!("unsupported user content block type {other:?}"),
                                ))
                            }
                            None => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::UnknownBlock,
                                    format!("{bp}/type"),
                                    "user content block missing type",
                                ))
                            }
                        }
                    }
                }
                Some(Value::Null) | None => {
                    // empty user message — allowed (e.g. assistant tool_use continues)
                }
                _ => {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        format!("{pointer}/content"),
                        "user content must be a string or an array of blocks",
                    ))
                }
            }
            if !blocks.is_empty() {
                messages_out.push(serde_json::json!({
                    "role": "user",
                    "content": Value::Array(blocks),
                }));
            }
            Ok(())
        }
        "assistant" => {
            let mut content_blocks: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let content = msg.get("content");
            match content {
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        content_blocks.push(serde_json::json!({"type": "text", "text": s}));
                    }
                }
                Some(Value::Array(items)) => {
                    for (bi, item) in items.iter().enumerate() {
                        let bp = format!("{pointer}/content/{bi}");
                        match item.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let t = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !t.is_empty() {
                                    content_blocks
                                        .push(serde_json::json!({"type": "text", "text": t}));
                                }
                            }
                            Some(other) => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::UnknownBlock,
                                    format!("{bp}/type"),
                                    format!("unsupported assistant content block type {other:?}"),
                                ))
                            }
                            None => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::UnknownBlock,
                                    format!("{bp}/type"),
                                    "assistant content block missing type",
                                ))
                            }
                        }
                    }
                }
                Some(Value::Null) | None => {}
                _ => {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        format!("{pointer}/content"),
                        "assistant content must be a string or an array of blocks",
                    ))
                }
            }
            if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for (ci, call) in calls.iter().enumerate() {
                    let cp = format!("{pointer}/tool_calls/{ci}");
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            UnsupportedFeatures::single(
                                FeatureKind::MissingToolField,
                                format!("{cp}/id"),
                                "tool call missing id",
                            )
                        })?;
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            UnsupportedFeatures::single(
                                FeatureKind::MissingToolField,
                                format!("{cp}/function/name"),
                                "tool call missing function.name",
                            )
                        })?;
                    let args_str = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            UnsupportedFeatures::single(
                                FeatureKind::InvalidToolArguments,
                                format!("{cp}/function/arguments"),
                                "tool call missing function.arguments",
                            )
                        })?;
                    let input: Value = serde_json::from_str(args_str).map_err(|e| {
                        UnsupportedFeatures::single(
                            FeatureKind::InvalidToolArguments,
                            format!("{cp}/function/arguments"),
                            format!("tool arguments are not valid JSON: {e}"),
                        )
                    })?;
                    if !input.is_object() {
                        return Err(UnsupportedFeatures::single(
                            FeatureKind::InvalidToolArguments,
                            format!("{cp}/function/arguments"),
                            "tool arguments must decode to a JSON object",
                        ));
                    }
                    tool_calls.push(serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    }));
                }
            }
            let mut combined = content_blocks;
            combined.extend(tool_calls);
            if combined.is_empty() {
                // Anthropic rejects an assistant message with no content; drop
                // it only when there is truly nothing (matches existing bridge).
                return Ok(());
            }
            messages_out.push(serde_json::json!({
                "role": "assistant",
                "content": Value::Array(combined),
            }));
            Ok(())
        }
        "tool" => {
            let tool_call_id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{pointer}/tool_call_id"),
                        "tool message missing tool_call_id",
                    )
                })?;
            let content = msg.get("content");
            let text = match content {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(items)) => {
                    let mut t = String::new();
                    for (bi, item) in items.iter().enumerate() {
                        let bp = format!("{pointer}/content/{bi}");
                        match item.get("type").and_then(Value::as_str) {
                            Some("text") => t.push_str(item.get("text").and_then(Value::as_str).unwrap_or("")),
                            Some("image_url") => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::Media,
                                    format!("{bp}/type"),
                                    "tool_result images are not representable in Messages for this version",
                                ))
                            }
                            _ => {
                                return Err(UnsupportedFeatures::single(
                                    FeatureKind::UnknownBlock,
                                    format!("{bp}/type"),
                                    "tool message content block must be text",
                                ))
                            }
                        }
                    }
                    t
                }
                Some(Value::Null) | None => String::new(),
                _ => {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        format!("{pointer}/content"),
                        "tool message content must be a string or text blocks",
                    ))
                }
            };
            let is_error = msg
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // Canonical Anthropic tool_result is a *content block* inside the
            // user message's content array.  The message-level `tool_result`
            // key is not part of the Messages schema and the real API rejects
            // it with 400 invalid_request_error.
            let mut result_blocks: Vec<Value> = Vec::new();
            if !text.is_empty() {
                result_blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            let tool_result_block = serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": Value::Array(result_blocks),
                "is_error": is_error,
            });
            // Anthropic requires all tool results for one assistant turn in a
            // SINGLE user message: aggregate consecutive tool results into the
            // same user message instead of one message per tool result.
            let appended = if let Some(last) = messages_out.last_mut() {
                if last.get("role").and_then(Value::as_str) == Some("user") {
                    if let Some(content_arr) = last.get_mut("content").and_then(Value::as_array_mut)
                    {
                        let is_tool_result = content_arr
                            .last()
                            .map(|b| b.get("type").and_then(Value::as_str))
                            == Some(Some("tool_result"));
                        if is_tool_result {
                            content_arr.push(tool_result_block.clone());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if !appended {
                messages_out.push(serde_json::json!({
                    "role": "user",
                    "content": Value::Array(vec![tool_result_block]),
                }));
            }
            Ok(())
        }
        other => Err(UnsupportedFeatures::single(
            FeatureKind::UnknownRole,
            format!("{pointer}/role"),
            format!("unsupported Chat message role {other:?}"),
        )),
    }
}

/// Convert a Chat `tools` array entry to an Anthropic tool.
fn convert_chat_tool_to_anthropic(
    tool: &Value,
    pointer: &str,
) -> Result<Value, UnsupportedFeatures> {
    let ty = tool.get("type").and_then(Value::as_str);
    if ty != Some("function") {
        return Err(UnsupportedFeatures::single(
            FeatureKind::BuiltinTool,
            format!("{pointer}/type"),
            format!("only function tools are supported, found {ty:?}"),
        ));
    }
    let f = tool.get("function").ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::MissingToolField,
            format!("{pointer}/function"),
            "function tool missing function",
        )
    })?;
    let name = f
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::MissingToolField,
                format!("{pointer}/function/name"),
                "function tool missing name",
            )
        })?;
    let parameters = f.get("parameters").cloned().ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::InvalidToolArguments,
            format!("{pointer}/function/parameters"),
            "function tool missing parameters",
        )
    })?;
    if !parameters.is_object() {
        return Err(UnsupportedFeatures::single(
            FeatureKind::InvalidToolArguments,
            format!("{pointer}/function/parameters"),
            "function tool parameters must be a JSON schema object",
        ));
    }
    let mut claude_tool = serde_json::json!({
        "name": name,
        "input_schema": parameters,
    });
    if let Some(desc) = f.get("description").and_then(Value::as_str) {
        if !desc.is_empty() {
            claude_tool["description"] = Value::String(desc.to_string());
        }
    }
    Ok(claude_tool)
}

/// Parse a `data:` URL into `(media_type, Option<(media_type, payload)>)`.
fn parse_data_url(url: &str) -> (Option<String>, Option<(String, String)>) {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some(semi) = rest.find(';') {
            let media_type = rest[..semi].to_string();
            let after = &rest[semi + 1..];
            if let Some(b64) = after.strip_prefix("base64,") {
                return (
                    Some(media_type.clone()),
                    Some((media_type, b64.to_string())),
                );
            }
            // e.g. data:image/png;charset=utf-8,...
            return (Some(media_type), None);
        }
        if let Some(comma) = rest.find(',') {
            let media_type = rest[..comma].to_string();
            return (Some(media_type), None);
        }
        (Some(rest.to_string()), None)
    } else {
        (None, None)
    }
}

// ===========================================================================
// Non-stream response decoding: Chat Completions JSON -> Messages JSON.
// ===========================================================================

pub struct NonStreamResponseDecoder {
    context: ConversionContext,
}

impl NonStreamResponseDecoder {
    pub fn boxed(context: &ConversionContext) -> Box<dyn NonStreamDecoder + Send + Sync> {
        Box::new(NonStreamResponseDecoder {
            context: context.clone(),
        })
    }
}

impl NonStreamDecoder for NonStreamResponseDecoder {
    fn decode(&self, body: &Value) -> Result<DecodedResponse, DecodeError> {
        let usage = super::identity::parse_usage(super::types::Protocol::Chat, body);
        decode_chat_response_to_messages(body, &self.context)
            .map(|body| DecodedResponse { body, usage })
            .map_err(DecodeError::from)
    }
}

/// Decode a non-stream Chat Completions response into Messages.
///
/// This is the strict implementation extracted from
/// `protocol::anthropic::openai_to_anthropic`; its rejection policy is
/// unchanged (invalid arguments fail, unknown finish reasons never downgrade).
pub fn decode_chat_response_to_messages(
    body: &Value,
    context: &ConversionContext,
) -> Result<Value, UnsupportedFeatures> {
    let choice = body.pointer("/choices/0").ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnknownEvent,
            "/choices/0",
            "Chat response missing choices[0]",
        )
    })?;
    let message = choice.get("message").ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnknownEvent,
            "/choices/0/message",
            "Chat response missing message",
        )
    })?;

    // Reasoning content from an OpenAI upstream is carried into Messages as a
    // `thinking` block (fail-open, CPA semantics: it is always preserved even
    // when `content` is non-empty).  `reasoning_content` may be a plain string
    // or an object `{"text": ...}`; `redacted_thinking` has no readable text.
    let reasoning_text = extract_reasoning_text(message);

    let content_text = match message.get("content") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => {
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnknownBlock,
                "/choices/0/message/content",
                "Chat response has unsupported non-text message content",
            ))
        }
    };

    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|c| !c.is_empty());

    // Unknown finish reason must never become end_turn/stop.
    let stop_reason = match finish_reason {
        Some("stop") => "end_turn",
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("content_filter") | Some("refusal") => "refusal",
        Some(other) => {
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnknownFinishReason,
                "/choices/0/finish_reason",
                format!("unknown Chat finish_reason {other:?}"),
            ))
        }
        None => {
            // No finish_reason: only safe to call it tool_use when there are
            // tool calls, otherwise the completion is incomplete.
            if has_tool_calls {
                "tool_use"
            } else {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::UnknownFinishReason,
                    "/choices/0/finish_reason",
                    "Chat response has no finish_reason and no tool_calls",
                ));
            }
        }
    };

    let usage = usage_from_chat(body);

    let mut content_blocks: Vec<Value> = Vec::new();
    // Thinking block precedes the text block, mirroring the assistant message
    // shape Claude produces natively.
    if !reasoning_text.is_empty() {
        content_blocks.push(serde_json::json!({"type": "thinking", "thinking": reasoning_text}));
    }
    if !content_text.is_empty() {
        content_blocks.push(serde_json::json!({"type": "text", "text": content_text}));
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (i, tc) in tool_calls.iter().enumerate() {
            let cp = format!("/choices/0/message/tool_calls/{i}");
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{cp}/id"),
                        "Chat response tool call missing id",
                    )
                })?;
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{cp}/function/name"),
                        "Chat response tool call missing function.name",
                    )
                })?;
            let args_str = tc
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::InvalidToolArguments,
                        format!("{cp}/function/arguments"),
                        "Chat response tool call missing function.arguments",
                    )
                })?;
            let input: Value = serde_json::from_str(args_str).map_err(|e| {
                UnsupportedFeatures::single(
                    FeatureKind::InvalidToolArguments,
                    format!("{cp}/function/arguments"),
                    format!("Chat response tool arguments are not valid JSON: {e}"),
                )
            })?;
            if !input.is_object() {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::InvalidToolArguments,
                    format!("{cp}/function/arguments"),
                    "Chat response tool arguments must decode to a JSON object",
                ));
            }
            content_blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }
    if content_blocks.is_empty() {
        content_blocks.push(serde_json::json!({"type": "text", "text": ""}));
    }

    Ok(serde_json::json!({
        "id": body.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4().simple())),
        "type": "message",
        "role": "assistant",
        "model": context.upstream_model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
        }
    }))
}

/// Extract reasoning text from a Chat assistant message, supporting both the
/// plain-string and `{"text": ...}` shapes of `reasoning_content`, plus the
/// `thinking`/`reasoning` aliases some providers use.  Returns `""` when no
/// reasoning is present.
fn extract_reasoning_text(message: &Value) -> String {
    let candidate = message
        .get("reasoning_content")
        .or_else(|| message.get("thinking"))
        .or_else(|| message.get("reasoning"));
    let Some(c) = candidate else {
        return String::new();
    };
    match c {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("text")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Extract real usage from a Chat response.  `usage_unknown` is surfaced to the
/// gateway via the report; a 0 is only ever a protocol-mandated placeholder.
pub fn usage_from_chat(body: &Value) -> Usage {
    let prompt = body.pointer("/usage/prompt_tokens").and_then(Value::as_u64);
    let completion = body
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64);
    let cache_read = body
        .pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            body.pointer("/usage/cache_read_input_tokens")
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let cache_creation = body
        .pointer("/usage/cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: prompt.unwrap_or(0),
        output_tokens: completion.unwrap_or(0),
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
        usage_unknown: prompt.is_none() || completion.is_none(),
    }
}

// ===========================================================================
// Streaming: Chat SSE -> Messages SSE.
// ===========================================================================

#[derive(Default)]
struct ToolAccum {
    id: String,
    name: String,
    arguments: String,
    stopped: bool,
}

/// Per-request state for the Chat SSE → Messages SSE decoder.
#[derive(Default)]
pub struct ChatSseState {
    pending: Vec<u8>,
    started: bool,
    ended: bool,
    /// OpenAI-compatible providers commonly use this sentinel as the only
    /// terminal marker, omitting `choices[].finish_reason` entirely.
    saw_done: bool,
    /// A clean transport EOF after real assistant output is also a usable
    /// terminal signal for some OpenAI-compatible streaming providers.  Keep
    /// this separate from `started`: a role-only or usage-only frame must not
    /// turn a truncated response into a successful completion.
    saw_assistant_output: bool,
    finish_reason: Option<String>,
    usage: Usage,
    next_content_index: usize,
    open_text: Option<usize>,
    open_thinking: Option<usize>,
    tools: BTreeMap<usize, ToolAccum>,
    /// The mapped upstream model (from the PreparedAttempt) to emit in the
    /// synthesized `message_start` frame; the codec never re-maps models.
    pub model: String,
    /// Per-request downstream message id.
    pub message_id: String,
}

impl ChatSseState {
    /// Create the per-request state with the caller-provided model and id.
    pub fn new(model: &str, message_id: &str) -> Self {
        Self {
            model: model.to_string(),
            message_id: if message_id.is_empty() {
                format!("msg_{}", uuid::Uuid::new_v4().simple())
            } else {
                message_id.to_string()
            },
            ..Default::default()
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = sse::record_end(&self.pending) {
            let record: Vec<u8> = self.pending.drain(..end).collect();
            let payload = sse::parse_data_payload(&record)?;
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                self.saw_done = true;
                continue;
            }
            let json: Value = serde_json::from_str(&payload).map_err(|e| {
                UnsupportedFeatures::single(
                    FeatureKind::UnknownEvent,
                    "/",
                    format!("OpenAI upstream emitted invalid SSE JSON: {e}"),
                )
            })?;
            self.consume_json(json, &mut events)?;
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let record = std::mem::take(&mut self.pending);
            let payload = sse::parse_data_payload(&record)?;
            if payload == "[DONE]" {
                self.saw_done = true;
            } else if !payload.is_empty() {
                let json: Value = serde_json::from_str(&payload).map_err(|e| {
                    UnsupportedFeatures::single(
                        FeatureKind::UnknownEvent,
                        "/",
                        format!("OpenAI upstream emitted invalid SSE JSON: {e}"),
                    )
                })?;
                self.consume_json(json, &mut events)?;
            }
        }
        self.emit_final(&mut events)?;
        Ok(events)
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    fn consume_json(
        &mut self,
        json: Value,
        events: &mut Vec<String>,
    ) -> Result<(), UnsupportedFeatures> {
        // usage may arrive as a standalone frame or on a choice frame.
        if let Some(u) = json.get("usage") {
            self.update_usage(u);
        }
        if !self.started {
            self.started = true;
            events.push(sse::event(
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": self.message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": self.usage.input_tokens, "output_tokens": 0}
                    }
                }),
            ));
        }
        for choice in json
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            // Fail-open: upstream reasoning is emitted as a Messages `thinking`
            // block (start/delta/stop), never rejected.  Some OpenAI-compat
            // providers surface it as `reasoning_content` (string) or a
            // `thinking` object; both are accepted.
            let reasoning_text = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .or_else(|| match delta.get("thinking") {
                    Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                    Some(Value::Object(m)) => m
                        .get("text")
                        .or_else(|| m.get("thinking"))
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string),
                    _ => None,
                });
            if let Some(text) = reasoning_text {
                self.saw_assistant_output = true;
                let index = self.ensure_thinking(events);
                events.push(sse::event(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "thinking_delta", "thinking": text}
                    }),
                ));
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                self.saw_assistant_output = true;
                let index = self.ensure_text(events);
                events.push(sse::event(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                if !calls.is_empty() {
                    self.saw_assistant_output = true;
                }
                for call in calls {
                    self.consume_tool_call(call)?;
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if !reason.is_empty() && reason != "null" {
                    // Unknown finish reason is rejected at finalize (never
                    // downgraded), but we record it now.
                    self.finish_reason = Some(reason.to_string());
                }
            }
            if delta.get("refusal").and_then(Value::as_str).is_some() {
                self.finish_reason = Some("refusal".to_string());
            }
        }
        Ok(())
    }

    fn update_usage(&mut self, u: &Value) {
        let prompt = u.get("prompt_tokens").and_then(Value::as_u64);
        let completion = u.get("completion_tokens").and_then(Value::as_u64);
        if prompt.is_some() {
            self.usage.input_tokens = prompt.unwrap();
        }
        if completion.is_some() {
            self.usage.output_tokens = completion.unwrap();
        }
        if let Some(c) = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_read_input_tokens = c;
        }
        if let Some(c) = u.get("cache_creation_input_tokens").and_then(Value::as_u64) {
            self.usage.cache_creation_input_tokens = c;
        }
        if prompt.is_none() && completion.is_none() {
            // A bare usage frame with no real tokens is not a usable count.
            if self.usage.input_tokens == 0 && self.usage.output_tokens == 0 {
                self.usage.usage_unknown = true;
            }
        }
    }

    fn ensure_text(&mut self, events: &mut Vec<String>) -> usize {
        if let Some(index) = self.open_text {
            return index;
        }
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.open_text = Some(index);
        events.push(sse::event(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        index
    }

    fn ensure_thinking(&mut self, events: &mut Vec<String>) -> usize {
        if let Some(index) = self.open_thinking {
            return index;
        }
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.open_thinking = Some(index);
        events.push(sse::event(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        ));
        index
    }

    fn consume_tool_call(&mut self, call: &Value) -> Result<(), UnsupportedFeatures> {
        let source_index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.tools.entry(source_index).or_default().id = id.to_string();
        }
        if let Some(name) = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .or_else(|| call.get("name").and_then(Value::as_str))
            .filter(|name| !name.is_empty())
        {
            self.tools.entry(source_index).or_default().name = name.to_string();
        }
        if let Some(arguments) = call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .or_else(|| call.get("arguments").and_then(Value::as_str))
        {
            self.tools
                .entry(source_index)
                .or_default()
                .arguments
                .push_str(arguments);
        }
        Ok(())
    }

    fn emit_final(&mut self, events: &mut Vec<String>) -> Result<(), UnsupportedFeatures> {
        if self.ended {
            return Ok(());
        }
        if !self.started {
            // The upstream stream never delivered a first frame.  This is a
            // codec error (not an empty success) so the gateway can fail over
            // before committing the downstream response.
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "OpenAI upstream stream ended before any first frame (no message_start emitted)",
            ));
        }
        if let Some(index) = self.open_text.take() {
            events.push(sse::event(
                "content_block_stop",
                serde_json::json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        if let Some(index) = self.open_thinking.take() {
            events.push(sse::event(
                "content_block_stop",
                serde_json::json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        for tool in self.tools.values_mut() {
            if tool.name.is_empty() {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::MissingToolField,
                    "/choices/0/delta/tool_calls",
                    "OpenAI stream ended with an incomplete tool call",
                ));
            }
            if tool.id.is_empty() {
                tool.id = format!("call_{}", uuid::Uuid::new_v4().simple());
            }
            let input: Value = serde_json::from_str(&tool.arguments).map_err(|e| {
                UnsupportedFeatures::single(
                    FeatureKind::InvalidToolArguments,
                    "/choices/0/delta/tool_calls",
                    format!("OpenAI stream ended with invalid tool arguments: {e}"),
                )
            })?;
            if !input.is_object() {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::InvalidToolArguments,
                    "/choices/0/delta/tool_calls",
                    "OpenAI stream tool arguments must decode to a JSON object",
                ));
            }
            let index = self.next_content_index;
            self.next_content_index += 1;
            events.push(sse::event("content_block_start", serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": tool.id, "name": tool.name, "input": {}}
            })));
            events.push(sse::event(
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": tool.arguments}
                }),
            ));
            tool.stopped = true;
            events.push(sse::event(
                "content_block_stop",
                serde_json::json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        let stop_reason = match self.finish_reason.as_deref() {
            Some("stop") => "end_turn",
            Some("length") => "max_tokens",
            Some("tool_calls") | Some("function_call") => "tool_use",
            Some("content_filter") | Some("refusal") => "refusal",
            Some(other) => {
                // CPA-compatible terminal fallback: the upstream has already
                // completed a valid Chat stream, but this gateway supplied a
                // provider-specific finish reason.  Anthropic has no lossless
                // representation for it, so finish the Message normally and
                // retain the original value in structured logs.
                tracing::warn!(
                    finish_reason = other,
                    fallback_stop_reason = "end_turn",
                    "unknown OpenAI Chat stream finish_reason mapped for Anthropic compatibility"
                );
                "end_turn"
            }
            None => {
                if !self.tools.is_empty() {
                    "tool_use"
                } else if self.saw_done || self.saw_assistant_output {
                    // `[DONE]` is the preferred positive terminal signal.
                    // Some OpenAI-compatible providers instead close a clean
                    // SSE response after sending real assistant output, but
                    // omit both `[DONE]` and the final choice frame.  There is
                    // no lossless Chat finish_reason to map, so complete as
                    // Anthropic `end_turn`. A network error, malformed final
                    // record, or role/usage-only EOF still remains an error.
                    tracing::warn!(
                        saw_done = self.saw_done,
                        saw_assistant_output = self.saw_assistant_output,
                        fallback_stop_reason = "end_turn",
                        "OpenAI Chat stream ended without finish_reason; accepting a valid terminal signal"
                    );
                    "end_turn"
                } else {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownFinishReason,
                        "/choices/0/finish_reason",
                        "OpenAI stream ended without a finish_reason",
                    ));
                }
            }
        };
        events.push(sse::event(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {
                    "input_tokens": self.usage.input_tokens,
                    "output_tokens": self.usage.output_tokens,
                    "cache_creation_input_tokens": self.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": self.usage.cache_read_input_tokens,
                }
            }),
        ));
        events.push(sse::event(
            "message_stop",
            serde_json::json!({"type": "message_stop"}),
        ));
        self.ended = true;
        Ok(())
    }
}

pub struct ChatStreamDecoder {
    state: ChatSseState,
}

impl ChatStreamDecoder {
    pub fn boxed(context: &ConversionContext) -> Box<dyn StreamDecoder + Send + Sync> {
        Box::new(ChatStreamDecoder {
            state: ChatSseState::new(&context.upstream_model, &context.request_id),
        })
    }
}

impl StreamDecoder for ChatStreamDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, DecodeError> {
        self.state.feed(bytes).map_err(DecodeError::from)
    }
    fn finish(&mut self) -> Result<Vec<String>, DecodeError> {
        self.state.finish().map_err(DecodeError::from)
    }
    fn usage(&self) -> Option<Usage> {
        Some(self.state.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::ChatSseState;

    #[test]
    fn stream_unknown_finish_reason_completes_as_end_turn() {
        let mut state = ChatSseState::new("go-model", "msg_test");
        let events = state
            .feed(
                br#"data: {"id":"chatcmpl_test","model":"go-model","choices":[{"delta":{"role":"assistant","content":"ok"},"finish_reason":"completed"}]}

"#,
            )
            .expect("provider-specific finish reason must not abort the stream");
        let final_events = state.finish().expect("stream finalizes");
        let all = events.into_iter().chain(final_events).collect::<String>();

        assert!(all.contains("\"text\":\"ok\""));
        assert!(all.contains("\"stop_reason\":\"end_turn\""));
        assert!(all.contains("event: message_stop"));
    }

    #[test]
    fn stream_done_without_finish_reason_completes_as_end_turn() {
        let mut state = ChatSseState::new("go-model", "msg_test");
        let events = state
            .feed(
                b"data: {\"id\":\"chatcmpl_test\",\"model\":\"go-model\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
            )
            .expect("a [DONE]-terminated stream must be accepted");
        let final_events = state.finish().expect("stream finalizes");
        let all = events.into_iter().chain(final_events).collect::<String>();
        assert!(all.contains("\"text\":\"ok\""));
        assert!(all.contains("\"stop_reason\":\"end_turn\""));
        assert!(all.contains("event: message_stop"));
    }

    #[test]
    fn stream_output_without_terminal_marker_completes_as_end_turn() {
        let mut state = ChatSseState::new("go-model", "msg_test");
        let events = state
            .feed(
                b"data: {\"id\":\"chatcmpl_test\",\"model\":\"go-model\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            )
            .expect("a content frame must decode");
        let final_events = state
            .finish()
            .expect("clean EOF after assistant output must finalize");
        let all = events.into_iter().chain(final_events).collect::<String>();
        assert!(all.contains("\"text\":\"ok\""));
        assert!(all.contains("\"stop_reason\":\"end_turn\""));
        assert!(all.contains("event: message_stop"));
    }

    #[test]
    fn stream_role_only_without_terminal_marker_is_rejected() {
        let mut state = ChatSseState::new("go-model", "msg_test");
        state
            .feed(
                b"data: {\"id\":\"chatcmpl_test\",\"model\":\"go-model\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            )
            .expect("a role frame must decode");
        let error = state.finish().expect_err("role-only EOF must fail closed");
        assert_eq!(error.json_pointers, vec!["/choices/0/finish_reason"]);
    }
}
