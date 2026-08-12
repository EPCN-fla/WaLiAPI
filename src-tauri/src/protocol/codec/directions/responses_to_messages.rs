//! Direct Responses request -> Messages request codec.
//!
//! This module deliberately owns both halves of the direction: the request
//! encoder consumes Responses, while its decoders consume Messages.  Keeping
//! the mapping here avoids a lossy intermediate representation and, in
//! particular, preserves function-call ids and output ordering.

use super::super::{
    direction::CodecDirection,
    error::{DecodeError, FeatureKind, PrepareError, UnsupportedFeatures},
    ports::{DecodedResponse, NonStreamDecoder, StreamDecoder},
    report::{ConversionContext, Usage},
    sse,
    types::{CodecId, Protocol},
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub static RESPONSES_TO_MESSAGES_V2: ResponsesToMessages = ResponsesToMessages;

pub struct ResponsesToMessages;

impl CodecDirection for ResponsesToMessages {
    fn id(&self) -> CodecId {
        CodecId::ResponsesToMessagesV2
    }
    fn downstream(&self) -> Protocol {
        Protocol::Responses
    }
    fn upstream(&self) -> Protocol {
        Protocol::Messages
    }

    fn encode_request(
        &self,
        request: &Value,
        mapped_model: &str,
    ) -> Result<(Value, ConversionContext), PrepareError> {
        encode_request(request, mapped_model).map_err(PrepareError::from)
    }
    fn new_response_decoder(
        &self,
        context: &ConversionContext,
    ) -> Box<dyn NonStreamDecoder + Send + Sync> {
        Box::new(MessagesResponseDecoder {
            context: context.clone(),
        })
    }
    fn new_stream_response_decoder(
        &self,
        context: &ConversionContext,
    ) -> Box<dyn StreamDecoder + Send + Sync> {
        Box::new(MessagesResponsesStream::new(context))
    }
}

fn unsupported(
    kind: FeatureKind,
    pointer: impl Into<String>,
    message: impl Into<String>,
) -> UnsupportedFeatures {
    UnsupportedFeatures::single(kind, pointer, message)
}

/// Encode a Responses request without routing through another protocol.
pub fn encode_request(
    request: &Value,
    mapped_model: &str,
) -> Result<(Value, ConversionContext), UnsupportedFeatures> {
    let object = request.as_object().ok_or_else(|| {
        unsupported(
            FeatureKind::UnsupportedField,
            "/",
            "Responses request must be an object",
        )
    })?;
    let mut normalized = Vec::new();
    for key in object.keys() {
        match key.as_str() {
            "model"
            | "instructions"
            | "input"
            | "tools"
            | "tool_choice"
            | "parallel_tool_calls"
            | "reasoning"
            | "max_output_tokens"
            | "stream"
            | "temperature"
            | "top_p"
            | "stop" => {}
            // These have no Messages wire equivalent but do not alter model output.
            "prompt_cache_key" | "client_metadata" | "metadata" | "include" => {
                normalized.push(format!("/{key}"))
            }
            "store" if object.get(key).and_then(Value::as_bool) == Some(false) => {
                normalized.push(format!("/{key}"))
            }
            "store" => {
                return Err(unsupported(
                    FeatureKind::UnsupportedField,
                    "/store",
                    "store:true has remote-side-effect semantics",
                ))
            }
            "background" => {
                return Err(unsupported(
                    FeatureKind::UnsupportedField,
                    "/background",
                    "background responses are not representable",
                ))
            }
            other => {
                return Err(unsupported(
                    FeatureKind::UnsupportedField,
                    format!("/{other}"),
                    "Responses field is not representable by Messages",
                ))
            }
        }
    }
    let mut messages = Vec::new();
    let mut system = Vec::new();
    if let Some(instructions) = object.get("instructions") {
        system.extend(instructions_to_system(instructions, "/instructions")?);
    }
    if let Some(input) = object.get("input") {
        // Responses accepts a shorthand string in addition to the structured
        // item array.  Normalize it to the same user text message without
        // routing through a Chat representation.
        let items = match input {
            Value::String(text) => vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{"type":"input_text", "text": text}]
            })],
            Value::Array(items) => items.clone(),
            _ => {
                return Err(unsupported(
                    FeatureKind::UnknownBlock,
                    "/input",
                    "input must be a string or array",
                ))
            }
        };
        for (index, item) in items.iter().enumerate() {
            let pointer = format!("/input/{index}");
            match item.get("type").and_then(Value::as_str) {
                Some("message") => messages.push(response_message(item, &pointer)?),
                Some("function_call") => messages.push(function_call_message(item, &pointer)?),
                Some("function_call_output") => {
                    messages.push(function_output_message(item, &pointer)?)
                }
                Some("reasoning") => messages.push(reasoning_message(item, &pointer)?),
                Some(other) => {
                    return Err(unsupported(
                        FeatureKind::UnknownBlock,
                        format!("{pointer}/type"),
                        format!("Responses input item {other:?} is not representable"),
                    ))
                }
                None => {
                    return Err(unsupported(
                        FeatureKind::UnknownBlock,
                        format!("{pointer}/type"),
                        "Responses input item requires type",
                    ))
                }
            }
        }
    }
    let mut out = Map::new();
    out.insert("model".into(), Value::String(mapped_model.into()));
    out.insert("messages".into(), Value::Array(messages));
    out.insert(
        "max_tokens".into(),
        object.get("max_output_tokens").cloned().unwrap_or_else(|| {
            normalized.push("/max_output_tokens".into());
            Value::from(32000)
        }),
    );
    out.insert(
        "stream".into(),
        Value::Bool(
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    if !system.is_empty() {
        out.insert("system".into(), Value::Array(system));
    }
    for field in ["temperature", "top_p", "stop"] {
        if let Some(value) = object.get(field) {
            out.insert(
                if field == "stop" {
                    "stop_sequences".into()
                } else {
                    field.into()
                },
                value.clone(),
            );
        }
    }
    if let Some(tools) = object.get("tools") {
        out.insert("tools".into(), response_tools(tools, "/tools")?);
    }
    if let Some(choice) = object.get("tool_choice") {
        out.insert(
            "tool_choice".into(),
            response_tool_choice(choice, "/tool_choice")?,
        );
    }
    if object.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
        normalized.push("/parallel_tool_calls".into());
        let choice = out
            .entry("tool_choice")
            .or_insert_with(|| serde_json::json!({"type":"auto"}));
        if let Some(choice) = choice.as_object_mut() {
            choice.insert("disable_parallel_tool_use".into(), Value::Bool(true));
        }
    }
    if let Some(effort) = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        out.insert(
            "thinking".into(),
            serde_json::json!({"type":"enabled", "budget_tokens": effort_budget(effort)}),
        );
        out.insert(
            "output_config".into(),
            serde_json::json!({"effort": crate::protocol::thinking::map_effort_to_claude(effort)}),
        );
        normalized.push("/reasoning/effort".into());
    }
    let mut context = ConversionContext::new(
        format!("resp_{}", uuid::Uuid::new_v4().simple()),
        mapped_model,
        object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    context.normalized = normalized;
    Ok((Value::Object(out), context))
}

