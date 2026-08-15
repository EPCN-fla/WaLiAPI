use super::super::super::{
    error::{FeatureKind, UnsupportedFeatures},
    report::ConversionContext,
    request::MAX_IMAGE_BYTES,
};
use super::{required, unsupported};
use serde_json::{Map, Value};

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
    if url.len() > MAX_IMAGE_BYTES * 2 {
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
