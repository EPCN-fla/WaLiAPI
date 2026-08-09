//! Strict Responses API codec used by auth-account upstreams.
//!
//! The Codex backend is always streamed.  This module consequently owns both
//! the request representation and the small, byte-framed Responses SSE state
//! machine used to express that stream as Chat (and, by composition, Messages).

use super::error::{FeatureKind, UnsupportedFeatures};
use super::messages;
use super::registry::{NonStreamDecoder, StreamDecoder};
use super::report::{ConversionContext, Usage};
use super::{request, sse};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

const CHAT_TOP_LEVEL: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "max_completion_tokens",
    "stream",
    "tools",
    "tool_choice",
];

/// Encode a Chat Completions request as a Responses request.  This deliberately
/// emits only the backend allow-list fields; callers must not get a silent
/// escape hatch for a field this account upstream cannot represent.
pub fn encode_chat_to_responses(
    body: &Value,
    model: &str,
) -> Result<(Value, ConversionContext), UnsupportedFeatures> {
    let object = body.as_object().ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnsupportedField,
            "/",
            "Chat request must be an object",
        )
    })?;
    let mut rejected = Vec::new();
    for key in object.keys() {
        if !CHAT_TOP_LEVEL.contains(&key.as_str()) {
            request::reject(
                &mut rejected,
                if key == "response_format" {
                    FeatureKind::StructuredOutput
                } else {
                    FeatureKind::UnsupportedField
                },
                format!("/{key}"),
                format!("Chat field {key:?} has no Responses backend representation"),
            );
        }
    }

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::UnsupportedField,
                "/messages",
                "Chat request requires messages array",
            )
        })?;
    let mut input = Vec::new();
    let mut instruction_parts = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match chat_message_to_responses(message, &format!("/messages/{index}")) {
            Ok(ChatMessageParts {
                instructions,
                mut items,
            }) => {
                instruction_parts.extend(instructions);
                input.append(&mut items);
            }
            Err(error) => rejected.extend(error.fields),
        }
    }
    if let Some(tools) = object.get("tools") {
        match chat_tools_to_responses(tools, "/tools") {
            Ok(_) => {}
            Err(error) => rejected.extend(error.fields),
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        if let Err(error) = chat_tool_choice_to_responses(choice, "/tool_choice") {
            rejected.extend(error.fields);
        }
    }
    request::finish(rejected)?;

    let mut response = Map::new();
    response.insert("model".to_owned(), Value::String(model.to_owned()));
    response.insert("input".to_owned(), Value::Array(input));
    if let Some(max_output_tokens) = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
        .and_then(Value::as_u64)
    {
        response.insert(
            "max_output_tokens".to_owned(),
            Value::from(max_output_tokens),
        );
    }
    response.insert(
        "stream".to_owned(),
        Value::Bool(
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    if !instruction_parts.is_empty() {
        response.insert(
            "instructions".to_owned(),
            Value::String(instruction_parts.join("\n")),
        );
    }
    if let Some(tools) = object.get("tools") {
        let tools = chat_tools_to_responses(tools, "/tools")?;
        if !tools.is_empty() {
            response.insert("tools".to_owned(), Value::Array(tools));
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        if let Some(choice) = chat_tool_choice_to_responses(choice, "/tool_choice")? {
            response.insert("tool_choice".to_owned(), choice);
        }
    }
    Ok((
        Value::Object(response),
        ConversionContext::new(
            format!("chatcmpl_{}", uuid::Uuid::new_v4().simple()),
            model,
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    ))
}

struct ChatMessageParts {
    instructions: Vec<String>,
    items: Vec<Value>,
}

fn chat_message_to_responses(
    message: &Value,
    pointer: &str,
) -> Result<ChatMessageParts, UnsupportedFeatures> {
    let message = message.as_object().ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnknownRole,
            pointer,
            "Chat message must be an object",
        )
    })?;
    for key in message.keys() {
        if !["role", "content", "tool_calls", "tool_call_id", "name"].contains(&key.as_str()) {
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnsupportedField,
                format!("{pointer}/{key}"),
                "message field is not representable",
            ));
        }
    }
    let role = message.get("role").and_then(Value::as_str).ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnknownRole,
            format!("{pointer}/role"),
            "message role is required",
        )
    })?;
    let content = chat_content_to_responses(
        message.get("content"),
        &format!("{pointer}/content"),
        role == "assistant",
    )?;
    match role {
        "system" | "developer" => {
            if message.get("tool_calls").is_some() {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::UnsupportedField,
                    format!("{pointer}/tool_calls"),
                    "system/developer tool calls are invalid",
                ));
            }
            let text = content
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            Ok(ChatMessageParts {
                instructions: vec![text],
                items: Vec::new(),
            })
        }
        "user" => Ok(ChatMessageParts {
            instructions: Vec::new(),
            items: vec![serde_json::json!({"type":"message", "role":"user", "content": content})],
        }),
        "assistant" => {
            let mut items = Vec::new();
            if !content.is_empty() {
                items.push(
                    serde_json::json!({"type":"message", "role":"assistant", "content": content}),
                );
            }
            if let Some(calls) = message.get("tool_calls") {
                let calls = calls.as_array().ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{pointer}/tool_calls"),
                        "tool_calls must be an array",
                    )
                })?;
                for (i, call) in calls.iter().enumerate() {
                    items.push(chat_tool_call_to_responses(
                        call,
                        &format!("{pointer}/tool_calls/{i}"),
                    )?);
                }
            }
            Ok(ChatMessageParts {
                instructions: Vec::new(),
                items,
            })
        }
        "tool" => {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{pointer}/tool_call_id"),
                        "tool message requires a tool_call_id",
                    )
                })?;
            let output = content
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            Ok(ChatMessageParts {
                instructions: Vec::new(),
                items: vec![
                    serde_json::json!({"type":"function_call_output", "call_id":call_id, "output":output}),
                ],
            })
        }
        _ => Err(UnsupportedFeatures::single(
            FeatureKind::UnknownRole,
            format!("{pointer}/role"),
            format!("Chat role {role:?} is not supported"),
        )),
    }
}