fn instructions_to_system(value: &Value, pointer: &str) -> Result<Vec<Value>, UnsupportedFeatures> {
    match value {
        Value::String(text) => Ok(vec![serde_json::json!({"type":"text", "text":text})]),
        Value::Array(parts) => parts
            .iter()
            .enumerate()
            .map(|(i, part)| match part.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("text") => part
                    .get("text")
                    .cloned()
                    .map(|text| serde_json::json!({"type":"text", "text":text}))
                    .ok_or_else(|| {
                        unsupported(
                            FeatureKind::UnknownBlock,
                            format!("{pointer}/{i}/text"),
                            "instruction text is required",
                        )
                    }),
                Some(other) => Err(unsupported(
                    FeatureKind::UnknownBlock,
                    format!("{pointer}/{i}/type"),
                    format!("instruction block {other:?} is not representable"),
                )),
                None => Err(unsupported(
                    FeatureKind::UnknownBlock,
                    format!("{pointer}/{i}/type"),
                    "instruction block requires type",
                )),
            })
            .collect(),
        _ => Err(unsupported(
            FeatureKind::UnknownBlock,
            pointer,
            "instructions must be text or an array of text blocks",
        )),
    }
}

fn response_message(item: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let role = item
        .get("role")
        .and_then(Value::as_str)
        .filter(|role| matches!(*role, "user" | "assistant"))
        .ok_or_else(|| {
            unsupported(
                FeatureKind::UnknownRole,
                format!("{pointer}/role"),
                "message role must be user or assistant",
            )
        })?;
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            unsupported(
                FeatureKind::UnknownBlock,
                format!("{pointer}/content"),
                "message content must be an array",
            )
        })?;
    let mut blocks = Vec::new();
    if let Some(reasoning) = item.get("reasoning_content") {
        blocks.push(serde_json::json!({
            "type": "thinking",
            "thinking": readable_reasoning(reasoning, &format!("{pointer}/reasoning_content"))?
        }));
    }
    for (index, part) in content.iter().enumerate() {
        let p = format!("{pointer}/content/{index}");
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => blocks.push(serde_json::json!({"type":"text", "text": part.get("text").and_then(Value::as_str).ok_or_else(|| unsupported(FeatureKind::UnknownBlock, format!("{p}/text"), "text is required"))?})),
            Some("input_image") if role == "user" => blocks.push(response_image(part, &p)?),
            Some("input_image") => return Err(unsupported(FeatureKind::Media, p, "image input is only valid for a user message")),
            Some(other) => return Err(unsupported(FeatureKind::UnknownBlock, format!("{p}/type"), format!("content type {other:?} is not representable"))),
            None => return Err(unsupported(FeatureKind::UnknownBlock, format!("{p}/type"), "content part requires type")),
        }
    }
    Ok(serde_json::json!({"role":role, "content":blocks}))
}

