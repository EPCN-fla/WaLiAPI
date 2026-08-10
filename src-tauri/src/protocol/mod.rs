pub mod anthropic;
pub mod codec;
pub mod responses;
pub mod sse_bridge;
pub mod thinking;

use serde_json::Value;

/// Extract API key from either `Authorization: Bearer xxx` or `x-api-key: xxx` header.
pub fn extract_api_key(headers: &axum::http::HeaderMap) -> Option<String> {
    // Try Authorization: Bearer xxx first
    if let Some(auth) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        if let Some(key) = auth.strip_prefix("Bearer ") {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    // Fall back to x-api-key
    if let Some(key) = headers.get("x-api-key").and_then(|h| h.to_str().ok()) {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Detect if a request is in Anthropic format by checking headers and body.
#[allow(dead_code)]
pub fn is_anthropic_request(headers: &axum::http::HeaderMap, body: &Value) -> bool {
    // Check for anthropic-version header
    if headers.contains_key("anthropic-version") {
        return true;
    }
    // Check for x-api-key without Authorization Bearer
    if headers.contains_key("x-api-key") && !headers.contains_key("authorization") {
        return true;
    }
    // Check body: Anthropic format uses "max_tokens" but not "messages" with OpenAI structure
    // Actually both use "messages", so rely on headers primarily.
    // As a fallback, check if body has "max_tokens" but not "model" (unlikely to help).
    // The header-based detection is the primary signal.
    let _ = body;
    false
}

/// Detect if a request targets the Responses API format.
#[allow(dead_code)]
pub fn is_responses_request(body: &Value) -> bool {
    // Responses API uses "input" instead of "messages"
    body.get("input").is_some() && body.get("messages").is_none()
}

/// Convert OpenAI Chat Completions response to Responses API format.
pub fn openai_to_responses(openai_resp: &Value, model: &str) -> Value {
    let choice = openai_resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    let message = choice.and_then(|ch| ch.get("message"));

    let content = message
        .and_then(|msg| msg.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    let finish_reason = choice
        .and_then(|ch| ch.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop");

    let prompt_tokens = openai_resp
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let completion_tokens = openai_resp
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    // Build output array: message + function_call items
    let mut output = Vec::new();

    // Add function_call outputs for tool_calls
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tool_calls {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let arguments = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let call_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            output.push(serde_json::json!({
                "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed"
            }));
        }
    }

    // Add text message output (always include, even if empty when tool_calls present)
    if !content.is_empty() || output.is_empty() {
        output.push(serde_json::json!({
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": content
            }],
            "status": "completed"
        }));
    }

    serde_json::json!({
        "id": openai_resp.get("id").cloned().unwrap_or(Value::String(format!("resp_{}", uuid::Uuid::new_v4()))),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        },
        "status": "completed",
        "finish_reason": finish_reason
    })
}

/// Normalize a Responses API `tool_choice` to OpenAI Chat Completions shape.
///
/// Responses API accepts either a bare string ("auto" | "none" | "required") or
/// an object — `{"type": "auto"|"none"|"required"}` or
/// `{"type": "function", "name": "foo"}`. Chat Completions wants a bare string
/// or `{"type": "function", "function": {"name": "foo"}}`. Returns `None` when
/// the value cannot be represented (caller then drops tool_choice).
fn responses_tool_choice_to_chat(tc: &Value) -> Option<Value> {
    if let Some(s) = tc.as_str() {
        return Some(Value::String(s.to_string()));
    }
    let obj = tc.as_object()?;
    let ty = obj.get("type")?.as_str()?;
    match ty {
        "auto" | "none" | "required" => Some(Value::String(ty.to_string())),
        "function" => {
            let name = obj.get("name")?.as_str()?;
            if name.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "type": "function",
                "function": {"name": name}
            }))
        }
        _ => None,
    }
}

/// Convert Responses API request to OpenAI Chat Completions format.
pub fn responses_to_openai(
    body: &Value,
) -> Result<Value, crate::protocol::codec::UnsupportedFeatures> {
    const SUPPORTED_TOP_LEVEL: &[&str] = &[
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "max_output_tokens",
        "stream",
        "temperature",
        "top_p",
        // Codex Responses controls with no Chat representation: tolerated and
        // dropped (the Responses→Messages composition wrapper records them in
        // the ConversionReport).
        "parallel_tool_calls",
        "store",
        "include",
        "prompt_cache_key",
        "client_metadata",
        // Mapped below: `reasoning.effort` → top-level `reasoning_effort`.
        "reasoning",
    ];
    let object = body.as_object().ok_or_else(|| {
        crate::protocol::codec::UnsupportedFeatures::single(
            crate::protocol::codec::FeatureKind::UnsupportedField,
            "/",
            "Responses request must be a JSON object",
        )
    })?;
    for key in object.keys() {
        if !SUPPORTED_TOP_LEVEL.contains(&key.as_str()) {
            return Err(crate::protocol::codec::UnsupportedFeatures::single(
                crate::protocol::codec::FeatureKind::UnsupportedField,
                format!("/{key}"),
                format!("Responses field {key:?} is not supported by Responses→Chat conversion"),
            ));
        }
    }
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // Convert input array to messages array
    let messages = if let Some(input) = body.get("input") {
        convert_responses_input_to_messages(input)
    } else {
        Value::Array(vec![])
    };

    // max_output_tokens -> max_tokens
    let max_tokens = body
        .get("max_output_tokens")
        .and_then(|m| m.as_u64())
        .unwrap_or(4096);

    let stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let mut openai_body = serde_json::json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    // Pass through temperature if present
    if let Some(temp) = body.get("temperature") {
        openai_body["temperature"] = temp.clone();
    }
    // Pass through top_p if present
    if let Some(top_p) = body.get("top_p") {
        openai_body["top_p"] = top_p.clone();
    }
    // Convert Responses API tools to Chat Completions tools format.
    // Responses API uses flat format: { type: "function", name, parameters, description }
    // Chat Completions uses nested format: { type: "function", function: { name, parameters, description } }
    if let Some(tools) = body.get("tools") {
        if let Some(arr) = tools.as_array() {
            let openai_tools: Vec<Value> = arr
                .iter()
                .filter_map(|t| {
                    let tool_type = t.get("type").and_then(|ty| ty.as_str()).unwrap_or("");
                    match tool_type {
                        // Function tools: convert flat → nested
                        "function" => {
                            // Already in Chat Completions format (has "function" field) — pass through
                            if t.get("function").is_some() {
                                return Some(t.clone());
                            }
                            // Responses API flat format → convert to Chat Completions nested format.
                            // Chat Completions requires an object JSON schema.
                            let parameters = t.get("parameters").cloned().unwrap_or(Value::Null);
                            let parameters = if parameters.is_null() || !parameters.is_object() {
                                serde_json::json!({"type": "object", "properties": {}})
                            } else {
                                let mut params = parameters;
                                if params.get("type").is_none() {
                                    if let Some(obj) = params.as_object_mut() {
                                        obj.insert(
                                            "type".to_string(),
                                            Value::String("object".to_string()),
                                        );
                                    }
                                }
                                params
                            };
                            let func = serde_json::json!({
                                "name": t.get("name").cloned().unwrap_or(Value::Null),
                                "parameters": parameters,
                            });
                            let mut func_obj = func;
                            if let Some(desc) = t.get("description") {
                                func_obj["description"] = desc.clone();
                            }
                            if let Some(strict) = t.get("strict") {
                                func_obj["strict"] = strict.clone();
                            }
                            Some(serde_json::json!({
                                "type": "function",
                                "function": func_obj
                            }))
                        }
                        // Built-in tools (web_search, file_search, computer_use, etc.) — skip
                        _ => None,
                    }
                })
                .collect();
            if !openai_tools.is_empty() {
                openai_body["tools"] = Value::Array(openai_tools);
            }
        }
    }

    // Normalize tool_choice to Chat Completions shape, but ONLY when the
    // converted request actually carries function tools. `openai_body["tools"]`
    // is only set when the conversion produced a non-empty array, so its
    // presence is the exact gate. OpenAI Chat Completions rejects `tool_choice`
    // without `tools` ("When using `tool_choice`, `tools` must be set."), and
    // Codex sends `tool_choice: "auto"` even for plain no-tool requests —
    // passing it through unconditionally turns those into an upstream 400/502.
    if openai_body.get("tools").is_some() {
        if let Some(tc) = body.get("tool_choice") {
            if let Some(normalized) = responses_tool_choice_to_chat(tc) {
                openai_body["tool_choice"] = normalized;
            }
        }
    }

    // Map Responses `reasoning.effort` to Chat `reasoning_effort` so the
    // Chat→Messages leg (encode_chat_to_messages) can express it as Anthropic
    // thinking. A missing or malformed `reasoning` is tolerated fail-open.
    if let Some(effort) = body
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        openai_body["reasoning_effort"] = Value::String(effort.to_string());
    }

    // Pass through instructions as a system message if present
    if let Some(instructions) = body.get("instructions").and_then(|i| i.as_str()) {
        if !instructions.is_empty() {
            if let Some(msgs) = openai_body
                .get_mut("messages")
                .and_then(|m| m.as_array_mut())
            {
                msgs.insert(
                    0,
                    serde_json::json!({
                        "role": "system",
                        "content": instructions
                    }),
                );
            }
        }
    }

    Ok(openai_body)
}