fn chat_content_to_responses(
    content: Option<&Value>,
    pointer: &str,
    output: bool,
) -> Result<Vec<Value>, UnsupportedFeatures> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![serde_json::json!({
            "type": if output { "output_text" } else { "input_text" },
            "text": text,
        })]),
        Some(Value::Array(parts)) => parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let p = format!("{pointer}/{i}");
                let object = part.as_object().ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        &p,
                        "content part must be an object",
                    )
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => object
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| {
                            serde_json::json!({
                                "type": if output { "output_text" } else { "input_text" },
                                "text": text,
                            })
                        })
                        .ok_or_else(|| {
                            UnsupportedFeatures::single(
                                FeatureKind::UnknownBlock,
                                format!("{p}/text"),
                                "text part requires text",
                            )
                        }),
                    Some("image_url") => {
                        if output {
                            return Err(UnsupportedFeatures::single(
                                FeatureKind::Media,
                                format!("{p}/type"),
                                "assistant image content is not representable",
                            ));
                        }
                        let url = object
                            .get("image_url")
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                UnsupportedFeatures::single(
                                    FeatureKind::Media,
                                    format!("{p}/image_url/url"),
                                    "image_url requires url",
                                )
                            })?;
                        if !(url.starts_with("https://")
                            || url.starts_with("http://")
                            || url.starts_with("data:image/"))
                        {
                            return Err(UnsupportedFeatures::single(
                                FeatureKind::Media,
                                format!("{p}/image_url/url"),
                                "image url must be http(s) or image data URL",
                            ));
                        }
                        if url.len() > request::MAX_IMAGE_BYTES * 2 {
                            return Err(UnsupportedFeatures::single(
                                FeatureKind::Media,
                                format!("{p}/image_url/url"),
                                "image exceeds maximum supported size",
                            ));
                        }
                        Ok(serde_json::json!({"type":"input_image", "image_url":url}))
                    }
                    Some(other) => Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        format!("{p}/type"),
                        format!("content type {other:?} is not representable"),
                    )),
                    None => Err(UnsupportedFeatures::single(
                        FeatureKind::UnknownBlock,
                        format!("{p}/type"),
                        "content part requires type",
                    )),
                }
            })
            .collect(),
        Some(_) => Err(UnsupportedFeatures::single(
            FeatureKind::UnknownBlock,
            pointer,
            "content must be a string, null, or array",
        )),
    }
}

fn chat_tool_call_to_responses(value: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let call_id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::MissingToolField,
                format!("{pointer}/id"),
                "tool call requires id",
            )
        })?;
    if value.get("type").and_then(Value::as_str) != Some("function") {
        return Err(UnsupportedFeatures::single(
            FeatureKind::BuiltinTool,
            format!("{pointer}/type"),
            "only function tool calls are supported",
        ));
    }
    let name = value
        .pointer("/function/name")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::MissingToolField,
                format!("{pointer}/function/name"),
                "tool call requires function name",
            )
        })?;
    let arguments = value
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::InvalidToolArguments,
                format!("{pointer}/function/arguments"),
                "tool arguments must be a JSON string",
            )
        })?;
    if serde_json::from_str::<Value>(arguments).is_err() {
        return Err(UnsupportedFeatures::single(
            FeatureKind::InvalidToolArguments,
            format!("{pointer}/function/arguments"),
            "tool arguments must contain valid JSON",
        ));
    }
    Ok(
        serde_json::json!({"type":"function_call", "call_id":call_id, "name":name, "arguments":arguments}),
    )
}