/// Responses reasoning is replay context: its readable summary/content must
/// survive conversion so providers that require the chain on the next turn can
/// accept the request.  Opaque/encrypted forms are deliberately rejected;
/// recording them as merely "normalized" would silently change the turn.
fn reasoning_message(item: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let text = if let Some(summary) = item.get("summary") {
        readable_reasoning(summary, &format!("{pointer}/summary"))?
    } else if let Some(content) = item.get("content") {
        readable_reasoning(content, &format!("{pointer}/content"))?
    } else {
        return Err(unsupported(
            FeatureKind::UnknownBlock,
            pointer,
            "reasoning item has no readable summary or content",
        ));
    };
    Ok(serde_json::json!({"role":"assistant", "content":[{"type":"thinking", "thinking":text}]}))
}

fn readable_reasoning(value: &Value, pointer: &str) -> Result<String, UnsupportedFeatures> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                unsupported(
                    FeatureKind::UnknownBlock,
                    pointer,
                    "reasoning object must contain readable text",
                )
            }),
        Value::Array(parts) => {
            let mut text = String::new();
            for (index, part) in parts.iter().enumerate() {
                let p = format!("{pointer}/{index}");
                match part.get("type").and_then(Value::as_str) {
                    Some("summary_text") | Some("output_text") | Some("input_text")
                    | Some("text") => {
                        text.push_str(part.get("text").and_then(Value::as_str).ok_or_else(
                            || {
                                unsupported(
                                    FeatureKind::UnknownBlock,
                                    format!("{p}/text"),
                                    "reasoning text is required",
                                )
                            },
                        )?);
                    }
                    Some(other) => {
                        return Err(unsupported(
                            FeatureKind::UnknownBlock,
                            format!("{p}/type"),
                            format!("reasoning part {other:?} is not readable"),
                        ))
                    }
                    None => {
                        return Err(unsupported(
                            FeatureKind::UnknownBlock,
                            format!("{p}/type"),
                            "reasoning part type is required",
                        ))
                    }
                }
            }
            if text.is_empty() {
                Err(unsupported(
                    FeatureKind::UnknownBlock,
                    pointer,
                    "reasoning has no readable text",
                ))
            } else {
                Ok(text)
            }
        }
        _ => Err(unsupported(
            FeatureKind::UnknownBlock,
            pointer,
            "reasoning must be readable text or text parts",
        )),
    }
}

fn response_image(part: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let url = part
        .get("image_url")
        .or_else(|| part.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            unsupported(
                FeatureKind::Media,
                format!("{pointer}/image_url"),
                "image URL is required",
            )
        })?;
    if !(url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("data:image/"))
    {
        return Err(unsupported(
            FeatureKind::Media,
            format!("{pointer}/image_url"),
            "image URL must be http(s) or image data URI",
        ));
    }
    if url.len() > super::super::request::MAX_IMAGE_BYTES * 2 {
        return Err(unsupported(
            FeatureKind::Media,
            format!("{pointer}/image_url"),
            "image exceeds supported maximum",
        ));
    }
    if let Some((header, data)) = url.strip_prefix("data:").and_then(|v| v.split_once(',')) {
        let media_type = header.split(';').next().unwrap_or_default();
        return Ok(
            serde_json::json!({"type":"image", "source":{"type":"base64", "media_type":media_type, "data":data}}),
        );
    }
    Ok(serde_json::json!({"type":"image", "source":{"type":"url", "url":url}}))
}

fn function_call_message(item: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let id = required(item, "call_id", pointer)?;
    let name = required(item, "name", pointer)?;
    let args = item
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            unsupported(
                FeatureKind::InvalidToolArguments,
                format!("{pointer}/arguments"),
                "arguments must be a JSON string",
            )
        })?;
    let input: Value = serde_json::from_str(args).map_err(|_| {
        unsupported(
            FeatureKind::InvalidToolArguments,
            format!("{pointer}/arguments"),
            "arguments must be valid JSON",
        )
    })?;
    if !input.is_object() {
        return Err(unsupported(
            FeatureKind::InvalidToolArguments,
            format!("{pointer}/arguments"),
            "arguments must be a JSON object",
        ));
    }
    Ok(
        serde_json::json!({"role":"assistant", "content":[{"type":"tool_use", "id":id, "name":name, "input":input}]}),
    )
}
fn function_output_message(item: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let id = required(item, "call_id", pointer)?;
    let content = messages_tool_result_content(item.get("output"), &format!("{pointer}/output"))?;
    Ok(
        serde_json::json!({"role":"user", "content":[{"type":"tool_result", "tool_use_id":id, "content":content}]}),
    )
}