/// Convert Responses API `input` array to OpenAI `messages` array.
/// Handles: message, function_call (assistant tool call), function_call_output (tool result)
fn convert_responses_input_to_messages(input: &Value) -> Value {
    let messages = if let Some(arr) = input.as_array() {
        // First pass: collect all function_call call_ids and their matching outputs
        let mut call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut output_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Map from original (possibly empty) call_id → fallback call_id
        let mut call_id_fallback: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut fallback_counter = 0u32;
        // Whether a `function_call` item appears anywhere AFTER index `i`.
        // A `reasoning` item followed (possibly through an intermediate
        // assistant text message) by function_calls belongs to that
        // tool-calling turn: its text must ride on the assistant(tool_calls)
        // message emitted at flush time, NOT on the intermediate text message.
        // DeepSeek thinking mode rejects the follow-up otherwise with
        // "The reasoning_content in the thinking mode must be passed back."
        let function_call_after: Vec<bool> = {
            let mut v = vec![false; arr.len()];
            let mut seen = false;
            for (i, item) in arr.iter().enumerate().rev() {
                v[i] = seen;
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    seen = true;
                }
            }
            v
        };

        for item in arr {
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match item_type {
                "function_call" => {
                    let cid = item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    if cid.is_empty() {
                        let fallback = format!("call_{}", fallback_counter);
                        fallback_counter += 1;
                        call_id_fallback.insert(cid.clone(), fallback.clone());
                        call_ids.insert(fallback);
                    } else {
                        call_ids.insert(cid);
                    }
                }
                "function_call_output" => {
                    let cid = item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Use fallback if one was generated for the corresponding function_call
                    let effective_cid = call_id_fallback.get(&cid).cloned().unwrap_or(cid);
                    output_ids.insert(effective_cid);
                }
                _ => {}
            }
        }

        let mut msgs = Vec::new();
        // Reasoning from a preceding `reasoning` item is attached to the next
        // assistant message as `reasoning_content`. Without this, thinking-mode
        // providers (e.g. DeepSeek) reject multi-turn requests with
        // "The `reasoning_content` in the thinking mode must be passed back."
        let mut pending_reasoning: Option<String> = None;
        // Function_call items are buffered and flushed together as ONE assistant
        // message with a multi-element `tool_calls` array. Emitting a separate
        // assistant message per call breaks parallel tool use: DeepSeek rejects any
        // assistant message carrying tool_calls that isn't immediately followed by
        // tool messages for each of its call_ids.
        let mut pending_tool_calls: Vec<(String, String, String)> = Vec::new();
        // call_ids whose tool response is still awaited (their real output exists
        // later in the input). Regular messages are deferred until this empties so
        // they never sit between an assistant(tool_calls) and its tool messages.
        let mut awaiting: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Regular messages deferred while tool responses are pending.
        let mut deferred: Vec<Value> = Vec::new();

        // Flush buffered function_calls as one assistant message. For each call:
        // if a real output exists later in the input, mark it awaiting; otherwise
        // synthesize an empty tool response so upstream never sees an unanswered
        // tool_call_id.
        let flush_tool_calls =
            |msgs: &mut Vec<Value>,
             pending_tool_calls: &mut Vec<(String, String, String)>,
             awaiting: &mut std::collections::HashSet<String>,
             output_ids: &std::collections::HashSet<String>,
             pending_reasoning: &mut Option<String>| {
                if pending_tool_calls.is_empty() {
                    return;
                }
                // Never flush a new tool batch while an earlier assistant(tool_calls)
                // is still awaiting its tool replies. Doing so would emit a SECOND
                // assistant message between the first one and its tool messages,
                // which DeepSeek rejects ("assistant with tool_calls must be
                // followed by tool messages responding to each tool_call_id"). The
                // new calls stay buffered and flush together once awaiting drains.
                if !awaiting.is_empty() {
                    return;
                }
                let tool_calls: Vec<Value> = pending_tool_calls
                    .iter()
                    .map(|(cid, name, arguments)| {
                        serde_json::json!({
                            "id": cid,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        })
                    })
                    .collect();
                let mut msg = serde_json::json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": tool_calls,
                });
                if let Some(rc) = pending_reasoning.take() {
                    msg["reasoning_content"] = Value::String(rc);
                }
                msgs.push(msg);
                for (cid, _, _) in pending_tool_calls.iter() {
                    if output_ids.contains(cid) {
                        awaiting.insert(cid.clone());
                    } else {
                        msgs.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": cid,
                            "content": ""
                        }));
                    }
                }
                pending_tool_calls.clear();
            };

        for (idx, item) in arr.iter().enumerate() {
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match item_type {
                // reasoning: thinking chain → attach as reasoning_content on the following assistant message
                "reasoning" => {
                    let mut text = String::new();
                    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                        for block in summary {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                    if text.is_empty() {
                        if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                            text = t.to_string();
                        }
                    }
                    if !text.is_empty() {
                        pending_reasoning = Some(text);
                    }
                }

                // function_call: assistant's tool call → buffer for the next merged assistant message
                "function_call" => {
                    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let arguments = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                    let original_call_id = item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Use fallback call_id if the original was empty
                    let call_id = call_id_fallback
                        .get(&original_call_id)
                        .cloned()
                        .unwrap_or(original_call_id);
                    pending_tool_calls.push((call_id, name.to_string(), arguments.to_string()));
                }

                // function_call_output: tool result → OpenAI tool message, then
                // release any deferred messages once every awaited output has landed
                "function_call_output" => {
                    flush_tool_calls(
                        &mut msgs,
                        &mut pending_tool_calls,
                        &mut awaiting,
                        &output_ids,
                        &mut pending_reasoning,
                    );
                    let original_call_id = item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Use fallback call_id if one was generated for the corresponding function_call
                    let call_id = call_id_fallback
                        .get(&original_call_id)
                        .cloned()
                        .unwrap_or(original_call_id);
                    let output = item.get("output").and_then(|o| o.as_str()).unwrap_or("");
                    awaiting.remove(&call_id);
                    msgs.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output
                    }));
                    // Tool responses are in; regular messages can be emitted again.
                    if awaiting.is_empty() {
                        msgs.append(&mut deferred);
                    }
                }

                // message: standard chat message
                "message" | _ if item.get("role").is_some() => {
                    flush_tool_calls(
                        &mut msgs,
                        &mut pending_tool_calls,
                        &mut awaiting,
                        &output_ids,
                        &mut pending_reasoning,
                    );
                    let role = item
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("user")
                        .to_string();
                    // Map Roles that some providers don't recognize
                    // 'developer' is an OpenAI alias for 'system' (used by Codex/Responses API)
                    let role = match role.as_str() {
                        "developer" => "system".to_string(),
                        other => other.to_string(),
                    };
                    let content =
                        if let Some(content_arr) = item.get("content").and_then(|c| c.as_array()) {
                            // Extract text from content blocks
                            let texts: Vec<String> = content_arr
                                .iter()
                                .filter_map(|block| {
                                    // input_text, output_text, text
                                    block
                                        .get("text")
                                        .and_then(|t| t.as_str())
                                        .map(|s| s.to_string())
                                })
                                .collect();
                            Value::String(texts.join(""))
                        } else if let Some(text) = item.get("content").and_then(|c| c.as_str()) {
                            Value::String(text.to_string())
                        } else {
                            Value::String(String::new())
                        };
                    let mut msg = serde_json::json!({
                        "role": role,
                        "content": content,
                    });
                    // Attach reasoning_content (from a preceding `reasoning` item,
                    // or carried directly on this message) for assistant turns so
                    // thinking-mode providers accept the request.
                    if role == "assistant" {
                        let rc = item
                            .get("reasoning_content")
                            .and_then(|r| r.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                // If this assistant text message is part of a
                                // tool-calling turn (function_calls follow it),
                                // keep the reasoning for the assistant(tool_calls)
                                // message emitted at flush — that is the message
                                // DeepSeek's thinking mode requires it on.
                                if function_call_after[idx] {
                                    None
                                } else {
                                    pending_reasoning.take()
                                }
                            });
                        if let Some(rc) = rc {
                            if !rc.is_empty() {
                                msg["reasoning_content"] = Value::String(rc);
                            }
                        }
                    } else {
                        // A reasoning item belongs to the assistant message that
                        // immediately follows it; drop it before any other role —
                        // unless a buffered tool call still needs to flush with it.
                        if pending_tool_calls.is_empty() && awaiting.is_empty() {
                            pending_reasoning = None;
                        }
                    }
                    // Defer regular messages while tool responses are pending so
                    // they never interrupt an assistant(tool_calls)→tool(...) run.
                    if awaiting.is_empty() {
                        msgs.push(msg);
                    } else {
                        deferred.push(msg);
                    }
                }

                // Simple text item
                _ if item.get("text").is_some() => {
                    flush_tool_calls(
                        &mut msgs,
                        &mut pending_tool_calls,
                        &mut awaiting,
                        &output_ids,
                        &mut pending_reasoning,
                    );
                    let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let msg = serde_json::json!({
                        "role": "user",
                        "content": text,
                    });
                    if awaiting.is_empty() {
                        msgs.push(msg);
                    } else {
                        deferred.push(msg);
                    }
                }

                // Raw string input
                _ => {
                    if let Some(s) = item.as_str() {
                        flush_tool_calls(
                            &mut msgs,
                            &mut pending_tool_calls,
                            &mut awaiting,
                            &output_ids,
                            &mut pending_reasoning,
                        );
                        let msg = serde_json::json!({
                            "role": "user",
                            "content": s,
                        });
                        if awaiting.is_empty() {
                            msgs.push(msg);
                        } else {
                            deferred.push(msg);
                        }
                    }
                }
            }
        }
        // End of input: flush any buffered tool calls and remaining deferred messages.
        flush_tool_calls(
            &mut msgs,
            &mut pending_tool_calls,
            &mut awaiting,
            &output_ids,
            &mut pending_reasoning,
        );
        if awaiting.is_empty() {
            msgs.append(&mut deferred);
        }
        msgs
    } else if let Some(s) = input.as_str() {
        // Simple string input
        vec![serde_json::json!({"role": "user", "content": s})]
    } else {
        vec![]
    };

    Value::Array(messages)
}

