use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Convert an OpenAI SSE chunk (Chat Completions stream) to Responses API SSE events.
///
/// OpenAI stream chunk format:
/// ```json
/// {"id":"...","choices":[{"delta":{"content":"hello"},"finish_reason":null}]}
/// ```
///
/// Responses API stream events (Codex-compatible event chain):
/// 1. response.created           — sent once at stream start (via create_response_created_events)
/// 2. response.in_progress       — sent once at stream start (via create_response_created_events)
/// 3. response.output_item.added — sent before first content delta
/// 4. response.content_part.added — sent before first content delta
/// 5. response.output_text.delta  — sent for each content chunk
/// 6. response.output_text.done   — sent at finish
/// 7. response.content_part.done  — sent at finish
/// 8. response.output_item.done   — sent at finish
/// 9. response.completed          — sent at finish
/// 10. data: [DONE]
///
/// State is tracked via `output_item_added_sent` and `output_item_done_sent`.
pub fn convert_openai_sse_to_responses(
    chunk_text: &str,
    model: &str,
    response_id: &str,
    accumulated_content: &str,
    output_item_added_sent: &mut bool,
    output_item_done_sent: &mut bool,
) -> Vec<String> {
    let mut events = Vec::new();
    let msg_id = if response_id.starts_with("resp_") { format!("msg_{}", &response_id[5..]) } else { format!("msg_{}", response_id) };

    for line in chunk_text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue;
        }
        let data_str = trimmed.trim_start_matches("data:").trim();
        if data_str == "[DONE]" || data_str.is_empty() {
            continue;
        }

        let json: Value = match serde_json::from_str(data_str) {
            Ok(j) => j,
            Err(_) => continue,
        };

        if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    // Content delta
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            // Emit output_item.added + content_part.added before first delta
                            if !*output_item_added_sent {
                                let item = serde_json::json!({
                                    "id": msg_id,
                                    "type": "message",
                                    "status": "in_progress",
                                    "role": "assistant"
                                });
                                let item_event = serde_json::json!({
                                    "type": "response.output_item.added",
                                    "response_id": response_id,
                                    "output_index": 0,
                                    "item": item
                                });
                                events.push(format!("event: response.output_item.added\ndata: {}\n\n", item_event));

                                let part = serde_json::json!({
                                    "type": "output_text",
                                    "text": "",
                                    "annotations": []
                                });
                                let part_event = serde_json::json!({
                                    "type": "response.content_part.added",
                                    "response_id": response_id,
                                    "item_id": msg_id,
                                    "output_index": 0,
                                    "content_index": 0,
                                    "part": part
                                });
                                events.push(format!("event: response.content_part.added\ndata: {}\n\n", part_event));

                                *output_item_added_sent = true;
                            }

                            let event = serde_json::json!({
                                "type": "response.output_text.delta",
                                "response_id": response_id,
                                "item_id": msg_id,
                                "output_index": 0,
                                "content_index": 0,
                                "delta": content
                            });
                            events.push(format!("event: response.output_text.delta\ndata: {}\n\n", event));
                        }
                    }

                    // Tool calls delta
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tool_calls {
                            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                            let tc_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                            let func = tc.get("function");
                            let name = func.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                            let arguments = func.and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or("");

                            if !tc_id.is_empty() && !name.is_empty() {
                                let event = serde_json::json!({
                                    "type": "response.function_call",
                                    "response_id": response_id,
                                    "call_id": tc_id,
                                    "name": name,
                                    "arguments": ""
                                });
                                events.push(format!("event: response.function_call\ndata: {}\n\n", event));
                            }

                            if !arguments.is_empty() {
                                let event = serde_json::json!({
                                    "type": "response.function_call_arguments.delta",
                                    "response_id": response_id,
                                    "item_id": format!("fc_{}", index),
                                    "delta": arguments
                                });
                                events.push(format!("event: response.function_call_arguments.delta\ndata: {}\n\n", event));
                            }
                        }
                    }
                }

                // Check for finish_reason
                if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    if !finish.is_empty() && finish != "null" {
                        // Emit closing events: output_text.done, content_part.done, output_item.done, response.completed
                        if *output_item_added_sent && !*output_item_done_sent {
                            // response.output_text.done
                            let text_done = serde_json::json!({
                                "type": "response.output_text.done",
                                "response_id": response_id,
                                "item_id": msg_id,
                                "output_index": 0,
                                "content_index": 0,
                                "text": accumulated_content
                            });
                            events.push(format!("event: response.output_text.done\ndata: {}\n\n", text_done));

                            // response.content_part.done
                            let part = serde_json::json!({
                                "type": "output_text",
                                "text": accumulated_content,
                                "annotations": []
                            });
                            let part_done = serde_json::json!({
                                "type": "response.content_part.done",
                                "response_id": response_id,
                                "item_id": msg_id,
                                "output_index": 0,
                                "content_index": 0,
                                "part": part
                            });
                            events.push(format!("event: response.content_part.done\ndata: {}\n\n", part_done));

                            // response.output_item.done
                            let completed_item = serde_json::json!({
                                "id": msg_id,
                                "type": "message",
                                "status": "completed",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": accumulated_content,
                                    "annotations": []
                                }]
                            });
                            let item_done = serde_json::json!({
                                "type": "response.output_item.done",
                                "response_id": response_id,
                                "output_index": 0,
                                "item": completed_item
                            });
                            events.push(format!("event: response.output_item.done\ndata: {}\n\n", item_done));

                            *output_item_done_sent = true;
                        }

                        let usage_prompt = json.get("usage").and_then(|u| u.get("prompt_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);
                        let usage_completion = json.get("usage").and_then(|u| u.get("completion_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);

                        let completed = serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "id": response_id,
                                "object": "response",
                                "created_at": now_ts(),
                                "status": "completed",
                                "model": model,
                                "output": [{
                                    "id": msg_id,
                                    "type": "message",
                                    "status": "completed",
                                    "role": "assistant",
                                    "content": [{
                                        "type": "output_text",
                                        "text": accumulated_content,
                                        "annotations": []
                                    }]
                                }],
                                "usage": {
                                    "input_tokens": usage_prompt,
                                    "output_tokens": usage_completion,
                                    "total_tokens": usage_prompt + usage_completion
                                }
                            }
                        });
                        events.push(format!("event: response.completed\ndata: {}\n\n", completed));
                    }
                }
            }
        }
    }

    if chunk_text.contains("[DONE]") {
        events.push("data: [DONE]\n\n".to_string());
    }

    events
}

