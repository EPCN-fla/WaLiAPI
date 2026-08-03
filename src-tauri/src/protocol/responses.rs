use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// State for tracking streaming output items during OpenAI SSE → Responses SSE conversion.
///
/// Tracks both text message items and function_call items so we can emit
/// the complete Codex-compatible event chain:
///
/// For text message:
///   response.output_item.added → response.content_part.added →
///   response.output_text.delta → response.output_text.done →
///   response.content_part.done → response.output_item.done
///
/// For function_call:
///   response.output_item.added(type=function_call) →
///   response.function_call_arguments.delta →
///   response.function_call_arguments.done →
///   response.output_item.done
#[derive(Default)]
pub struct StreamState {
    /// Whether the text message output_item.added has been sent.
    pub text_item_added: bool,
    /// Whether the text message output_item.done has been sent.
    pub text_item_done: bool,
    /// Whether the text content_part.added has been sent.
    pub text_part_added: bool,
    /// The output_index assigned to the text message item.
    pub text_output_index: u32,
    /// Next output_index to use for a new output item.
    pub next_output_index: u32,
    /// Map from tool_call index → (output_index, call_id, name, accumulated_arguments, item_added_sent, arguments_done_sent)
    pub tool_calls: HashMap<u64, ToolCallState>,
    /// Whether any tool calls were seen in this stream.
    pub has_tool_calls: bool,
    /// Whether response.completed has been emitted.
    pub completed_sent: bool,
    /// Monotonic sequence number counter for all events.
    pub sequence_number: u64,
}

/// Per-tool-call streaming state.
#[derive(Clone)]
pub struct ToolCallState {
    pub output_index: u32,
    pub call_id: String,
    pub name: String,
    pub item_id: String,
    pub accumulated_arguments: String,
    pub item_added_sent: bool,
    pub arguments_done_sent: bool,
    pub output_item_done_sent: bool,
}