fn chat_tools_to_responses(
    value: &Value,
    pointer: &str,
) -> Result<Vec<Value>, UnsupportedFeatures> {
    let tools = value.as_array().ok_or_else(|| {
        UnsupportedFeatures::single(
            FeatureKind::UnsupportedField,
            pointer,
            "tools must be an array",
        )
    })?;
    tools
        .iter()
        .enumerate()
        .map(|(i, tool)| {
            let p = format!("{pointer}/{i}");
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::BuiltinTool,
                    format!("{p}/type"),
                    "only function tools are supported",
                ));
            }
            if let Some(object) = tool.as_object() {
                for key in object.keys() {
                    if !["type", "function"].contains(&key.as_str()) {
                        return Err(UnsupportedFeatures::single(
                            FeatureKind::UnsupportedField,
                            format!("{p}/{key}"),
                            "tool property is not representable",
                        ));
                    }
                }
            }
            let function = tool.get("function").ok_or_else(|| {
                UnsupportedFeatures::single(
                    FeatureKind::MissingToolField,
                    format!("{p}/function"),
                    "function tool requires function object",
                )
            })?;
            let function = function.as_object().ok_or_else(|| {
                UnsupportedFeatures::single(
                    FeatureKind::MissingToolField,
                    format!("{p}/function"),
                    "function must be an object",
                )
            })?;
            for key in function.keys() {
                if !["name", "description", "parameters", "strict"].contains(&key.as_str()) {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::UnsupportedField,
                        format!("{p}/function/{key}"),
                        "tool property is not representable",
                    ));
                }
            }
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{p}/function/name"),
                        "function tool requires name",
                    )
                })?;
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type":"object", "properties":{}}));
            if !parameters.is_object() {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::InvalidToolArguments,
                    format!("{p}/function/parameters"),
                    "parameters must be an object",
                ));
            }
            let mut result =
                serde_json::json!({"type":"function", "name":name, "parameters":parameters});
            if let Some(description) = function.get("description") {
                result["description"] = description.clone();
            }
            if let Some(strict) = function.get("strict") {
                result["strict"] = strict.clone();
            }
            Ok(result)
        })
        .collect()
}

fn chat_tool_choice_to_responses(
    value: &Value,
    pointer: &str,
) -> Result<Option<Value>, UnsupportedFeatures> {
    match value {
        Value::String(value) if matches!(value.as_str(), "auto" | "none" | "required") => {
            Ok(Some(Value::String(value.to_string())))
        }
        Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("function") => {
            let name = object
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    UnsupportedFeatures::single(
                        FeatureKind::MissingToolField,
                        format!("{pointer}/function/name"),
                        "function tool_choice requires name",
                    )
                })?;
            if object.keys().any(|key| key != "type" && key != "function") {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::UnsupportedField,
                    pointer,
                    "tool_choice contains an unrepresentable field",
                ));
            }
            Ok(Some(serde_json::json!({"type":"function", "name":name})))
        }
        _ => Err(UnsupportedFeatures::single(
            FeatureKind::UnsupportedField,
            pointer,
            "unsupported Chat tool_choice",
        )),
    }
}

/// Messages → Chat → Responses.  The first encoder remains the authoritative
/// validator for Anthropic-specific fields.
pub fn encode_messages_to_responses(
    body: &Value,
    model: &str,
) -> Result<(Value, ConversionContext), UnsupportedFeatures> {
    let (mut chat, _) = messages::encode_messages_to_chat(body, model)?;
    // The Messages→Chat leg synthesizes `stream_options.include_usage=true`
    // for a Chat upstream.  A Codex Responses account always emits usage in
    // `response.completed` and its strict backend allow-list intentionally has
    // no `stream_options`, so carrying that synthetic field would reject an
    // otherwise valid Messages stream before any account request is sent.
    if let Some(object) = chat.as_object_mut() {
        object.remove("stream_options");
    }
    encode_chat_to_responses(&chat, model)
}