/// Create the initial response.created + response.in_progress events for Responses API stream.
/// Returns both events as a single string to write at stream start.
pub fn create_response_created_event(model: &str, response_id: &str) -> String {
    let created = now_ts();
    let response_obj = serde_json::json!({
        "id": response_id,
        "object": "response",
        "created_at": created,
        "status": "in_progress",
        "model": model,
        "output": []
    });

    let created_event = serde_json::json!({
        "type": "response.created",
        "response": response_obj
    });

    let in_progress_event = serde_json::json!({
        "type": "response.in_progress",
        "response": response_obj
    });

    format!(
        "event: response.created\ndata: {}\n\nevent: response.in_progress\ndata: {}\n\n",
        created_event, in_progress_event
    )
}

/// Create synthetic closing events when upstream stream ends without finish_reason.
/// Emits: output_text.done, content_part.done, output_item.done, response.completed
pub fn create_synthetic_completed_events(
    model: &str,
    response_id: &str,
    accumulated_content: &str,
    output_item_added_sent: bool,
    output_item_done_sent: bool,
) -> Vec<String> {
    let mut events = Vec::new();
    let msg_id = if response_id.starts_with("resp_") { format!("msg_{}", &response_id[5..]) } else { format!("msg_{}", response_id) };

    if output_item_added_sent && !output_item_done_sent {
        // response.output_text.done
        let text_done = serde_json::json!({
            "type": "response.output_text.done",
            "response_id": response_id,
            "item_id": msg_id,
            "output_index": 0,
            "content_index": 0,
            "text": accumulated_content
        });
        events.push(format!("event: response.output_text.done\ndata: {}\n\n", text_done));

        // response.content_part.done
        let part = serde_json::json!({
            "type": "output_text",
            "text": accumulated_content,
            "annotations": []
        });
        let part_done = serde_json::json!({
            "type": "response.content_part.done",
            "response_id": response_id,
            "item_id": msg_id,
            "output_index": 0,
            "content_index": 0,
            "part": part
        });
        events.push(format!("event: response.content_part.done\ndata: {}\n\n", part_done));

        // response.output_item.done
        let completed_item = serde_json::json!({
            "id": msg_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": accumulated_content,
                "annotations": []
            }]
        });
        let item_done = serde_json::json!({
            "type": "response.output_item.done",
            "response_id": response_id,
            "output_index": 0,
            "item": completed_item
        });
        events.push(format!("event: response.output_item.done\ndata: {}\n\n", item_done));
    }

    // response.completed
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": now_ts(),
            "status": "completed",
            "model": model,
            "output": if output_item_added_sent {
                serde_json::json!([{
                    "id": msg_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": accumulated_content,
                        "annotations": []
                    }]
                }])
            } else {
                serde_json::json!([])
            }
        }
    });
    events.push(format!("event: response.completed\ndata: {}\n\n", completed));

    events
}

/// Parse usage from OpenAI SSE chunk (reuses logic from handlers).
pub fn parse_usage_from_sse_chunk(text: &str) -> Option<(i64, i64, i64)> {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue;
        }
        let data_str = trimmed.trim_start_matches("data:").trim();
        if data_str == "[DONE]" || data_str.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<Value>(data_str) {
            if let Some(usage) = json.get("usage") {
                let prompt = usage.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let completion = usage.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let total = usage.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                if total > 0 || prompt > 0 || completion > 0 {
                    return Some((prompt, completion, total));
                }
            }
        }
    }
    None
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