/// Convert an OpenAI SSE chunk (Chat Completions stream) to Responses API SSE events.
///
/// This function is called repeatedly for each upstream SSE chunk and must be stateful.
/// The `state` parameter tracks all output items (text + tool calls) across calls.
///
/// # Event chains emitted
///
/// ## Text content
/// ```text
/// response.output_item.added (type=message)
/// response.content_part.added (type=output_text)
/// response.output_text.delta (per chunk)
/// response.output_text.done (at finish)
/// response.content_part.done
/// response.output_item.done
/// ```
///
/// ## Function call (tool_calls)
/// ```text
/// response.output_item.added (type=function_call)
/// response.function_call_arguments.delta (per chunk)
/// response.function_call_arguments.done
/// response.output_item.done
/// ```
///
/// ## Final events (emitted by `create_synthetic_completed_events`)
/// ```text
/// response.completed
/// data: [DONE]
/// ```
pub fn convert_openai_sse_to_responses(
    chunk_text: &str,
    _model: &str,
    response_id: &str,
    accumulated_content: &str,
    state: &mut StreamState,
) -> Vec<String> {
    let mut events = Vec::new();
    let msg_id = if response_id.starts_with("resp_") {
        format!("msg_{}", &response_id[5..])
    } else {
        format!("msg_{}", response_id)
    };
    let reasoning_id = if response_id.starts_with("resp_") {
        format!("rs_{}", &response_id[5..])
    } else {
        format!("rs_{}", response_id)
    };

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
                    // Reasoning content delta (DeepSeek R1, OpenAI o1/o3, etc.)
                    if let Some(reasoning) =
                        delta.get("reasoning_content").and_then(|c| c.as_str())
                    {
                        if !reasoning.is_empty() {
                            let seq = next_seq(state);
                            let event = serde_json::json!({
                                "type": "response.reasoning_summary_text.delta",
                                "item_id": reasoning_id,
                                "output_index": 0,
                                "summary_index": 0,
                                "delta": reasoning,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.reasoning_summary_text.delta\ndata: {}\n\n",
                                event
                            ));
                        }
                    }

                    // Content delta (text)
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            // Emit output_item.added + content_part.added before first text delta
                            if !state.text_item_added {
                                let text_output_index = state.next_output_index;
                                state.text_output_index = text_output_index;
                                let seq = next_seq(state);
                                let item = serde_json::json!({
                                    "id": msg_id,
                                    "type": "message",
                                    "status": "in_progress",
                                    "role": "assistant",
                                    "content": []
                                });
                                let item_event = serde_json::json!({
                                    "type": "response.output_item.added",
                                    "output_index": text_output_index,
                                    "item": item,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.output_item.added\ndata: {}\n\n",
                                    item_event
                                ));

                                let seq = next_seq(state);
                                let part = serde_json::json!({
                                    "type": "output_text",
                                    "text": "",
                                    "annotations": []
                                });
                                let part_event = serde_json::json!({
                                    "type": "response.content_part.added",
                                    "item_id": msg_id,
                                    "output_index": text_output_index,
                                    "content_index": 0,
                                    "part": part,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.content_part.added\ndata: {}\n\n",
                                    part_event
                                ));

                                state.text_item_added = true;
                                state.text_part_added = true;
                                state.next_output_index += 1;
                            }

                            let text_output_index = state.text_output_index;
                            let seq = next_seq(state);

                            let event = serde_json::json!({
                                "type": "response.output_text.delta",
                                "item_id": msg_id,
                                "output_index": text_output_index,
                                "content_index": 0,
                                "delta": content,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.output_text.delta\ndata: {}\n\n",
                                event
                            ));
                        }
                    }

                    // Tool calls delta
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        state.has_tool_calls = true;

                        for tc in tool_calls {
                            let tc_index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                            let tc_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                            let func = tc.get("function");
                            let name =
                                func.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                            let arguments =
                                func.and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or("");

                            // Initialize tool call state if this is the first time we see it
                            if !state.tool_calls.contains_key(&tc_index) {
                                let output_index = state.next_output_index;
                                let item_id = if !tc_id.is_empty() {
                                    tc_id.to_string()
                                } else {
                                    format!("fc_{}", tc_index)
                                };

                                state.tool_calls.insert(
                                    tc_index,
                                    ToolCallState {
                                        output_index,
                                        call_id: tc_id.to_string(),
                                        name: name.to_string(),
                                        item_id: item_id.clone(),
                                        accumulated_arguments: String::new(),
                                        item_added_sent: false,
                                        arguments_done_sent: false,
                                        output_item_done_sent: false,
                                    },
                                );
                                state.next_output_index += 1;
                            }

                            let tc_state = state.tool_calls.get_mut(&tc_index).unwrap();

                            // Always update call_id and name if they were empty and we now have values
                            // (upstream may send id in a later chunk than the first one)
                            if tc_state.call_id.is_empty() && !tc_id.is_empty() {
                                tc_state.call_id = tc_id.to_string();
                            }
                            if tc_state.name.is_empty() && !name.is_empty() {
                                tc_state.name = name.to_string();
                            }

                            // Emit output_item.added for function_call if not yet sent
                            if !tc_state.item_added_sent {
                                // If we have a call_id and name, emit the added event
                                let effective_name = if tc_state.name.is_empty() {
                                    name.to_string()
                                } else {
                                    tc_state.name.clone()
                                };
                                let effective_call_id = if tc_state.call_id.is_empty() {
                                    tc_id.to_string()
                                } else {
                                    tc_state.call_id.clone()
                                };

                                // Update stored values if they were empty before
                                if tc_state.call_id.is_empty() && !effective_call_id.is_empty() {
                                    tc_state.call_id = effective_call_id.clone();
                                }
                                if tc_state.name.is_empty() && !effective_name.is_empty() {
                                    tc_state.name = effective_name.clone();
                                }

                                let fc_item = serde_json::json!({
                                    "id": tc_state.item_id,
                                    "type": "function_call",
                                    "status": "in_progress",
                                    "call_id": tc_state.call_id,
                                    "name": tc_state.name,
                                    "arguments": ""
                                });
                                // Increment seq before borrowing tc_state
                                state.sequence_number += 1;
                                let seq = state.sequence_number;
                                let added_event = serde_json::json!({
                                    "type": "response.output_item.added",
                                    "output_index": tc_state.output_index,
                                    "item": fc_item,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.output_item.added\ndata: {}\n\n",
                                    added_event
                                ));
                                tc_state.item_added_sent = true;
                            }

                            // Emit arguments delta if we have arguments content
                            if !arguments.is_empty() {
                                tc_state.accumulated_arguments.push_str(arguments);

                                state.sequence_number += 1;
                                let seq = state.sequence_number;
                                let delta_event = serde_json::json!({
                                    "type": "response.function_call_arguments.delta",
                                    "item_id": tc_state.item_id,
                                    "output_index": tc_state.output_index,
                                    "delta": arguments,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.function_call_arguments.delta\ndata: {}\n\n",
                                    delta_event
                                ));
                            }
                        }
                    }
                }

                // Check for finish_reason
                if let Some(finish) =
                    choice.get("finish_reason").and_then(|f| f.as_str())
                {
                    if !finish.is_empty() && finish != "null" {
                        // Close text item if it was opened and not yet closed
                        if state.text_item_added && !state.text_item_done {
                            let text_output_index = state.text_output_index;
                            let seq = next_seq(state);
                            let text_done = serde_json::json!({
                                "type": "response.output_text.done",
                                "item_id": msg_id,
                                "output_index": text_output_index,
                                "content_index": 0,
                                "text": accumulated_content,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.output_text.done\ndata: {}\n\n",
                                text_done
                            ));

                            let seq = next_seq(state);
                            let part = serde_json::json!({
                                "type": "output_text",
                                "text": accumulated_content,
                                "annotations": []
                            });
                            let part_done = serde_json::json!({
                                "type": "response.content_part.done",
                                "item_id": msg_id,
                                "output_index": text_output_index,
                                "content_index": 0,
                                "part": part,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.content_part.done\ndata: {}\n\n",
                                part_done
                            ));

                            let seq = next_seq(state);
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
                                "output_index": text_output_index,
                                "item": completed_item,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.output_item.done\ndata: {}\n\n",
                                item_done
                            ));

                            state.text_item_done = true;
                        }

                        // Ensure all tool calls have non-empty call_id before closing them.
                        // Some upstreams never send a tool_call id in streaming chunks.
                        for (_, tc_state) in state.tool_calls.iter_mut() {
                            if tc_state.call_id.is_empty() {
                                tc_state.call_id = format!("call_{}", tc_state.output_index);
                            }
                        }

                        // Close all tool call items
                        // Collect tool call data first to avoid double mutable borrow of state
                        let tool_calls_data: Vec<(u64, String, String, String, String, bool, bool, bool)> =
                            state.tool_calls.iter().map(|(_, tc)| {
                                (
                                    tc.output_index as u64,
                                    tc.item_id.clone(),
                                    tc.call_id.clone(),
                                    tc.name.clone(),
                                    tc.accumulated_arguments.clone(),
                                    tc.item_added_sent,
                                    tc.arguments_done_sent,
                                    tc.output_item_done_sent,
                                )
                            }).collect();

                        for (output_index, item_id, call_id, name, accumulated_args, _item_added, arguments_done, output_item_done) in &tool_calls_data {
                            if !arguments_done {
                                let seq = next_seq(state);
                                let args_done = serde_json::json!({
                                    "type": "response.function_call_arguments.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "arguments": accumulated_args,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.function_call_arguments.done\ndata: {}\n\n",
                                    args_done
                                ));
                            }

                            if !output_item_done {
                                let seq = next_seq(state);
                                let fc_completed = serde_json::json!({
                                    "id": item_id,
                                    "type": "function_call",
                                    "status": "completed",
                                    "call_id": call_id,
                                    "name": name,
                                    "arguments": accumulated_args
                                });
                                let item_done = serde_json::json!({
                                    "type": "response.output_item.done",
                                    "output_index": output_index,
                                    "item": fc_completed,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.output_item.done\ndata: {}\n\n",
                                    item_done
                                ));
                            }
                        }

                        // Mark tool calls as done
                        for (_, tc_state) in state.tool_calls.iter_mut() {
                            tc_state.arguments_done_sent = true;
                            tc_state.output_item_done_sent = true;
                        }

                        // Note: response.completed is NOT sent here. It's sent after the stream ends,
                        // so we can include usage from the final usage chunk (which comes after finish_reason).
                    }
                }
            }
        }
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
        "output": [],
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "metadata": null,
        "parallel_tool_calls": false,
        "temperature": null,
        "tool_choice": "auto",
        "tools": [],
        "top_p": null,
        "truncation": null,
        "usage": null,
        "background": false,
        "completed_at": null
    });

    let created_event = serde_json::json!({
        "type": "response.created",
        "response": response_obj,
        "sequence_number": 0
    });

    let in_progress_event = serde_json::json!({
        "type": "response.in_progress",
        "response": response_obj,
        "sequence_number": 1
    });

    format!(
        "event: response.created\ndata: {}\n\nevent: response.in_progress\ndata: {}\n\n",
        created_event, in_progress_event
    )
}