/// Convert a completed Responses object to a non-stream Chat completion.
pub fn decode_responses_response_to_chat(
    body: &Value,
    context: &ConversionContext,
) -> Result<Value, UnsupportedFeatures> {
    let response = body
        .get("response")
        .filter(|value| value.is_object())
        .unwrap_or(body);
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/output",
                "Responses response requires output array",
            )
        })?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("output_text") | Some("text")
                        ) {
                            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        UnsupportedFeatures::single(
                            FeatureKind::MissingToolField,
                            format!("/output/{index}/call_id"),
                            "function call requires call_id",
                        )
                    })?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        UnsupportedFeatures::single(
                            FeatureKind::MissingToolField,
                            format!("/output/{index}/name"),
                            "function call requires name",
                        )
                    })?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        UnsupportedFeatures::single(
                            FeatureKind::InvalidToolArguments,
                            format!("/output/{index}/arguments"),
                            "function call requires arguments",
                        )
                    })?;
                if !serde_json::from_str::<Value>(arguments).is_ok_and(|parsed| parsed.is_object())
                {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::InvalidToolArguments,
                        format!("/output/{index}/arguments"),
                        "function call arguments must be a valid JSON object",
                    ));
                }
                calls.push(serde_json::json!({"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}));
            }
            Some("reasoning") => {
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    for part in summary {
                        reasoning.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                }
            }
            Some(_) => {}
            None => {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::UnknownEvent,
                    format!("/output/{index}/type"),
                    "output item missing type",
                ))
            }
        }
    }
    let usage = usage_from_responses(response);
    let mut message = serde_json::json!({"role":"assistant","content": if text.is_empty() { Value::Null } else { Value::String(text) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let has_tool_calls = !calls.is_empty();
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    let finish_reason = responses_finish_reason(response, has_tool_calls)?;
    Ok(serde_json::json!({
        "id": response.get("id").and_then(Value::as_str).unwrap_or(&context.request_id),
        "object":"chat.completion", "created":response.get("created_at").and_then(Value::as_i64).unwrap_or_else(|| chrono::Utc::now().timestamp()), "model":response.get("model").and_then(Value::as_str).unwrap_or(&context.upstream_model),
        "choices":[{"index":0,"message":message,"finish_reason":finish_reason}],
        "usage":{"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"total_tokens":usage.input_tokens+usage.output_tokens}
    }))
}

fn responses_finish_reason(
    response: &Value,
    has_tool_calls: bool,
) -> Result<&'static str, UnsupportedFeatures> {
    match response.get("status").and_then(Value::as_str) {
        Some("completed") | None => Ok(if has_tool_calls { "tool_calls" } else { "stop" }),
        Some("incomplete") => match response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") | Some("max_tokens") | None => Ok("length"),
            Some("content_filter") | Some("safety") => Ok("content_filter"),
            Some(other) => Err(UnsupportedFeatures::single(
                FeatureKind::UnknownFinishReason,
                "/incomplete_details/reason",
                format!("unknown Responses incomplete reason {other:?}"),
            )),
        },
        Some("failed") => Err(UnsupportedFeatures::single(
            FeatureKind::UnknownEvent,
            "/status",
            "Responses response status is failed",
        )),
        Some(other) => Err(UnsupportedFeatures::single(
            FeatureKind::UnknownEvent,
            "/status",
            format!("Responses response has unsupported status {other:?}"),
        )),
    }
}

pub fn usage_from_responses(response: &Value) -> Usage {
    let input = response
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64);
    let output = response
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64);
    Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        usage_unknown: input.is_none() || output.is_none(),
        ..Usage::default()
    }
}

pub struct ResponsesNonStreamDecoder {
    context: ConversionContext,
}
impl ResponsesNonStreamDecoder {
    pub fn boxed(context: &ConversionContext) -> Box<dyn NonStreamDecoder + Send + Sync> {
        Box::new(Self {
            context: context.clone(),
        })
    }
}
impl NonStreamDecoder for ResponsesNonStreamDecoder {
    fn decode(&self, body: &Value) -> Result<Value, UnsupportedFeatures> {
        decode_responses_response_to_chat(body, &self.context)
    }
}

/// A complete-record SSE accumulator for non-stream account requests.
#[derive(Default)]
pub struct ResponsesEventAccumulator {
    pending: Vec<u8>,
    completed: Option<Value>,
    output_items: BTreeMap<u64, Value>,
    failed: Option<String>,
}
impl ResponsesEventAccumulator {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), UnsupportedFeatures> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = sse::record_end(&self.pending) {
            let record: Vec<u8> = self.pending.drain(..end).collect();
            self.record(&record)?;
        }
        Ok(())
    }
    fn record(&mut self, record: &[u8]) -> Result<(), UnsupportedFeatures> {
        let payload = sse::parse_data_payload(record)?;
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(&payload).map_err(|_| {
            UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE data is not JSON",
            )
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item").cloned() {
                    let index = event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.output_items.len() as u64);
                    self.output_items.insert(index, item);
                }
            }
            Some("response.completed") => self.completed = event.get("response").cloned(),
            Some("response.failed") | Some("response.incomplete") => {
                self.failed = Some("Responses upstream reported a terminal failure".to_string())
            }
            _ => {}
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<Value, UnsupportedFeatures> {
        if !self.pending.is_empty() {
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE ended mid-record",
            ));
        }
        if self.failed.is_some() {
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses upstream failed",
            ));
        }
        let mut completed = self.completed.take().ok_or_else(|| {
            UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE ended without response.completed",
            )
        })?;
        let needs_output_backfill = completed
            .get("output")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        if needs_output_backfill && !self.output_items.is_empty() {
            let output = self.output_items.into_values().collect::<Vec<_>>();
            if let Some(object) = completed.as_object_mut() {
                object.insert("output".to_owned(), Value::Array(output));
            }
        }
        Ok(completed)
    }
}