/// Convert an OpenAI Chat Completions response to Anthropic Messages format.
///
/// This deliberately fails instead of inventing tool input when an upstream
/// returns malformed function arguments. Claude Code uses those arguments to
/// execute local tools, so replacing bad JSON with `{}` is unsafe.
pub fn openai_to_anthropic(openai_resp: &Value, model: &str) -> Result<Value, String> {
    let choice = openai_resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    let message = choice.and_then(|ch| ch.get("message"));

    let message = message
        .ok_or_else(|| "OpenAI response does not contain a completion message".to_string())?;
    // Fail-open (CPA semantics): upstream reasoning is surfaced as a Messages
    // `thinking` block, always kept (even when content is also present).  Only
    // the visible text is used; `{text: ...}` object form is unwrapped.
    let reasoning_text = message
        .get("reasoning_content")
        .and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(m) => m
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from),
            _ => None,
        })
        .or_else(|| match message.get("thinking") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Object(m)) => m
                .get("thinking")
                .or_else(|| m.get("text"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from),
            _ => None,
        });
    let content_text = match message.get("content") {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value,
        Some(_) => {
            return Err("OpenAI response has unsupported non-text message content".to_string())
        }
    };

    let finish_reason = choice
        .and_then(|ch| ch.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("");

    // Chat Completions normally sets `tool_calls`, but some compatible
    // upstreams omit it.  The tool-call payload is less ambiguous than a
    // missing finish reason, so do not report a completed tool turn as an
    // ordinary end_turn.
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ if message.get("refusal").is_some() => "refusal",
        _ if has_tool_calls => "tool_use",
        _ => "end_turn",
    };

    let input_tokens = openai_resp
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = openai_resp
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    // Build content array: thinking block (if any) + text blocks + tool_use
    let mut content_blocks = Vec::new();

    // Add thinking block first (reasoning precedes visible text)
    if let Some(rt) = reasoning_text.as_ref().filter(|s| !s.is_empty()) {
        content_blocks.push(serde_json::json!({
            "type": "thinking",
            "thinking": rt
        }));
    }

    // Add text block if present
    if !content_text.is_empty() {
        content_blocks.push(serde_json::json!({
            "type": "text",
            "text": content_text
        }));
    }

    // Add tool_use blocks for tool_calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "OpenAI response tool call is missing its id".to_string())?;
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name").and_then(|n| n.as_str()))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "OpenAI response tool call is missing its function name".to_string()
                })?;
            let arguments_str = func
                .and_then(|f| f.get("arguments").and_then(|a| a.as_str()))
                .ok_or_else(|| {
                    "OpenAI response tool call is missing function arguments".to_string()
                })?;
            let input: Value = serde_json::from_str(arguments_str).map_err(|error| {
                format!(
                    "OpenAI response contained invalid tool arguments: {}",
                    error
                )
            })?;
            if !input.is_object() {
                return Err(
                    "OpenAI response tool arguments must decode to a JSON object".to_string(),
                );
            }

            content_blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
        }
    }

    // If no content blocks at all, add empty text
    if content_blocks.is_empty() {
        content_blocks.push(serde_json::json!({
            "type": "text",
            "text": ""
        }));
    }

    Ok(serde_json::json!({
        "id": openai_resp.get("id").cloned().unwrap_or(Value::String(format!("msg_{}", uuid::Uuid::new_v4().simple()))),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    }))
}