fn messages_tool_result_content(
    value: Option<&Value>,
    pointer: &str,
) -> Result<Value, UnsupportedFeatures> {
    match value.unwrap_or(&Value::Null) {
        Value::Null => Ok(Value::String(String::new())),
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let p = format!("{pointer}/{index}");
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| serde_json::json!({"type":"text", "text":text}))
                        .ok_or_else(|| {
                            unsupported(
                                FeatureKind::UnknownBlock,
                                format!("{p}/text"),
                                "tool-result text is required",
                            )
                        }),
                    Some(other) => Err(unsupported(
                        FeatureKind::UnknownBlock,
                        format!("{p}/type"),
                        format!("function output part {other:?} is not representable by Messages"),
                    )),
                    None => Err(unsupported(
                        FeatureKind::UnknownBlock,
                        format!("{p}/type"),
                        "function output part type is required",
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(unsupported(
            FeatureKind::UnknownBlock,
            pointer,
            "function output must be text or text parts",
        )),
    }
}
fn required<'a>(
    value: &'a Value,
    field: &str,
    pointer: &str,
) -> Result<&'a str, UnsupportedFeatures> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            unsupported(
                FeatureKind::MissingToolField,
                format!("{pointer}/{field}"),
                format!("{field} is required"),
            )
        })
}