#[derive(Default)]
struct ResponsesChatState {
    pending: Vec<u8>,
    id: String,
    model: String,
    created: i64,
    role_emitted: bool,
    tool_calls: BTreeMap<u64, ToolCallState>,
    reasoning: String,
    usage: Usage,
    terminal: bool,
}

#[derive(Default)]
struct ToolCallState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    final_arguments: Option<String>,
    completed: bool,
}

impl ResponsesChatState {
    fn new(context: &ConversionContext) -> Self {
        Self {
            id: context.request_id.clone(),
            model: context.upstream_model.clone(),
            created: chrono::Utc::now().timestamp(),
            ..Self::default()
        }
    }
    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        sse::data_frame(
            serde_json::json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
        )
    }
    fn role(&mut self, output: &mut Vec<String>) {
        if !self.role_emitted {
            self.role_emitted = true;
            output.push(self.chunk(serde_json::json!({"role":"assistant"}), None));
        }
    }
    fn tool_index(&self, event: &Value) -> Option<u64> {
        event
            .get("output_index")
            .and_then(Value::as_u64)
            .or_else(|| {
                let item_id = event
                    .get("item_id")
                    .or_else(|| event.pointer("/item/id"))
                    .and_then(Value::as_str)?;
                self.tool_calls
                    .iter()
                    .find_map(|(index, call)| (call.item_id == item_id).then_some(*index))
            })
    }
    fn merge_tool_item(&mut self, index: u64, item: &Value) {
        let call = self.tool_calls.entry(index).or_default();
        if let Some(item_id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            call.item_id = item_id.to_string();
        }
        if let Some(call_id) = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            call.call_id = call_id.to_string();
        }
        if let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            call.name = name.to_string();
        }
    }
    fn merge_tool_event_fields(&mut self, index: u64, event: &Value) {
        let call = self.tool_calls.entry(index).or_default();
        if let Some(item_id) = event
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            call.item_id = item_id.to_string();
        }
        if let Some(call_id) = event
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            call.call_id = call_id.to_string();
        }
        if let Some(name) = event
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            call.name = name.to_string();
        }
    }
    fn complete_tool_call(
        &mut self,
        index: u64,
        arguments: &str,
    ) -> Result<Option<(String, String, String)>, UnsupportedFeatures> {
        let parsed: Value = serde_json::from_str(arguments).map_err(|_| {
            UnsupportedFeatures::single(
                FeatureKind::InvalidToolArguments,
                "/arguments",
                "Responses function call arguments are not valid JSON",
            )
        })?;
        if !parsed.is_object() {
            return Err(UnsupportedFeatures::single(
                FeatureKind::InvalidToolArguments,
                "/arguments",
                "Responses function call arguments must be a JSON object",
            ));
        }
        let call = self.tool_calls.entry(index).or_default();
        call.final_arguments = Some(arguments.to_string());
        if call.call_id.is_empty() || call.name.is_empty() {
            return Ok(None);
        }
        let remaining = if call.arguments.is_empty() {
            arguments.to_string()
        } else if arguments == call.arguments {
            String::new()
        } else if let Some(remaining) = arguments.strip_prefix(&call.arguments) {
            remaining.to_string()
        } else {
            return Err(UnsupportedFeatures::single(
                FeatureKind::InvalidToolArguments,
                "/arguments",
                "Responses function call arguments disagree with prior deltas",
            ));
        };
        call.arguments = arguments.to_string();
        call.completed = true;
        Ok((!remaining.is_empty()).then(|| (call.call_id.clone(), call.name.clone(), remaining)))
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        while let Some(end) = sse::record_end(&self.pending) {
            let record: Vec<u8> = self.pending.drain(..end).collect();
            output.extend(self.record(&record)?);
        }
        Ok(output)
    }
    fn record(&mut self, record: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        let payload = sse::parse_data_payload(record)?;
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_str(&payload).map_err(|_| {
            UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE data is not JSON",
            )
        })?;
        if event.get("type").and_then(Value::as_str) == Some("codex.rate_limits") {
            return Ok(vec![String::from_utf8_lossy(record).into_owned()]);
        }
        let mut output = Vec::new();
        match event.get("type").and_then(Value::as_str) {
            Some("response.created") | Some("response.in_progress") => {
                self.id = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.id)
                    .to_string();
                self.model = event
                    .pointer("/response/model")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.model)
                    .to_string();
                self.role(&mut output);
            }
            Some("response.output_item.added") => {
                self.role(&mut output);
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let index = event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.tool_calls.len() as u64);
                    self.merge_tool_item(index, event.get("item").unwrap_or(&Value::Null));
                }
            }
            Some("response.output_text.delta") => {
                self.role(&mut output);
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    output.push(self.chunk(serde_json::json!({"content":delta}), None));
                }
            }
            Some("response.function_call_arguments.delta") => {
                self.role(&mut output);
                let index = self.tool_index(&event).unwrap_or(0);
                self.merge_tool_event_fields(index, &event);
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    let (call_id, name) = {
                        let entry = self.tool_calls.entry(index).or_default();
                        entry.arguments.push_str(delta);
                        (entry.call_id.clone(), entry.name.clone())
                    };
                    output.push(self.chunk(serde_json::json!({"tool_calls":[{"index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":delta}}]}), None));
                }
            }
            Some("response.function_call_arguments.done") => {
                self.role(&mut output);
                let index = self.tool_index(&event).unwrap_or(0);
                self.merge_tool_event_fields(index, &event);
                let arguments =
                    event
                        .get("arguments")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            UnsupportedFeatures::single(
                                FeatureKind::InvalidToolArguments,
                                "/arguments",
                                "Responses function call completion is missing arguments",
                            )
                        })?;
                if let Some((call_id, name, remaining)) =
                    self.complete_tool_call(index, arguments)?
                {
                    output.push(self.chunk(serde_json::json!({"tool_calls":[{"index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":remaining}}]}), None));
                }
            }
            Some("response.output_item.done") => {
                self.role(&mut output);
                let item = event.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = self
                        .tool_index(&event)
                        .unwrap_or(self.tool_calls.len() as u64);
                    self.merge_tool_item(index, item);
                    if self
                        .tool_calls
                        .get(&index)
                        .is_some_and(|call| call.completed)
                    {
                        return Ok(output);
                    }
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            self.tool_calls
                                .get(&index)
                                .and_then(|call| call.final_arguments.clone())
                        })
                        .ok_or_else(|| {
                            UnsupportedFeatures::single(
                                FeatureKind::InvalidToolArguments,
                                "/item/arguments",
                                "Responses function call completion is missing arguments",
                            )
                        })?;
                    if let Some((call_id, name, remaining)) =
                        self.complete_tool_call(index, &arguments)?
                    {
                        output.push(self.chunk(serde_json::json!({"tool_calls":[{"index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":remaining}}]}), None));
                    }
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                self.role(&mut output);
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.reasoning.push_str(delta);
                    output.push(self.chunk(serde_json::json!({"reasoning_content":delta}), None));
                }
            }
            Some("response.completed") => {
                if self.terminal {
                    return Ok(Vec::new());
                }
                if self.tool_calls.values().any(|call| !call.completed) {
                    return Err(UnsupportedFeatures::single(
                        FeatureKind::InvalidToolArguments,
                        "/output",
                        "Responses stream completed with an incomplete function call",
                    ));
                }
                self.role(&mut output);
                self.usage = usage_from_responses(event.get("response").unwrap_or(&event));
                output.push(sse::data_frame(serde_json::json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[],"usage":{"prompt_tokens":self.usage.input_tokens,"completion_tokens":self.usage.output_tokens,"total_tokens":self.usage.input_tokens+self.usage.output_tokens}})));
                output.push(self.chunk(
                    Value::Object(Map::new()),
                    Some(if self.tool_calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    }),
                ));
                output.push("data: [DONE]\n\n".to_string());
                self.terminal = true;
            }
            Some("response.failed") | Some("response.incomplete") => {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::UnknownEvent,
                    "/type",
                    "Responses upstream failed",
                ))
            }
            Some(_) => {}
            None => {
                return Err(UnsupportedFeatures::single(
                    FeatureKind::UnknownEvent,
                    "/type",
                    "Responses SSE event missing type",
                ))
            }
        }
        Ok(output)
    }
    fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures> {
        if !self.pending.is_empty() {
            return Err(UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE ended mid-record",
            ));
        }
        if self.terminal {
            Ok(Vec::new())
        } else {
            Err(UnsupportedFeatures::single(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE ended without terminal event",
            ))
        }
    }
}