/// Convert an Anthropic Messages request to OpenAI Chat Completions.
///
/// This converter intentionally accepts only the intersection which can be
/// represented by Chat Completions. Native Anthropic channels must bypass it.
pub fn anthropic_to_openai(body: &Value) -> Result<Value, String> {
    // Fail-open (CLIProxyAPI semantics): thinking/output_config are mapped to
    // `reasoning_effort` below; container/context_management are dropped.  The
    // upstream provider adjudicates capability; we never reject thinking.
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let messages = body
        .get("messages")
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    let max_tokens = body
        .get("max_tokens")
        .and_then(|m| m.as_u64())
        .unwrap_or(4096);
    let stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // Extract top-level system message and prepend it.
    let system = body
        .get("system")
        .map(anthropic_system_content_to_openai_text)
        .transpose()?;

    // Convert Anthropic message content (array format) to OpenAI string format
    let openai_messages = convert_anthropic_messages_to_openai(&messages, system)?;

    let mut openai_body = serde_json::json!({
        "model": model,
        "messages": openai_messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    if let Some(temp) = body.get("temperature") {
        openai_body["temperature"] = temp.clone();
    }
    if let Some(top_p) = body.get("top_p") {
        openai_body["top_p"] = top_p.clone();
    }
    // Pass through top_k (OpenAI also supports this via some providers)
    if let Some(top_k) = body.get("top_k") {
        openai_body["top_k"] = top_k.clone();
    }
    // Pass through stop_sequences → stop
    if let Some(stop_seq) = body.get("stop_sequences") {
        openai_body["stop"] = stop_seq.clone();
    }
    if stream {
        let mut options = body
            .get("stream_options")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if !options.is_object() {
            return Err("stream_options must be an object".to_string());
        }
        options["include_usage"] = Value::Bool(true);
        openai_body["stream_options"] = options;
    }
    // Convert Anthropic tools to OpenAI tools format
    // Anthropic: {"name": "xxx", "description": "xxx", "input_schema": {...}}
    // OpenAI: {"type": "function", "function": {"name": "xxx", "description": "xxx", "parameters": {...}}}
    // Also handles Anthropic built-in tools (web_search, computer_use, etc.) which are skipped.
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let mut openai_tools = Vec::new();
        for tool in tools {
            // `cache_control` on a custom tool is likewise an Anthropic
            // caching annotation and has no Chat Completions equivalent.
            // Get the tool type — Anthropic custom tools use "custom" or have no type field
            let tool_type = tool
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("custom");
            match tool_type {
                // Standard function tools (type "custom" or no type)
                "custom" | "" => {
                    let name = tool
                        .get("name")
                        .and_then(|n| n.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| "Anthropic tool is missing its name".to_string())?;
                    let description = tool
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    let parameters = tool.get("input_schema").cloned().ok_or_else(|| {
                        format!("Anthropic tool '{}' is missing input_schema", name)
                    })?;
                    openai_tools.push(serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description,
                            "parameters": parameters
                        }
                    }));
                }
                _ => {
                    return Err(
                        "Anthropic built-in tools require a native Anthropic Messages channel"
                            .to_string(),
                    )
                }
            }
        }
        if !openai_tools.is_empty() {
            openai_body["tools"] = Value::Array(openai_tools);
        }
    }

    // Convert tool_choice
    // Anthropic: {"type": "auto"} or {"type": "any"} or {"type": "tool", "name": "xxx"}
    // OpenAI: "auto" or "required" or {"type": "function", "function": {"name": "xxx"}}
    if let Some(tc) = body.get("tool_choice") {
        if let Some(tc_type) = tc.get("type").and_then(|t| t.as_str()) {
            let openai_tc = match tc_type {
                "auto" => Value::String("auto".to_string()),
                "any" => Value::String("required".to_string()),
                "tool" => {
                    let name = tc.get("name").and_then(|n| n.as_str()).filter(|s| !s.is_empty())
                        .ok_or_else(|| "Anthropic tool_choice type 'tool' is missing a name".to_string())?;
                    serde_json::json!({
                        "type": "function",
                        "function": {"name": name}
                    })
                }
                _ => return Err("unsupported Anthropic tool_choice requires a native Anthropic Messages channel".to_string()),
            };
            openai_body["tool_choice"] = openai_tc;
        } else if let Some(s) = tc.as_str() {
            let openai_tc = match s {
                "auto" => Value::String("auto".to_string()),
                "any" => Value::String("required".to_string()),
                "tool" => return Err("Anthropic tool_choice 'tool' requires a name".to_string()),
                _ => {
                    return Err(
                        "unsupported Anthropic tool_choice requires a native Anthropic Messages channel"
                            .to_string(),
                    )
                }
            };
            openai_body["tool_choice"] = openai_tc;
        } else {
            return Err(
                "unsupported Anthropic tool_choice requires a native Anthropic Messages channel"
                    .to_string(),
            );
        }
    }

    if body
        .get("tool_choice")
        .and_then(|choice| choice.get("disable_parallel_tool_use"))
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        openai_body["parallel_tool_calls"] = Value::Bool(false);
    }

    // Fail-open thinking mapping: Anthropic `thinking` / `output_config` →
    // OpenAI `reasoning_effort` (CPA semantics).  Only set when the downstream
    // asked for thinking; otherwise leave unset so the upstream applies its own
    // default.  `container`/`context_management`/`context_management_config`
    // have no Chat equivalent and were dropped above (fail-open).
    if let Some(effort) = anthropic_thinking_to_reasoning_effort(body) {
        openai_body["reasoning_effort"] = Value::String(effort);
    }

    Ok(openai_body)
}

/// Map an Anthropic `thinking` config to an OpenAI `reasoning_effort` value.
///
/// `None` when the downstream did not ask for thinking (or asked for an
/// unrecognized type), in which case `reasoning_effort` is left unset.
fn anthropic_thinking_to_reasoning_effort(body: &Value) -> Option<String> {
    let thinking = body.get("thinking")?;
    if !thinking.is_object() {
        return None;
    }
    let ty = thinking.get("type").and_then(Value::as_str)?;
    match ty {
        "enabled" => match thinking.get("budget_tokens").and_then(Value::as_i64) {
            Some(budget) => crate::protocol::thinking::budget_to_level(budget).map(String::from),
            None => Some("auto".to_string()),
        },
        "adaptive" | "auto" => match body
            .get("output_config")
            .and_then(|oc| oc.get("effort"))
            .and_then(Value::as_str)
        {
            Some(effort) if !effort.trim().is_empty() => Some(effort.trim().to_ascii_lowercase()),
            _ => Some("xhigh".to_string()),
        },
        "disabled" => Some("none".to_string()),
        _ => None,
    }
}

/// Estimate structured Anthropic request size for the optional count_tokens endpoint.
#[allow(dead_code)]
pub fn estimate_anthropic_input_tokens(body: &Value) -> u64 {
    fn estimate(value: &Value) -> u64 {
        match value {
            Value::String(text) => ((text.chars().count() as u64) + 3) / 4,
            Value::Array(values) => values.iter().map(estimate).sum(),
            Value::Object(object) => object
                .iter()
                // Image source data is base64, not prompt text. Counting it would
                // overestimate by orders of magnitude on OpenAI-only channels.
                .filter(|(key, _)| !matches!(key.as_str(), "model" | "stream" | "data"))
                .map(|(_, value)| estimate(value))
                .sum(),
            _ => 0,
        }
    }
    estimate(body).max(1)
}

fn tool_result_to_openai_content(block: &Value) -> Result<String, String> {
    match block.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(items)) => {
            let mut text = String::new();
            for item in items {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("text") => text.push_str(item.get("text").and_then(|v| v.as_str()).unwrap_or("")),
                    Some("image") => return Err("tool_result images require a native Anthropic Messages channel".to_string()),
                    _ => return Err("unsupported tool_result content requires a native Anthropic Messages channel".to_string()),
                }
            }
            Ok(text)
        }
        _ => Err("tool_result content must be text or text blocks".to_string()),
    }
}

fn anthropic_system_content_to_openai_text(value: &Value) -> Result<String, String> {
    if let Some(str_val) = value.as_str() {
        Ok(str_val.to_string())
    } else if let Some(arr) = value.as_array() {
        let mut texts = Vec::new();
        for block in arr {
            // Prompt caching changes Anthropic billing/cache behavior but not
            // the text content of a Chat Completions request.  It is safe to
            // drop this annotation on the OpenAI bridge; native channels still
            // receive the original body unchanged.
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => texts.push(
                    block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                Some("thinking") => {
                    // Fail-open: reasoning instructions on the system prompt
                    // are dropped (no Chat equivalent), not rejected.
                }
                Some("cache_control") => {
                    return Err(
                        "system cache_control blocks require a native Anthropic Messages channel"
                            .to_string(),
                    )
                }
                _ => {
                    return Err(
                        "unsupported non-text system content requires a native Anthropic Messages channel"
                            .to_string(),
                    )
                }
            }
        }
        Ok(texts.join(""))
    } else {
        Err("system must be text or an array of text blocks".to_string())
    }
}