/// Create synthetic closing events when upstream stream ends.
/// Emits closing events for any still-open items (text and/or tool calls),
/// then emits response.completed with usage.
///
/// This is called:
/// - When the upstream stream ends without a finish_reason (synthetic close)
/// - When the upstream stream ends with finish_reason but response.completed hasn't been sent yet
///   (because response.completed needs usage data which comes in the final chunk)
pub fn create_synthetic_completed_events(
    model: &str,
    response_id: &str,
    accumulated_content: &str,
    state: &StreamState,
    usage_prompt: i64,
    usage_completion: i64,
) -> Vec<String> {
    let mut events = Vec::new();
    let msg_id = if response_id.starts_with("resp_") {
        format!("msg_{}", &response_id[5..])
    } else {
        format!("msg_{}", response_id)
    };

    // We need a mutable state to track sequence numbers, but we receive &StreamState.
    // Use a local counter starting from the state's current sequence_number.
    let mut seq = state.sequence_number;

    macro_rules! next_seq {
        () => {{
            seq += 1;
            seq
        }};
    }

    // Close text item if it was opened and not yet closed
    if state.text_item_added && !state.text_item_done {
        let text_output_index = state.text_output_index;

        let s = next_seq!();
        let text_done = serde_json::json!({
            "type": "response.output_text.done",
            "item_id": msg_id,
            "output_index": text_output_index,
            "content_index": 0,
            "text": accumulated_content,
            "sequence_number": s
        });
        events.push(format!("event: response.output_text.done\ndata: {}\n\n", text_done));

        let s = next_seq!();
        let part = serde_json::json!({
            "type": "output_text",
            "text": accumulated_content,
            "annotations": []
        });
        let part_done = serde_json::json!({
            "type": "response.content_part.done",
            "item_id": msg_id,
            "output_index": text_output_index,
            "content_index": 0,
            "part": part,
            "sequence_number": s
        });
        events.push(format!("event: response.content_part.done\ndata: {}\n\n", part_done));

        let s = next_seq!();
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
            "output_index": text_output_index,
            "item": completed_item,
            "sequence_number": s
        });
        events.push(format!("event: response.output_item.done\ndata: {}\n\n", item_done));
    }

    // Close any still-open tool call items
    for (_, tc_state) in state.tool_calls.iter() {
        // Fallback: ensure call_id is never empty
        let effective_call_id = if tc_state.call_id.is_empty() {
            format!("call_{}", tc_state.output_index)
        } else {
            tc_state.call_id.clone()
        };
        if !tc_state.arguments_done_sent {
            let s = next_seq!();
            let args_done = serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": tc_state.item_id,
                "output_index": tc_state.output_index,
                "arguments": tc_state.accumulated_arguments,
                "sequence_number": s
            });
            events.push(format!("event: response.function_call_arguments.done\ndata: {}\n\n", args_done));
        }

        if !tc_state.output_item_done_sent {
            let s = next_seq!();
            let fc_completed = serde_json::json!({
                "id": tc_state.item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": effective_call_id,
                "name": tc_state.name,
                "arguments": tc_state.accumulated_arguments
            });
            let item_done = serde_json::json!({
                "type": "response.output_item.done",
                "output_index": tc_state.output_index,
                "item": fc_completed,
                "sequence_number": s
            });
            events.push(format!("event: response.output_item.done\ndata: {}\n\n", item_done));
        }
    }

    // Build the output array for response.completed
    let mut output_items: Vec<Value> = Vec::new();

    // Add text item to output if it was added
    if state.text_item_added {
        output_items.push(serde_json::json!({
            "id": msg_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": accumulated_content,
                "annotations": []
            }]
        }));
    }

    // Add tool call items to output
    for (_, tc_state) in state.tool_calls.iter() {
        let effective_call_id = if tc_state.call_id.is_empty() {
            format!("call_{}", tc_state.output_index)
        } else {
            tc_state.call_id.clone()
        };
        output_items.push(serde_json::json!({
            "id": tc_state.item_id,
            "type": "function_call",
            "status": "completed",
            "call_id": effective_call_id,
            "name": tc_state.name,
            "arguments": tc_state.accumulated_arguments
        }));
    }

    let s = next_seq!();
    // response.completed (with usage)
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": now_ts(),
            "status": "completed",
            "model": model,
            "output": output_items,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "metadata": null,
            "parallel_tool_calls": false,
            "temperature": null,
            "tool_choice": "auto",
            "tools": [],
            "top_p": null,
            "truncation": null,
            "background": false,
            "completed_at": now_ts(),
            "usage": {
                "input_tokens": usage_prompt,
                "output_tokens": usage_completion,
                "total_tokens": usage_prompt + usage_completion
            }
        },
        "sequence_number": s
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