pub struct ResponsesStreamDecoder {
    state: ResponsesChatState,
}
impl ResponsesStreamDecoder {
    pub fn boxed(context: &ConversionContext) -> Box<dyn StreamDecoder + Send + Sync> {
        Box::new(Self {
            state: ResponsesChatState::new(context),
        })
    }
}
impl StreamDecoder for ResponsesStreamDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        self.state.feed(bytes)
    }
    fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures> {
        self.state.finish()
    }
    fn usage(&self) -> Option<Usage> {
        Some(self.state.usage)
    }
}

/// Composition-only streaming decoder: Responses → Chat SSE then the existing
/// Chat → Messages state machine.  There is intentionally no second direct
/// Responses → Messages protocol machine.
pub struct ResponsesMessagesStreamDecoder {
    chat: ResponsesStreamDecoder,
    messages: Box<dyn StreamDecoder + Send + Sync>,
}
impl ResponsesMessagesStreamDecoder {
    pub fn boxed(context: &ConversionContext) -> Box<dyn StreamDecoder + Send + Sync> {
        Box::new(Self {
            chat: ResponsesStreamDecoder {
                state: ResponsesChatState::new(context),
            },
            messages: super::chat::ChatStreamDecoder::boxed(context),
        })
    }
}
impl StreamDecoder for ResponsesMessagesStreamDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        let events = self.chat.feed(bytes)?;
        let mut output = Vec::new();
        for event in events {
            if event.contains("codex.rate_limits") {
                output.push(event);
            } else {
                output.extend(self.messages.feed(event.as_bytes())?);
            }
        }
        Ok(output)
    }
    fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures> {
        let events = self.chat.finish()?;
        let mut output = Vec::new();
        for event in events {
            if event.contains("codex.rate_limits") {
                output.push(event);
            } else {
                output.extend(self.messages.feed(event.as_bytes())?);
            }
        }
        output.extend(self.messages.finish()?);
        Ok(output)
    }
    fn usage(&self) -> Option<Usage> {
        self.chat.usage()
    }
}