/// Convert Anthropic messages array to OpenAI messages array.
/// Anthropic content can be string or array of content blocks.
/// Handles: text, tool_use (assistant), tool_result (user)
fn convert_anthropic_messages_to_openai(
    messages: &Value,
    system: Option<String>,
) -> Result<Value, String> {
    let mut msgs = Vec::new();

    // Prepend system message if present
    if let Some(sys) = system {
        msgs.push(serde_json::json!({"role": "system", "content": sys}));
    }

    if let Some(arr) = messages.as_array() {
        for msg in arr {
            let role = msg
                .get("role")
                .and_then(|r| r.as_str())
                .ok_or_else(|| "Anthropic message is missing role".to_string())?
                .to_string();
            if role != "user" && role != "assistant" && role != "system" {
                return Err("only user, assistant, and system Anthropic messages can be sent to OpenAI Chat Completions".to_string());
            }

            if role == "system" {
                let content = msg
                    .get("content")
                    .ok_or_else(|| "system message is missing content".to_string())?;
                msgs.push(serde_json::json!({
                    "role": "system",
                    "content": anthropic_system_content_to_openai_text(content)?,
                }));
                continue;
            }

            if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
                let mut parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut assistant_reasoning = String::new();
                let flush_user_parts = |parts: &mut Vec<Value>, msgs: &mut Vec<Value>| {
                    if !parts.is_empty() {
                        msgs.push(
                            serde_json::json!({"role": "user", "content": std::mem::take(parts)}),
                        );
                    }
                };
                for block in content_arr {
                    // Cache controls are annotations on otherwise supported
                    // blocks.  Strip them instead of rejecting an entire
                    // OpenAI-only Claude Code request.
                    match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "text" => parts.push(serde_json::json!({"type": "text", "text": block.get("text").and_then(|t| t.as_str()).unwrap_or("")})),
                        "image" => {
                            if role != "user" { return Err("OpenAI Chat Completions cannot safely encode assistant image blocks".to_string()); }
                            let source = block.get("source").ok_or_else(|| "Anthropic image block is missing source".to_string())?;
                            let url = match source.get("type").and_then(|v| v.as_str()) {
                                Some("url") => source.get("url").and_then(|v| v.as_str()).ok_or_else(|| "Anthropic image URL source is missing url".to_string())?.to_string(),
                                Some("base64") => format!("data:{};base64,{}", source.get("media_type").and_then(|v| v.as_str()).ok_or_else(|| "Anthropic base64 image is missing media_type".to_string())?, source.get("data").and_then(|v| v.as_str()).ok_or_else(|| "Anthropic base64 image is missing data".to_string())?),
                                _ => return Err("unsupported Anthropic image source requires a native channel".to_string()),
                            };
                            parts.push(serde_json::json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                        "tool_use" => {
                            if role != "assistant" { return Err("tool_use blocks must be in an assistant message".to_string()); }
                            let id = block.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty()).ok_or_else(|| "tool_use is missing id".to_string())?;
                            let name = block.get("name").and_then(|n| n.as_str()).filter(|s| !s.is_empty()).ok_or_else(|| "tool_use is missing name".to_string())?;
                            let input = block.get("input").ok_or_else(|| "tool_use is missing input".to_string())?;
                            if !input.is_object() {
                                return Err("tool_use input must be a JSON object".to_string());
                            }
                            let input = input.clone();
                            tool_calls.push(serde_json::json!({"id": id, "type": "function", "function": {"name": name, "arguments": serde_json::to_string(&input).map_err(|e| e.to_string())?}}));
                        }
                        "tool_result" => {
                            if role != "user" { return Err("tool_result blocks must be in a user message".to_string()); }
                            flush_user_parts(&mut parts, &mut msgs);
                            let tool_use_id = block.get("tool_use_id").and_then(|t| t.as_str()).filter(|s| !s.is_empty()).ok_or_else(|| "tool_result is missing tool_use_id".to_string())?;
                            let result_content = tool_result_to_openai_content(block)?;
                            let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                            msgs.push(serde_json::json!({"role": "tool", "tool_call_id": tool_use_id, "content": if is_error { format!("Tool execution error:\n{}", result_content) } else { result_content }}));
                        }
                        "thinking" => {
                            // Fail-open: assistant reasoning is carried into
                            // the Chat message as `reasoning_content` (OpenAI
                            // non-stream field).  Reasoning on any other role is
                            // dropped — we never inject thinking into a
                            // user/system channel.
                            if role == "assistant" {
                                if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                                    assistant_reasoning.push_str(t);
                                }
                            }
                        }
                        "redacted_thinking" => {
                            // Encrypted/signature form — no usable text; drop.
                        }
                        "cache_control" => return Err("Anthropic cache controls require a native Anthropic Messages channel".to_string()),
                        _ => return Err("unsupported Anthropic content block requires a native Anthropic Messages channel".to_string()),
                    }
                }
                if role == "assistant" {
                    let content = if parts.is_empty() {
                        Value::Null
                    } else if parts
                        .iter()
                        .all(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
                    {
                        Value::String(
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                                .collect::<String>(),
                        )
                    } else {
                        Value::Array(parts)
                    };
                    // Reasoning content extracted from assistant `thinking`
                    // blocks (fail-open mapping to OpenAI `reasoning_content`).
                    let reasoning = if assistant_reasoning.is_empty() {
                        None
                    } else {
                        Some(assistant_reasoning)
                    };
                    if tool_calls.is_empty() && content.is_null() && reasoning.is_none() {
                        return Err("assistant message is empty".to_string());
                    }
                    let mut assistant =
                        serde_json::json!({"role": "assistant", "content": content});
                    if let Some(r) = reasoning {
                        assistant["reasoning_content"] = Value::String(r);
                    }
                    if !tool_calls.is_empty() {
                        assistant["tool_calls"] = Value::Array(tool_calls);
                    }
                    msgs.push(assistant);
                } else {
                    flush_user_parts(&mut parts, &mut msgs);
                }
            } else if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                msgs.push(serde_json::json!({
                    "role": role,
                    "content": s.to_string(),
                }));
            } else {
                msgs.push(serde_json::json!({
                    "role": role,
                    "content": msg.get("content").cloned().unwrap_or(Value::String(String::new())),
                }));
            }
        }
    } else {
        return Err("Anthropic messages must be an array".to_string());
    }

    Ok(Value::Array(msgs))
}

#[cfg(test)]
mod anthropic_tests {
    use super::*;

    #[test]
    fn counts_structured_anthropic_input() {
        let body = serde_json::json!({
            "model": "test-model",
            "system": [{"type": "text", "text": "system prompt"}],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
        });
        assert!(estimate_anthropic_input_tokens(&body) > 1);
    }