fn response_tools(value: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    let values = value.as_array().ok_or_else(|| {
        unsupported(
            FeatureKind::UnsupportedField,
            pointer,
            "tools must be an array",
        )
    })?;
    values
        .iter()
        .enumerate()
        .map(|(i, tool)| {
            let p = format!("{pointer}/{i}");
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(unsupported(
                    FeatureKind::BuiltinTool,
                    format!("{p}/type"),
                    "only function tools are representable",
                ));
            }
            let name = required(tool, "name", &p)?;
            let schema = tool
                .get("parameters")
                .or_else(|| tool.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type":"object", "properties":{}}));
            if !schema.is_object() {
                return Err(unsupported(
                    FeatureKind::InvalidToolArguments,
                    format!("{p}/parameters"),
                    "tool schema must be an object",
                ));
            }
            let mut out = serde_json::json!({"name":name,"input_schema":schema});
            if let Some(d) = tool.get("description") {
                out["description"] = d.clone();
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}
fn response_tool_choice(value: &Value, pointer: &str) -> Result<Value, UnsupportedFeatures> {
    match value {
        Value::String(s) if matches!(s.as_str(), "auto" | "required" | "none") => {
            Ok(serde_json::json!({"type": if s == "required" { "any" } else { s }}))
        }
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let name = required(value, "name", pointer)?;
            Ok(serde_json::json!({"type":"tool","name":name}))
        }
        _ => Err(unsupported(
            FeatureKind::UnsupportedField,
            pointer,
            "unsupported Responses tool_choice",
        )),
    }
}
fn effort_budget(effort: &str) -> u64 {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" => 512,
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        _ => 32768,
    }
}

struct MessagesResponseDecoder {
    context: ConversionContext,
}
impl NonStreamDecoder for MessagesResponseDecoder {
    fn decode(&self, body: &Value) -> Result<DecodedResponse, DecodeError> {
        decode_messages_response(body, &self.context).map_err(DecodeError::from)
    }
}

pub fn decode_messages_response(
    body: &Value,
    context: &ConversionContext,
) -> Result<DecodedResponse, UnsupportedFeatures> {
    if body.get("type").and_then(Value::as_str) != Some("message") {
        return Err(unsupported(
            FeatureKind::UnknownEvent,
            "/type",
            "Messages response must have type=message",
        ));
    }
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            unsupported(
                FeatureKind::UnknownEvent,
                "/content",
                "Messages response requires content array",
            )
        })?;
    let mut output = Vec::new();
    for (i, block) in content.iter().enumerate() {
        let p = format!("/content/{i}");
        match block.get("type").and_then(Value::as_str) { Some("text") => output.push(serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":block.get("text").and_then(Value::as_str).unwrap_or("")}]})), Some("thinking") => { if let Some(text)=block.get("thinking").and_then(Value::as_str) { output.push(serde_json::json!({"type":"reasoning","summary":[{"type":"summary_text","text":text}]})); } }, Some("redacted_thinking") => {}, Some("tool_use") => { let id=required(block,"id",&p)?; let name=required(block,"name",&p)?; let input=block.get("input").ok_or_else(|| unsupported(FeatureKind::MissingToolField,format!("{p}/input"),"tool input is required"))?; if !input.is_object() { return Err(unsupported(FeatureKind::InvalidToolArguments,format!("{p}/input"),"tool input must be an object")); } output.push(serde_json::json!({"type":"function_call","call_id":id,"name":name,"arguments":serde_json::to_string(input).map_err(|_| unsupported(FeatureKind::InvalidToolArguments,format!("{p}/input"),"tool input could not be serialized"))?})); }, Some(other)=>return Err(unsupported(FeatureKind::UnknownBlock,format!("{p}/type"),format!("Messages response block {other:?} is unsupported"))), None=>return Err(unsupported(FeatureKind::UnknownBlock,format!("{p}/type"),"content block type is required")) }
    }
    let usage = usage_from_messages(body);
    let status = match body.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn" | "stop_sequence" | "refusal" | "pause_turn") => "completed",
        Some("tool_use") => "completed",
        Some("max_tokens" | "model_context_window_exceeded") => "incomplete",
        Some(other) => {
            return Err(unsupported(
                FeatureKind::UnknownFinishReason,
                "/stop_reason",
                format!("unknown Messages stop reason {other:?}"),
            ))
        }
        None => {
            return Err(unsupported(
                FeatureKind::UnknownFinishReason,
                "/stop_reason",
                "Messages response is missing stop_reason",
            ))
        }
    };
    let mut response = serde_json::json!({"id":body.get("id").and_then(Value::as_str).unwrap_or(&context.request_id),"object":"response","model":body.get("model").and_then(Value::as_str).unwrap_or(&context.upstream_model),"status":status,"output":output,"usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"total_tokens":usage.input_tokens+usage.output_tokens}});
    if status == "incomplete" {
        response["incomplete_details"] = serde_json::json!({"reason":"max_output_tokens"});
    }
    Ok(DecodedResponse {
        body: response,
        usage: Some(usage),
    })
}
fn usage_from_messages(value: &Value) -> Usage {
    let input = value.pointer("/usage/input_tokens").and_then(Value::as_u64);
    let output = value
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64);
    Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        cache_creation_input_tokens: value
            .pointer("/usage/cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_input_tokens: value
            .pointer("/usage/cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        usage_unknown: input.is_none() || output.is_none(),
    }
}

fn merge_usage(usage: &mut Usage, value: &Value) {
    if let Some(input) = value.get("input_tokens").and_then(Value::as_u64) {
        usage.input_tokens = input;
    }
    if let Some(output) = value.get("output_tokens").and_then(Value::as_u64) {
        usage.output_tokens = output;
    }
    if let Some(cache_creation) = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        usage.cache_creation_input_tokens = cache_creation;
    }
    if let Some(cache_read) = value.get("cache_read_input_tokens").and_then(Value::as_u64) {
        usage.cache_read_input_tokens = cache_read;
    }
    usage.usage_unknown =
        value.get("input_tokens").is_none() || value.get("output_tokens").is_none();
}

// Streaming Messages -> Responses.  The implementation buffers only whole SSE
// records and derives item ids from the source block index, so arbitrary byte
// splits cannot change the output sequence.
struct MessagesResponsesStream {
    pending: Vec<u8>,
    id: String,
    model: String,
    started: bool,
    terminal: bool,
    usage: Usage,
    blocks: BTreeMap<usize, Block>,
    stop: Option<String>,
}
#[derive(Default)]
struct Block {
    item_id: String,
    kind: String,
    id: String,
    name: String,
    text: String,
    args: String,
    stopped: bool,
}
impl MessagesResponsesStream {
    fn new(c: &ConversionContext) -> Self {
        Self {
            pending: Vec::new(),
            id: c.request_id.clone(),
            model: c.upstream_model.clone(),
            started: false,
            terminal: false,
            usage: Usage {
                usage_unknown: true,
                ..Usage::default()
            },
            blocks: BTreeMap::new(),
            stop: None,
        }
    }
    fn frame(name: &str, value: Value) -> String {
        sse::event(name, value)
    }
    fn start(&mut self, out: &mut Vec<String>) {
        if !self.started {
            self.started = true;
            let r = serde_json::json!({"id":self.id,"object":"response","model":self.model,"status":"in_progress","output":[]});
            out.push(Self::frame(
                "response.created",
                serde_json::json!({"type":"response.created","response":r}),
            ));
            out.push(Self::frame("response.in_progress",serde_json::json!({"type":"response.in_progress","response":{"id":self.id,"model":self.model}})));
        }
    }
}
impl StreamDecoder for MessagesResponsesStream {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, DecodeError> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = sse::record_end(&self.pending) {
            let rec: Vec<u8> = self.pending.drain(..end).collect();
            out.extend(self.record(&rec).map_err(DecodeError::from)?);
        }
        Ok(out)
    }
    fn finish(&mut self) -> Result<Vec<String>, DecodeError> {
        if !self.pending.is_empty() {
            return Err(DecodeError::from(unsupported(
                FeatureKind::UnknownEvent,
                "/",
                "Messages SSE ended mid-record",
            )));
        }
        if !self.terminal {
            return Err(DecodeError::from(unsupported(
                FeatureKind::UnknownEvent,
                "/",
                "Messages SSE ended without message_stop",
            )));
        }
        Ok(Vec::new())
    }
    fn usage(&self) -> Option<Usage> {
        Some(self.usage)
    }
}
impl MessagesResponsesStream {
    fn record(&mut self, record: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        let payload = sse::parse_data_payload(record)?;
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_str(&payload).map_err(|_| {
            unsupported(
                FeatureKind::UnknownEvent,
                "/",
                "Messages SSE data is not JSON",
            )
        })?;
        if event.get("type").and_then(Value::as_str) == Some("codex.rate_limits") {
            return Ok(vec![String::from_utf8_lossy(record).into_owned()]);
        }
        let ty = event.get("type").and_then(Value::as_str).ok_or_else(|| {
            unsupported(
                FeatureKind::UnknownEvent,
                "/type",
                "Messages SSE frame type is required",
            )
        })?;
        let mut out = Vec::new();
        match ty {
            "message_start" => {
                if let Some(m) = event.get("message") {
                    self.id = m
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(&self.id)
                        .to_string();
                    self.model = m
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or(&self.model)
                        .to_string();
                    if let Some(u) = m.get("usage") {
                        merge_usage(&mut self.usage, u);
                    }
                }
                self.start(&mut out)
            }
            "content_block_start" => {
                self.start(&mut out);
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let b = event.get("content_block").unwrap_or(&Value::Null);
                let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
                let item_id = format!("item_{index}");
                let mut block = Block {
                    item_id: item_id.clone(),
                    kind: kind.into(),
                    ..Block::default()
                };
                match kind {
                    "text" => {
                        out.push(Self::frame("response.output_item.added",serde_json::json!({"type":"response.output_item.added","output_index":index,"item":{"id":item_id,"type":"message","role":"assistant","content":[]}})));
                        out.push(Self::frame("response.content_part.added", serde_json::json!({"type":"response.content_part.added","output_index":index,"content_index":0,"part":{"type":"output_text","text":""}})));
                    }
                    "thinking" => {
                        out.push(Self::frame("response.output_item.added",serde_json::json!({"type":"response.output_item.added","output_index":index,"item":{"id":item_id,"type":"reasoning","summary":[]}})));
                        out.push(Self::frame("response.reasoning_summary_part.added", serde_json::json!({"type":"response.reasoning_summary_part.added","output_index":index,"summary_index":0,"part":{"type":"reasoning_summary_text","text":""}})));
                    }
                    "tool_use" => {
                        let id = required(b, "id", "/content_block_start/content_block")?;
                        let name = required(b, "name", "/content_block_start/content_block")?;
                        block.id = id.to_string();
                        block.name = name.to_string();
                        out.push(Self::frame("response.output_item.added",serde_json::json!({"type":"response.output_item.added","output_index":index,"item":{"id":item_id,"type":"function_call","call_id":id,"name":name,"arguments":""}})))
                    }
                    _ => {
                        return Err(unsupported(
                            FeatureKind::UnknownBlock,
                            "/content_block_start/content_block/type",
                            "unknown Messages content block",
                        ))
                    }
                }
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        let b = self.blocks.get_mut(&index).ok_or_else(|| {
                            unsupported(
                                FeatureKind::UnknownEvent,
                                "/index",
                                "text delta references unknown block",
                            )
                        })?;
                        b.text.push_str(text);
                        out.push(Self::frame("response.output_text.delta",serde_json::json!({"type":"response.output_text.delta","output_index":index,"item_id":b.item_id,"delta":text})))
                    }
                    Some("thinking_delta") => {
                        let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        let b = self.blocks.get_mut(&index).ok_or_else(|| {
                            unsupported(
                                FeatureKind::UnknownEvent,
                                "/index",
                                "thinking delta references unknown block",
                            )
                        })?;
                        b.text.push_str(text);
                        out.push(Self::frame("response.reasoning_summary_text.delta",serde_json::json!({"type":"response.reasoning_summary_text.delta","output_index":index,"delta":text})))
                    }
                    Some("input_json_delta") => {
                        let text = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let b = self.blocks.get_mut(&index).ok_or_else(|| {
                            unsupported(
                                FeatureKind::UnknownEvent,
                                "/index",
                                "tool delta references unknown block",
                            )
                        })?;
                        b.args.push_str(text);
                        out.push(Self::frame("response.function_call_arguments.delta",serde_json::json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":b.item_id,"delta":text})))
                    }
                    Some("signature_delta") => {}
                    Some(other) => {
                        return Err(unsupported(
                            FeatureKind::UnknownEvent,
                            "/delta/type",
                            format!("unknown Messages delta {other:?}"),
                        ))
                    }
                    None => {
                        return Err(unsupported(
                            FeatureKind::UnknownEvent,
                            "/delta/type",
                            "Messages delta type is required",
                        ))
                    }
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let b = self.blocks.get_mut(&index).ok_or_else(|| {
                    unsupported(
                        FeatureKind::UnknownEvent,
                        "/index",
                        "block stop references unknown block",
                    )
                })?;
                if b.stopped {
                    return Err(unsupported(
                        FeatureKind::UnknownEvent,
                        "/index",
                        "duplicate content block stop",
                    ));
                }
                b.stopped = true;
                if b.kind == "text" {
                    out.push(Self::frame("response.output_text.done", serde_json::json!({"type":"response.output_text.done","output_index":index,"content_index":0,"text":b.text})));
                    out.push(Self::frame("response.content_part.done", serde_json::json!({"type":"response.content_part.done","output_index":index,"content_index":0,"part":{"type":"output_text","text":b.text}})));
                } else if b.kind == "thinking" {
                    out.push(Self::frame("response.reasoning_summary_text.done", serde_json::json!({"type":"response.reasoning_summary_text.done","output_index":index,"summary_index":0,"text":b.text})));
                    out.push(Self::frame("response.reasoning_summary_part.done", serde_json::json!({"type":"response.reasoning_summary_part.done","output_index":index,"summary_index":0,"part":{"type":"reasoning_summary_text","text":b.text}})));
                } else if b.kind == "tool_use" {
                    let p: Value = serde_json::from_str(&b.args).map_err(|_| {
                        unsupported(
                            FeatureKind::InvalidToolArguments,
                            "/delta/partial_json",
                            "tool arguments are not valid JSON",
                        )
                    })?;
                    if !p.is_object() {
                        return Err(unsupported(
                            FeatureKind::InvalidToolArguments,
                            "/delta/partial_json",
                            "tool arguments must be an object",
                        ));
                    }
                    out.push(Self::frame("response.function_call_arguments.done",serde_json::json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":b.item_id,"arguments":b.args})));
                }
                let item = match b.kind.as_str() {
                    "text" => {
                        serde_json::json!({"id":b.item_id,"type":"message","role":"assistant","content":[{"type":"output_text","text":b.text}]})
                    }
                    "thinking" => {
                        serde_json::json!({"id":b.item_id,"type":"reasoning","summary":[{"type":"summary_text","text":b.text}]})
                    }
                    "tool_use" => {
                        serde_json::json!({"id":b.item_id,"type":"function_call","call_id":b.id,"name":b.name,"arguments":b.args})
                    }
                    _ => unreachable!("validated at content_block_start"),
                };
                out.push(Self::frame("response.output_item.done",serde_json::json!({"type":"response.output_item.done","output_index":index,"item":item})))
            }
            "message_delta" => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(reason.into())
                }
                if let Some(u) = event.get("usage") {
                    merge_usage(&mut self.usage, u);
                }
            }
            "message_stop" => {
                if self.terminal {
                    return Err(unsupported(
                        FeatureKind::UnknownEvent,
                        "/type",
                        "duplicate message_stop",
                    ));
                }
                self.start(&mut out);
                let status = match self.stop.as_deref() {
                    Some("max_tokens" | "model_context_window_exceeded") => "incomplete",
                    Some("end_turn" | "stop_sequence" | "refusal" | "pause_turn" | "tool_use") => {
                        "completed"
                    }
                    Some(x) => {
                        return Err(unsupported(
                            FeatureKind::UnknownFinishReason,
                            "/delta/stop_reason",
                            format!("unknown stop reason {x:?}"),
                        ))
                    }
                    None => {
                        return Err(unsupported(
                            FeatureKind::UnknownFinishReason,
                            "/delta/stop_reason",
                            "message_stop without stop reason",
                        ))
                    }
                };
                let output = self.blocks.values().filter_map(|block| match block.kind.as_str() {
                    "text" => Some(serde_json::json!({"id":block.item_id,"type":"message","role":"assistant","content":[{"type":"output_text","text":block.text}]})),
                    "thinking" => Some(serde_json::json!({"id":block.item_id,"type":"reasoning","summary":[{"type":"summary_text","text":block.text}]})),
                    "tool_use" => Some(serde_json::json!({"id":block.item_id,"type":"function_call","call_id":block.id,"name":block.name,"arguments":block.args})),
                    _ => None,
                }).collect::<Vec<_>>();
                let mut response = serde_json::json!({"id":self.id,"object":"response","model":self.model,"status":status,"output":output,"usage":{"input_tokens":self.usage.input_tokens,"output_tokens":self.usage.output_tokens,"total_tokens":self.usage.input_tokens+self.usage.output_tokens}});
                if status == "incomplete" {
                    response["incomplete_details"] =
                        serde_json::json!({"reason":"max_output_tokens"});
                }
                out.push(Self::frame(
                    "response.completed",
                    serde_json::json!({"type":"response.completed","response":response}),
                ));
                out.push("data: [DONE]\n\n".into());
                self.terminal = true
            }
            _ => {
                return Err(unsupported(
                    FeatureKind::UnknownEvent,
                    "/type",
                    format!("unknown Messages SSE event {ty:?}"),
                ))
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_is_direct_and_preserves_tool_identity() {
        let (out,_)=encode_request(&serde_json::json!({"input":[{"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Shanghai\"}"}]}),"m").unwrap();
        assert_eq!(out["messages"][0]["content"][0]["id"], "call_1");
        assert_eq!(out["max_tokens"], 32000);
    }
    #[test]
    fn request_preserves_readable_reasoning_and_item_order() {
        let (out, _) = encode_request(&serde_json::json!({"input":[
            {"type":"reasoning", "summary":[{"type":"summary_text", "text":"think"}]},
            {"type":"message", "role":"assistant", "reasoning_content":"direct", "content":[{"type":"output_text", "text":"answer"}]},
            {"type":"function_call", "call_id":"call_1", "name":"lookup", "arguments":"{}"},
            {"type":"function_call_output", "call_id":"call_1", "output":"result"}
        ]}), "m").unwrap();
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["content"][0]["thinking"], "think");
        assert_eq!(messages[1]["content"][0]["thinking"], "direct");
        assert_eq!(messages[2]["content"][0]["type"], "tool_use");
        assert_eq!(messages[3]["content"][0]["type"], "tool_result");

        assert!(encode_request(&serde_json::json!({"input":[{"type":"reasoning", "summary":[{"type":"encrypted_content"}]}]}), "m").is_err());
    }
    #[test]
    fn response_maps_tool_input() {
        let c = ConversionContext::new("r", "m", false);
        let out=decode_messages_response(&serde_json::json!({"type":"message","content":[{"type":"tool_use","id":"call_1","name":"weather","input":{"city":"Shanghai"}}],"stop_reason":"tool_use","usage":{"input_tokens":2,"output_tokens":1}}),&c).unwrap();
        assert_eq!(out.body["output"][0]["call_id"], "call_1");
    }

    #[test]
    fn stream_is_split_invariant_and_emits_a_single_terminal() {
        let source = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"usage\":{\"input_tokens\":2}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let context = ConversionContext::new("resp_1", "m", true);
        let mut expected = None;
        for split in 0..=source.len() {
            let mut decoder = MessagesResponsesStream::new(&context);
            let mut events = decoder.feed(&source.as_bytes()[..split]).unwrap();
            events.extend(decoder.feed(&source.as_bytes()[split..]).unwrap());
            events.extend(decoder.finish().unwrap());
            if let Some(expected) = &expected {
                assert_eq!(&events, expected);
            } else {
                expected = Some(events);
            }
        }
        let events = expected.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.contains("response.completed"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "data: [DONE]\n\n")
                .count(),
            1
        );
    }

    #[test]
    fn stream_emits_complete_responses_lifecycle_for_text_reasoning_and_tool() {
        let source = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"model\":\"m\",\"usage\":{\"input_tokens\":2}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"think\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"lookup\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let context = ConversionContext::new("r", "m", true);
        let mut decoder = MessagesResponsesStream::new(&context);
        let mut events = decoder.feed(source.as_bytes()).unwrap();
        events.extend(decoder.finish().unwrap());
        let output = events.join("");
        for event in [
            "response.content_part.added",
            "response.output_text.done",
            "response.content_part.done",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ] {
            assert!(output.contains(event), "missing {event}");
        }
        assert!(output.contains("\"text\":\"hello\""));
        assert!(output.contains("\"text\":\"think\""));
        assert!(output.contains("\"arguments\":\"{}\""));
    }
}