pub struct ResponsesMessagesNonStreamDecoder {
    context: ConversionContext,
}
impl ResponsesMessagesNonStreamDecoder {
    pub fn boxed(context: &ConversionContext) -> Box<dyn NonStreamDecoder + Send + Sync> {
        Box::new(Self {
            context: context.clone(),
        })
    }
}
impl NonStreamDecoder for ResponsesMessagesNonStreamDecoder {
    fn decode(&self, body: &Value) -> Result<Value, UnsupportedFeatures> {
        let chat = decode_responses_response_to_chat(body, &self.context)?;
        super::chat::decode_chat_response_to_messages(&chat, &self.context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_request_encodes_function_call_and_text() {
        let request = serde_json::json!({"model":"ignored","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{}"}}]}],"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}]});
        let (encoded, _) = encode_chat_to_responses(&request, "gpt-test").unwrap();
        assert_eq!(encoded["model"], "gpt-test");
        assert_eq!(encoded["input"][1]["type"], "function_call");
    }

    #[test]
    fn chat_request_does_not_synthesize_unsupported_max_output_tokens() {
        let request = serde_json::json!({
            "model": "ignored",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (encoded, _) = encode_chat_to_responses(&request, "gpt-test").unwrap();
        assert!(encoded.get("max_output_tokens").is_none());
    }

    #[test]
    fn responses_stream_terminal_usage_once_and_any_split() {
        let events = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n"
        );
        let context = ConversionContext::new("chatcmpl_1", "m", true);
        let mut expected = None;
        for split in 0..=events.len() {
            let mut decoder = ResponsesStreamDecoder {
                state: ResponsesChatState::new(&context),
            };
            let mut actual = decoder.feed(&events.as_bytes()[..split]).unwrap();
            actual.extend(decoder.feed(&events.as_bytes()[split..]).unwrap());
            actual.extend(decoder.finish().unwrap());
            let joined = actual.concat();
            assert_eq!(joined.matches("[DONE]").count(), 1);
            assert_eq!(joined.matches("\"usage\"").count(), 1);
            if let Some(value) = &expected {
                assert_eq!(&joined, value);
            } else {
                expected = Some(joined);
            }
        }
    }
    #[test]
    fn rate_limits_record_is_unchanged() {
        let record = "event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\",\"x\":1}\n\n";
        let context = ConversionContext::new("x", "m", true);
        let mut decoder = ResponsesStreamDecoder {
            state: ResponsesChatState::new(&context),
        };
        assert_eq!(
            decoder.feed(record.as_bytes()).unwrap(),
            vec![record.to_string()]
        );
    }

    #[test]
    fn accumulator_requires_completed_and_returns_final_response() {
        let mut accumulator = ResponsesEventAccumulator::default();
        accumulator
            .push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n")
            .unwrap();
        assert_eq!(accumulator.finish().unwrap()["id"], "resp_1");
        assert!(ResponsesEventAccumulator::default().finish().is_err());
    }

    #[test]
    fn accumulator_backfills_empty_completed_output_from_item_done() {
        let mut accumulator = ResponsesEventAccumulator::default();
        accumulator
            .push(br#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}

data: {"type":"response.completed","response":{"id":"resp_1","model":"m","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}

"#)
            .unwrap();
        let completed = accumulator.finish().unwrap();
        assert_eq!(completed["output"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn non_stream_decoders_cover_responses_chat_and_messages() {
        let completed = serde_json::json!({
            "id":"resp_1", "model":"m", "status":"completed", "output":[
                {"type":"reasoning", "summary":[{"type":"summary_text", "text":"think"}]},
                {"type":"message", "content":[{"type":"output_text", "text":"answer"}]},
                {"type":"function_call", "call_id":"call_1", "name":"weather", "arguments":"{}"}
            ], "usage":{"input_tokens":2,"output_tokens":3}
        });
        let context = ConversionContext::new("chatcmpl_1", "m", false);
        let chat = decode_responses_response_to_chat(&completed, &context).unwrap();
        assert_eq!(chat["usage"]["total_tokens"], 5);
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        let messages = ResponsesMessagesNonStreamDecoder { context }
            .decode(&completed)
            .unwrap();
        assert_eq!(messages["type"], "message");
    }

    #[test]
    fn non_stream_responses_incomplete_never_becomes_stop() {
        let incomplete = serde_json::json!({
            "id":"resp_1",
            "model":"m",
            "status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"},
            "output":[{"type":"message", "content":[{"type":"output_text", "text":"partial"}]}],
            "usage":{"input_tokens":2,"output_tokens":3}
        });
        let context = ConversionContext::new("chatcmpl_1", "m", false);
        let chat = decode_responses_response_to_chat(&incomplete, &context).unwrap();
        assert_eq!(chat["choices"][0]["finish_reason"], "length");
        let messages = ResponsesMessagesNonStreamDecoder { context }
            .decode(&incomplete)
            .unwrap();
        assert_eq!(messages["stop_reason"], "max_tokens");
    }

    #[test]
    fn non_stream_responses_failed_is_rejected() {
        let failed = serde_json::json!({
            "id":"resp_1",
            "model":"m",
            "status":"failed",
            "output":[],
            "usage":{"input_tokens":1,"output_tokens":0}
        });
        let context = ConversionContext::new("chatcmpl_1", "m", false);
        let error = decode_responses_response_to_chat(&failed, &context).unwrap_err();
        assert!(error.json_pointers.contains(&"/status".to_string()));
    }

    #[test]
    fn stream_rejects_failed_and_missing_terminal() {
        let context = ConversionContext::new("chatcmpl_1", "m", true);
        let mut failed = ResponsesStreamDecoder {
            state: ResponsesChatState::new(&context),
        };
        assert!(failed
            .feed(b"data: {\"type\":\"response.failed\"}\n\n")
            .is_err());
        let mut incomplete = ResponsesStreamDecoder {
            state: ResponsesChatState::new(&context),
        };
        incomplete
            .feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n")
            .unwrap();
        assert!(incomplete.finish().is_err());
    }

    #[test]
    fn stream_function_call_arguments_done_without_delta_emits_executable_tool_call() {
        let context = ConversionContext::new("chatcmpl_1", "m", true);
        let mut decoder = ResponsesStreamDecoder {
            state: ResponsesChatState::new(&context),
        };
        let mut output = decoder
            .feed(
                br#"data: {"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","arguments":"{\"city\":\"Shanghai\"}"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"weather"}}

data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}

"#,
            )
            .unwrap();
        output.extend(decoder.finish().unwrap());
        let output = output.concat();
        assert!(output.contains(r#""id":"call_1"#));
        assert!(output.contains(r#""name":"weather"#));
        assert!(output.contains("Shanghai"));
        assert!(output.contains(r#""finish_reason":"tool_calls"#));
    }

    #[test]
    fn stream_function_call_delta_and_done_complete_without_duplicate_arguments() {
        let context = ConversionContext::new("chatcmpl_1", "m", true);
        let mut decoder = ResponsesStreamDecoder {
            state: ResponsesChatState::new(&context),
        };
        let output = decoder
            .feed(
                br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"weather"}}

data: {"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"city\":"}

data: {"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","arguments":"{\"city\":\"Shanghai\"}"}

data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}

"#,
            )
            .unwrap()
            .concat();
        assert_eq!(output.matches("Shanghai").count(), 1);
        assert!(output.contains(r#""finish_reason":"tool_calls"#));
    }

    #[test]
    fn stream_rejects_invalid_done_function_call_arguments() {
        let context = ConversionContext::new("chatcmpl_1", "m", true);
        let mut decoder = ResponsesStreamDecoder {
            state: ResponsesChatState::new(&context),
        };
        assert!(decoder
            .feed(
                br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"weather"}}

data: {"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","arguments":"not-json"}

"#,
            )
            .is_err());
    }

    #[test]
    fn messages_stream_composes_done_only_function_call() {
        let context = ConversionContext::new("msg_1", "m", true);
        let mut decoder = ResponsesMessagesStreamDecoder {
            chat: ResponsesStreamDecoder {
                state: ResponsesChatState::new(&context),
            },
            messages: crate::protocol::codec::chat::ChatStreamDecoder::boxed(&context),
        };
        let mut output = decoder
            .feed(
                br#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Shanghai\"}"}}

data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}

"#,
            )
            .unwrap();
        output.extend(decoder.finish().unwrap());
        let output = output.concat();
        assert!(output.contains("content_block_start"));
        assert!(output.contains("tool_use"));
        assert!(output.contains("Shanghai"));
    }

    #[test]
    fn unsupported_chat_field_fails_before_encoding() {
        let error = encode_chat_to_responses(
            &serde_json::json!({"model":"m", "messages":[], "metadata":{"x":1}}),
            "m",
        )
        .unwrap_err();
        assert!(error.json_pointers.contains(&"/metadata".to_string()));
    }
}