    #[test]
    fn maps_tools_parallel_control_and_mixed_tool_results() {
        let request = serde_json::json!({
            "model": "claude-compatible",
            "max_tokens": 32,
            "system": [{"type":"text", "text":"be concise"}],
            "tools": [{"name":"weather", "description":"weather", "input_schema":{"type":"object"}}],
            "tool_choice": {"type":"any", "disable_parallel_tool_use":true},
            "messages": [
                {"role":"assistant", "content":[{"type":"text","text":"checking"},{"type":"tool_use","id":"call_1","name":"weather","input":{"city":"Paris"}}]},
                {"role":"user", "content":[{"type":"tool_result","tool_use_id":"call_1","content":"sunny"},{"type":"text","text":"thanks"}]}
            ]
        });
        let converted = anthropic_to_openai(&request).unwrap();
        assert_eq!(converted["parallel_tool_calls"], false);
        assert_eq!(converted["tool_choice"], "required");
        assert_eq!(
            converted["tools"][0]["function"]["parameters"]["type"],
            "object"
        );
        assert_eq!(
            converted["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Paris\"}"
        );
        assert_eq!(converted["messages"][2]["role"], "tool");
        assert_eq!(converted["messages"][3]["content"][0]["text"], "thanks");
    }

    #[test]
    fn maps_mid_conversation_system_messages_to_chat_system_role() {
        let request = serde_json::json!({
            "model": "claude-compatible",
            "messages": [
                {"role":"user", "content":"use the strict profile"},
                {"role":"system", "content":[{"type":"text", "text":"strict profile active", "cache_control":{"type":"ephemeral"}}]},
                {"role":"assistant", "content":[{"type":"text", "text":"ack"}]}
            ]
        });
        let converted = anthropic_to_openai(&request).unwrap();
        assert_eq!(converted["messages"][0]["role"], "user");
        assert_eq!(converted["messages"][1]["role"], "system");
        assert_eq!(converted["messages"][1]["content"], "strict profile active");
        assert_eq!(converted["messages"][2]["role"], "assistant");
    }

    #[test]
    fn legacy_tool_choice_strings_map_or_reject() {
        for (input, expected) in [("auto", "auto"), ("any", "required")] {
            let request = serde_json::json!({
                "model": "model",
                "messages": [{"role":"user", "content":"hi"}],
                "tool_choice": input
            });
            let converted = anthropic_to_openai(&request).unwrap();
            assert_eq!(converted["tool_choice"], expected);
        }

        let request = serde_json::json!({
            "model": "model",
            "messages": [{"role":"user", "content":"hi"}],
            "tool_choice": "tool"
        });
        assert!(anthropic_to_openai(&request).is_err());

        let request = serde_json::json!({
            "model": "model",
            "messages": [{"role":"user", "content":"hi"}],
            "tool_choice": "bogus"
        });
        assert!(anthropic_to_openai(&request).is_err());
    }

    #[test]
    fn legacy_tool_use_requires_input_not_fabricated() {
        let request = serde_json::json!({
            "model": "model",
            "messages": [{"role":"assistant", "content":[{"type":"tool_use", "id":"call_1", "name":"run"}]}]
        });
        assert!(anthropic_to_openai(&request).is_err());

        let request = serde_json::json!({
            "model": "model",
            "messages": [{"role":"assistant", "content":[{"type":"tool_use", "id":"call_1", "name":"run", "input":[]}]}]
        });
        assert!(anthropic_to_openai(&request).is_err());

        let request = serde_json::json!({
            "model": "model",
            "messages": [{"role":"assistant", "content":[{"type":"tool_use", "id":"call_1", "name":"run", "input":{}}]}]
        });
        let converted = anthropic_to_openai(&request).unwrap();
        assert_eq!(
            converted["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }

    #[test]
    fn rejects_invalid_openai_tool_arguments_without_inventing_input() {
        let response = serde_json::json!({"choices":[{"finish_reason":"tool_calls", "message":{"role":"assistant", "content":null, "tool_calls":[{"id":"call_1", "function":{"name":"run", "arguments":"{bad"}}]}}]});
        assert!(openai_to_anthropic(&response, "model").is_err());
    }

    #[test]
    fn rejects_non_object_openai_tool_arguments_and_strips_cache_controls() {
        let response = serde_json::json!({"choices":[{"message":{"role":"assistant", "tool_calls":[{"id":"call_1", "function":{"name":"run", "arguments":"[]"}}]}}]});
        assert!(openai_to_anthropic(&response, "model").is_err());

        let cache_in_system = serde_json::json!({"model":"model", "system":[{"type":"text", "text":"cached", "cache_control":{"type":"ephemeral"}}], "messages":[]});
        assert_eq!(
            anthropic_to_openai(&cache_in_system).unwrap()["messages"][0]["content"],
            "cached"
        );
        let cache_in_message = serde_json::json!({"model":"model", "messages":[{"role":"user", "content":[{"type":"text", "text":"cached", "cache_control":{"type":"ephemeral"}}]}]});
        assert_eq!(
            anthropic_to_openai(&cache_in_message).unwrap()["messages"][0]["content"][0]["text"],
            "cached"
        );
    }

    #[test]
    fn preserves_anthropic_response_shape_for_refusals_and_implicit_tools() {
        let refusal = serde_json::json!({"choices":[{"finish_reason":"content_filter", "message":{"role":"assistant", "content":null, "refusal":"no"}}]});
        let converted = openai_to_anthropic(&refusal, "model").unwrap();
        assert_eq!(converted["stop_reason"], "refusal");
        assert!(converted.get("stop_sequence").is_some());

        let implicit_tool = serde_json::json!({"choices":[{"finish_reason":null, "message":{"role":"assistant", "content":null, "tool_calls":[{"id":"call_1", "function":{"name":"run", "arguments":"{}"}}]}}]});
        assert_eq!(
            openai_to_anthropic(&implicit_tool, "model").unwrap()["stop_reason"],
            "tool_use"
        );
    }

    #[test]
    fn streaming_openai_requests_always_request_late_usage() {
        let request = serde_json::json!({"model":"model", "stream":true, "stream_options":{"include_usage":false, "custom":true}, "messages":[]});
        let converted = anthropic_to_openai(&request).unwrap();
        assert_eq!(converted["stream_options"]["include_usage"], true);
        assert_eq!(converted["stream_options"]["custom"], true);
    }

    #[test]
    fn anthropic_to_openai_maps_thinking_fail_open() {
        // thinking enabled + budget_tokens 1024 -> reasoning_effort "low".
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "u"}],
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        });
        let converted = anthropic_to_openai(&body).unwrap();
        assert_eq!(converted["reasoning_effort"], "low");

        // adaptive + output_config.effort passthrough (lowercased).
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "u"}],
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "HIGH"}
        });
        let converted = anthropic_to_openai(&body).unwrap();
        assert_eq!(converted["reasoning_effort"], "high");

        // container / context_management dropped fail-open (no error).
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "u"}],
            "container": {"type": "super_container"},
            "context_management": {"turns": 4}
        });
        let converted = anthropic_to_openai(&body).unwrap();
        assert!(converted.get("container").is_none());
        assert!(converted.get("context_management").is_none());

        // system thinking block dropped.
        let body = serde_json::json!({
            "model": "m",
            "system": [{"type": "thinking", "thinking": "instruct"}],
            "messages": []
        });
        let converted = anthropic_to_openai(&body).unwrap();
        assert_eq!(converted["messages"][0]["content"], "");

        // assistant thinking block -> reasoning_content; redacted dropped.
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "chain"},
                {"type": "redacted_thinking", "data": "sig"},
                {"type": "text", "text": "answer"}
            ]}]
        });
        let converted = anthropic_to_openai(&body).unwrap();
        assert_eq!(converted["messages"][0]["reasoning_content"], "chain");
        assert_eq!(converted["messages"][0]["content"], "answer");
    }

    #[test]
    fn openai_to_anthropic_maps_reasoning_fail_open() {
        // reasoning_content -> Messages thinking block, kept even with content.
        let response = serde_json::json!({"choices":[{"finish_reason":"stop", "message":{"role":"assistant", "reasoning_content":"chain", "content":"answer"}}]});
        let converted = openai_to_anthropic(&response, "model").unwrap();
        assert_eq!(converted["content"][0]["type"], "thinking");
        assert_eq!(converted["content"][0]["thinking"], "chain");
        assert_eq!(converted["content"][1]["type"], "text");
        assert_eq!(converted["content"][1]["text"], "answer");
    }

    #[test]
    fn responses_input_reasoning_item_becomes_assistant_reasoning_content() {
        // A `reasoning` item must be forwarded to the upstream Chat request as
        // `reasoning_content` on the assistant message it precedes. Without this,
        // DeepSeek thinking models reject the 2nd+ turn with
        // "The `reasoning_content` in the thinking mode must be passed back."
        let input = serde_json::json!([
            {
                "type": "reasoning",
                "id": "rs_abc",
                "summary": [{"type": "summary_text", "text": "Let me think."}],
                "content": [{"type": "reasoning_text", "text": "chain of thought"}]
            },
            {
                "type": "message",
                "id": "msg_xyz",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "The answer is 42."}]
            }
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "The answer is 42.");
        assert_eq!(
            msgs[0]["reasoning_content"],
            "Let me think.chain of thought"
        );
    }

    #[test]
    fn responses_input_reasoning_item_without_content_still_preserved() {
        // DeepSeek-compatible: even a reasoning item with only a summary must be
        // forwarded as reasoning_content (no crash, no drop).
        let input = serde_json::json!([
            {"type": "reasoning", "id": "rs_abc", "summary": [{"type": "summary_text", "text": "Let me think."}]},
            {"type": "message", "id": "msg_xyz", "role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs[0]["reasoning_content"], "Let me think.");
    }

    #[test]
    fn responses_input_message_carries_reasoning_content_directly() {
        // Some clients attach reasoning_content directly to the message item.
        let input = serde_json::json!([
            {"type": "message", "id": "msg_xyz", "role": "assistant",
             "reasoning_content": "direct chain",
             "content": [{"type": "output_text", "text": "Hi"}]}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs[0]["reasoning_content"], "direct chain");
    }

    #[test]
    fn responses_reasoning_round_trip_to_chat() {
        // Simulates the full Codex repro: upstream DeepSeek streams reasoning_content,
        // WaLiAPI emits a `reasoning` item in Responses API format, then the next
        // turn's input (echoing that reasoning item) converts back to Chat with
        // `reasoning_content` so DeepSeek accepts the request.
        use crate::protocol::responses::{
            convert_openai_sse_to_responses, create_synthetic_completed_events, StreamState,
        };

        let response_id = "resp_repro";
        let mut state = StreamState::default();

        // Upstream turn output: reasoning_content then content, then stop.
        let ev1 = convert_openai_sse_to_responses(
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"deepseek thought"},"finish_reason":null}]}"#,
            "deepseek-v4-flash",
            response_id,
            "",
            &mut state,
        );
        let ev2 = convert_openai_sse_to_responses(
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
            "deepseek-v4-flash",
            response_id,
            "answer",
            &mut state,
        );
        let ev3 = convert_openai_sse_to_responses(
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "deepseek-v4-flash",
            response_id,
            "answer",
            &mut state,
        );
        let ev4 = create_synthetic_completed_events(
            "deepseek-v4-flash",
            response_id,
            "answer",
            &state,
            10,
            5,
        );

        // The stream Codex receives must announce + complete the reasoning item.
        let stream: String = ev1.into_iter().chain(ev2).chain(ev3).chain(ev4).collect();
        assert!(stream.contains("\"type\":\"response.output_item.added\""));
        assert!(stream.contains("\"type\":\"reasoning\""));
        assert!(stream.contains("\"type\":\"reasoning_summary_text\""));
        assert!(stream.contains("deepseek thought"));
        assert!(stream.contains("\"type\":\"response.completed\""));

        // Next turn: Codex echoes reasoning + message items back in input.
        let next_input = serde_json::json!([
            {"type": "reasoning", "id": "rs_repro", "summary": [{"type": "summary_text", "text": "deepseek thought"}]},
            {"type": "message", "id": "msg_repro", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "message", "id": "msg_u2", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
        ]);
        let messages = convert_responses_input_to_messages(&next_input);
        let msgs = messages.as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["reasoning_content"], "deepseek thought");
        assert_eq!(assistant["content"], "answer");
    }

    #[test]
    fn responses_to_openai_drops_tool_choice_when_no_function_tools() {
        // GitHub issue #13: Codex sends `tool_choice: "auto"` even on plain
        // no-tool requests. Chat Completions rejects `tool_choice` without
        // `tools` ("When using `tool_choice`, `tools` must be set."), so the
        // conversion must strip it whenever no convertible function tools exist.
        let body = serde_json::json!({
            "model": "gpt-4",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tool_choice": "auto",
            // Only non-function tools (e.g. web_search) — must NOT keep tool_choice.
            "tools": [{"type": "web_search"}]
        });
        let converted = responses_to_openai(&body).unwrap();
        assert!(
            converted.get("tool_choice").is_none(),
            "tool_choice must be dropped when no function tools convert"
        );
        assert!(
            converted.get("tools").is_none(),
            "non-function tools must not be forwarded"
        );
    }

    #[test]
    fn responses_to_openai_keeps_tool_choice_only_with_function_tools() {
        // When the request does carry convertible function tools, tool_choice
        // passes through; the assistant tool-call message must use "" instead of
        // null content (some strict OpenAI-compatible services reject content:null).
        let body = serde_json::json!({
            "model": "gpt-4",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "function_call", "call_id": "call_A", "name": "list", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_A", "output": "a.txt"}
            ],
            "tools": [{"type": "function", "name": "list", "parameters": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "function", "name": "list"}
        });
        let converted = responses_to_openai(&body).unwrap();
        assert!(
            converted.get("tool_choice").is_some(),
            "tool_choice must be kept when function tools are present"
        );
        assert_eq!(converted["tool_choice"]["type"], "function");
        assert_eq!(converted["tool_choice"]["function"]["name"], "list");
        assert_eq!(converted["tools"].as_array().unwrap().len(), 1);

        let msgs = converted["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["tool_calls"][0]["id"], "call_A");
        assert_eq!(
            assistant["content"], "",
            "assistant tool-call message must use empty string, not null"
        );
    }

    #[test]
    fn responses_to_openai_flattens_object_tool_choice_modes() {
        // Responses object forms {"type": "auto"} / {"type": "none"} must flatten
        // to the bare strings Chat Completions expects.
        for (input, expected) in [("auto", "auto"), ("none", "none"), ("required", "required")] {
            let body = serde_json::json!({
                "model": "gpt-4",
                "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
                "tool_choice": {"type": input},
                "tools": [{"type": "function", "name": "list", "parameters": {"type": "object", "properties": {}}}]
            });
            let converted = responses_to_openai(&body).unwrap();
            assert_eq!(
                converted["tool_choice"], expected,
                "object {{type:{input}}} must flatten to string"
            );
        }
    }

    #[test]
    fn responses_to_drops_object_tool_choice_without_name() {
        // {"type":"function"} without a name has no Chat equivalent — drop it.
        let body = serde_json::json!({
            "model": "gpt-4",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tool_choice": {"type": "function"},
            "tools": [{"type": "function", "name": "list", "parameters": {"type": "object", "properties": {}}}]
        });
        assert!(
            responses_to_openai(&body).is_ok(),
            "legacy malformed tool choice remains safely omitted"
        );
    }

    #[test]
    fn responses_to_openai_tolerates_codex_controls_and_maps_reasoning_effort() {
        // codex 0.147.0 always sends these top-level controls alongside the
        // standard Responses fields. They have no Chat representation and must
        // be tolerated (dropped) rather than rejected, and `reasoning.effort`
        // must map to top-level `reasoning_effort` for the Chat→Messages leg.
        let body = serde_json::json!({
            "model": "gpt-4",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "parallel_tool_calls": false,
            "store": true,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "key",
            "client_metadata": {"turn": "1"},
            "reasoning": {"effort": "high"}
        });
        let converted = responses_to_openai(&body).unwrap();
        // The controls are dropped from the Chat body...
        for key in [
            "parallel_tool_calls",
            "store",
            "include",
            "prompt_cache_key",
            "client_metadata",
        ] {
            assert!(
                converted.get(key).is_none(),
                "{key} must not leak into the Chat body"
            );
        }
        // ...and reasoning.effort is mapped to reasoning_effort.
        assert_eq!(converted["reasoning_effort"], "high");
        // Legacy max_tokens default is unchanged (4096) on this path.
        assert_eq!(converted["max_tokens"], 4096);
    }

    #[test]
    fn responses_input_reasoning_stays_on_tool_calls_message() {
        // Real Codex echo order (captured from the local gateway log):
        // reasoning → assistant text → function_call → function_call_output.
        // DeepSeek thinking mode requires `reasoning_content` on the
        // assistant(tool_calls) message; consuming it on the intermediate
        // text message makes the follow-up fail with
        // "The reasoning_content in the thinking mode must be passed back
        // to the API."
        let input = serde_json::json!([
            {"type": "reasoning", "id": "rs_a", "summary": [{"type": "summary_text", "text": "Let me check the file."}]},
            {"type": "message", "id": "msg_a", "role": "assistant", "content": [{"type": "output_text", "text": "Let me look at that."}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_A", "name": "read", "arguments": "{\"path\":\"/tmp/x\"}"},
            {"type": "function_call_output", "call_id": "call_A", "output": "file contents"},
            {"type": "message", "id": "msg_u", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();

        // The tool-calling assistant message MUST carry the reasoning.
        let tool_call_msg = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
            .unwrap();
        assert_eq!(tool_call_msg["reasoning_content"], "Let me check the file.");
        assert_eq!(tool_call_msg["tool_calls"][0]["id"], "call_A");
        assert_eq!(msgs[0]["role"], "assistant");

        // The intermediate assistant text message must NOT have consumed it.
        let text_msg = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
            .unwrap();
        assert_eq!(text_msg["content"], "Let me look at that.");
        assert!(
            text_msg.get("reasoning_content").is_none()
                || text_msg["reasoning_content"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty(),
            "reasoning must not be consumed by the intermediate text message"
        );

        // Tool output still follows the tool_calls assistant message.
        assert_eq!(
            msgs[2],
            serde_json::json!({"role": "tool", "tool_call_id": "call_A", "content": "file contents"})
        );
    }

    #[test]
    fn responses_input_reasoning_with_user_interleaved_before_output() {
        // Codex can interleave a user text message between function_call and
        // function_call_output. The reasoning must survive until the
        // assistant(tool_calls) flush even across that user message.
        let input = serde_json::json!([
            {"type": "reasoning", "id": "rs_a", "summary": [{"type": "summary_text", "text": "thinking"}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_A", "name": "shell", "arguments": "{}"},
            {"type": "message", "id": "msg_ok", "role": "user", "content": [{"type": "input_text", "text": "Approved command prefix saved"}]},
            {"type": "function_call_output", "call_id": "call_A", "output": "done"},
            {"type": "message", "id": "msg_next", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        let tool_call_msg = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
            .unwrap();
        assert_eq!(tool_call_msg["reasoning_content"], "thinking");
        assert_eq!(msgs[1]["role"], "tool");
    }

    #[test]
    fn responses_input_parallel_function_calls_merge_into_one_assistant() {
        // Parallel function_calls must be merged into ONE assistant message with a
        // multi-element tool_calls array, immediately followed by their tool
        // messages. Splitting them into per-call assistant messages makes DeepSeek
        // reject the request ("assistant with tool_calls must be followed by tool
        // messages responding to each tool_call_id").
        let input = serde_json::json!([
            {"type": "function_call", "call_id": "call_A", "name": "read", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_B", "name": "grep", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_A", "output": "file contents"},
            {"type": "function_call_output", "call_id": "call_B", "output": "matched lines"}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_A");
        assert_eq!(msgs[0]["tool_calls"][1]["id"], "call_B");
        assert_eq!(
            msgs[1],
            serde_json::json!({"role": "tool", "tool_call_id": "call_A", "content": "file contents"})
        );
        assert_eq!(
            msgs[2],
            serde_json::json!({"role": "tool", "tool_call_id": "call_B", "content": "matched lines"})
        );
    }

    #[test]
    fn responses_input_defers_text_message_until_tool_output() {
        // Codex interleaves a user text message ("Approved command prefix saved")
        // between function_call and function_call_output. That text must be
        // deferred until the tool output lands, so the assistant(tool_calls) is
        // immediately followed by its tool message.
        let input = serde_json::json!([
            {"type": "function_call", "call_id": "call_A", "name": "shell", "arguments": "{}"},
            {"type": "message", "id": "msg_ok", "role": "user", "content": [{"type": "input_text", "text": "Approved command prefix saved"}]},
            {"type": "function_call_output", "call_id": "call_A", "output": "done"},
            {"type": "message", "id": "msg_next", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_A");
        assert_eq!(
            msgs[1],
            serde_json::json!({"role": "tool", "tool_call_id": "call_A", "content": "done"})
        );
        assert_eq!(
            msgs[2],
            serde_json::json!({"role": "user", "content": "Approved command prefix saved"})
        );
        assert_eq!(
            msgs[3],
            serde_json::json!({"role": "user", "content": "next"})
        );
    }

    #[test]
    fn responses_input_orphan_function_call_gets_empty_tool_response() {
        // A function_call with no matching output must still get a synthesized
        // empty tool message, otherwise the assistant(tool_calls) has no response.
        let input = serde_json::json!([
            {"type": "function_call", "call_id": "call_A", "name": "shell", "arguments": "{}"},
            {"type": "message", "id": "msg_next", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_A");
        assert_eq!(
            msgs[1],
            serde_json::json!({"role": "tool", "tool_call_id": "call_A", "content": ""})
        );
        assert_eq!(
            msgs[2],
            serde_json::json!({"role": "user", "content": "next"})
        );
    }

    #[test]
    fn responses_input_reasoning_attaches_to_merged_tool_calls_message() {
        // When reasoning directly precedes parallel function_calls, the reasoning
        // content is attached to the merged assistant tool_calls message.
        let input = serde_json::json!([
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "think"}]},
            {"type": "function_call", "call_id": "call_A", "name": "read", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_B", "name": "grep", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_A", "output": "a"},
            {"type": "function_call_output", "call_id": "call_B", "output": "b"}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["reasoning_content"], "think");
        assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[2]["role"], "tool");
    }

    #[test]
    fn responses_input_interleaved_call_never_leaves_orphan_assistant() {
        // A second function_call that arrives AFTER a user confirmation message
        // (still awaiting tool output for a prior call) must NOT be flushed as a
        // separate assistant message while the first one is still awaiting its
        // tool reply. Flushing it mid-await would emit
        // `assistant(tool_calls=[A]), assistant(tool_calls=[B]), tool_A, ...`
        // — the first assistant left without its tool message, which DeepSeek
        // rejects exactly like the original 502. Each assistant(tool_calls)
        // must be immediately followed by its own tool messages.
        let input = serde_json::json!([
            {"type": "function_call", "call_id": "call_A", "name": "shell", "arguments": "{}"},
            {"type": "message", "id": "msg_ok", "role": "user", "content": [{"type": "input_text", "text": "Approved command prefix saved"}]},
            {"type": "function_call", "call_id": "call_B", "name": "read", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_A", "output": "done"},
            {"type": "function_call_output", "call_id": "call_B", "output": "file contents"}
        ]);
        let messages = convert_responses_input_to_messages(&input);
        let msgs = messages.as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        // assistant(A) is immediately followed by tool_A — never by assistant(B).
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_A");
        assert_eq!(
            msgs[1],
            serde_json::json!({"role": "tool", "tool_call_id": "call_A", "content": "done"})
        );
        assert_eq!(
            msgs[2],
            serde_json::json!({"role": "user", "content": "Approved command prefix saved"})
        );
        // assistant(B) starts a fresh, valid turn: tool_B follows it directly.
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["tool_calls"][0]["id"], "call_B");
        assert_eq!(
            msgs[4],
            serde_json::json!({"role": "tool", "tool_call_id": "call_B", "content": "file contents"})
        );
    }
}