/// Get the next sequence number from StreamState.
fn next_seq(state: &mut StreamState) -> u64 {
    state.sequence_number += 1;
    state.sequence_number
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_event_types(events: &[String]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| {
                // Each event string is like: "event: response.output_item.added\ndata: ...\n\n"
                let first_line = e.lines().next()?;
                if first_line.starts_with("event: ") {
                    Some(first_line.trim_start_matches("event: ").trim().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    fn extract_event_data(event: &str) -> Value {
        let data_line = event
            .lines()
            .find(|l| l.starts_with("data: "))
            .unwrap()
            .trim_start_matches("data: ")
            .trim();
        serde_json::from_str(data_line).unwrap()
    }

    #[test]
    fn test_text_only_stream() {
        let mut state = StreamState::default();
        let response_id = "resp_test123";

        // Chunk 1: text delta
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let events1 = convert_openai_sse_to_responses(
            chunk1, "gpt-4", response_id, "Hello", &mut state,
        );
        let types1 = extract_event_types(&events1);
        assert_eq!(
            types1,
            vec![
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ]
        );

        // Verify output_index for output_item.added is 0
        let added_data = extract_event_data(&events1[0]);
        assert_eq!(added_data["output_index"], 0);
        assert_eq!(added_data["item"]["type"], "message");

        // Chunk 2: more text
        let chunk2 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#;
        let events2 = convert_openai_sse_to_responses(
            chunk2, "gpt-4", response_id, "Hello world", &mut state,
        );
        let types2 = extract_event_types(&events2);
        assert_eq!(types2, vec!["response.output_text.delta"]);

        // Chunk 3: finish
        let chunk3 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let events3 = convert_openai_sse_to_responses(
            chunk3, "gpt-4", response_id, "Hello world", &mut state,
        );
        let types3 = extract_event_types(&events3);
        assert_eq!(
            types3,
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
            ]
        );

        // Verify output_index in done events
        let text_done_data = extract_event_data(&events3[0]);
        assert_eq!(text_done_data["output_index"], 0);
        assert_eq!(text_done_data["text"], "Hello world");
    }

    #[test]
    fn test_tool_call_only_stream() {
        let mut state = StreamState::default();
        let response_id = "resp_test456";

        // Chunk 1: tool call start (id + name)
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#;
        let events1 = convert_openai_sse_to_responses(
            chunk1, "gpt-4", response_id, "", &mut state,
        );
        let types1 = extract_event_types(&events1);
        assert_eq!(
            types1,
            vec!["response.output_item.added"]
        );

        // Verify it's a function_call item
        let added_data = extract_event_data(&events1[0]);
        assert_eq!(added_data["item"]["type"], "function_call");
        assert_eq!(added_data["item"]["call_id"], "call_abc");
        assert_eq!(added_data["item"]["name"], "get_weather");
        assert_eq!(added_data["output_index"], 0);

        // Chunk 2: arguments delta
        let chunk2 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"SF\"}"}}]},"finish_reason":null}]}"#;
        let events2 = convert_openai_sse_to_responses(
            chunk2, "gpt-4", response_id, "", &mut state,
        );
        let types2 = extract_event_types(&events2);
        assert_eq!(types2, vec!["response.function_call_arguments.delta"]);

        // Verify output_index in arguments delta
        let args_delta_data = extract_event_data(&events2[0]);
        assert_eq!(args_delta_data["output_index"], 0);
        assert_eq!(args_delta_data["delta"], "{\"city\":\"SF\"}");

        // Chunk 3: finish with tool_calls
        let chunk3 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        let events3 = convert_openai_sse_to_responses(
            chunk3, "gpt-4", response_id, "", &mut state,
        );
        let types3 = extract_event_types(&events3);
        assert_eq!(
            types3,
            vec![
                "response.function_call_arguments.done",
                "response.output_item.done",
            ]
        );

        // Verify output_item.done has function_call type
        let item_done_data = extract_event_data(&events3[1]);
        assert_eq!(item_done_data["item"]["type"], "function_call");
        assert_eq!(item_done_data["item"]["arguments"], "{\"city\":\"SF\"}");
    }

    #[test]
    fn test_text_then_tool_call_stream() {
        let mut state = StreamState::default();
        let response_id = "resp_test789";

        // Chunk 1: text
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Let me check"},"finish_reason":null}]}"#;
        let _ = convert_openai_sse_to_responses(
            chunk1, "gpt-4", response_id, "Let me check", &mut state,
        );
        assert_eq!(state.text_output_index, 0);
        assert_eq!(state.next_output_index, 1);

        // Chunk 2: tool call start
        let chunk2 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_xyz","function":{"name":"search","arguments":""}}]},"finish_reason":null}]}"#;
        let events2 = convert_openai_sse_to_responses(
            chunk2, "gpt-4", response_id, "Let me check", &mut state,
        );
        let types2 = extract_event_types(&events2);
        assert_eq!(types2, vec!["response.output_item.added"]);

        // Verify tool call gets output_index=1 (after text's index 0)
        let added_data = extract_event_data(&events2[0]);
        assert_eq!(added_data["output_index"], 1);
        assert_eq!(added_data["item"]["type"], "function_call");

        // Chunk 3: arguments
        let chunk3 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#;
        let events3 = convert_openai_sse_to_responses(
            chunk3, "gpt-4", response_id, "Let me check", &mut state,
        );
        let types3 = extract_event_types(&events3);
        assert_eq!(types3, vec!["response.function_call_arguments.delta"]);

        // Chunk 4: finish
        let chunk4 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        let events4 = convert_openai_sse_to_responses(
            chunk4, "gpt-4", response_id, "Let me check", &mut state,
        );
        let types4 = extract_event_types(&events4);
        assert_eq!(
            types4,
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
            ]
        );

        // Verify text done uses index 0
        let text_done = extract_event_data(&events4[0]);
        assert_eq!(text_done["output_index"], 0);

        // Verify function_call done uses index 1
        let fc_item_done = extract_event_data(&events4[4]);
        assert_eq!(fc_item_done["output_index"], 1);
        assert_eq!(fc_item_done["item"]["type"], "function_call");
    }

    #[test]
    fn test_synthetic_completed_with_tool_calls() {
        let mut state = StreamState::default();
        let response_id = "resp_test_syn";

        // Simulate: tool call only, no finish_reason in stream
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"test","arguments":"{\"x\":1}"}}]},"finish_reason":null}]}"#;
        let _ = convert_openai_sse_to_responses(
            chunk1, "gpt-4", response_id, "", &mut state,
        );

        // Stream ends without finish_reason — call synthetic completed
        let synth = create_synthetic_completed_events(
            "gpt-4", response_id, "", &state, 10, 20,
        );
        let synth_types = extract_event_types(&synth);
        assert_eq!(
            synth_types,
            vec![
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        // Verify response.completed has function_call in output
        let completed_data = extract_event_data(&synth[2]);
        assert_eq!(completed_data["response"]["output"][0]["type"], "function_call");
        assert_eq!(completed_data["response"]["usage"]["input_tokens"], 10);
        assert_eq!(completed_data["response"]["usage"]["output_tokens"], 20);
    }
}
