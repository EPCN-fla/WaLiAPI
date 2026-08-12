use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Reassembles upstream SSE records that arrive fragmented across TCP chunks.
///
/// ResponsesViaChat has no codec decoder, so a record split across TCP frames
/// would otherwise be fed to [`convert_openai_sse_to_responses`] as several
/// half-records and silently dropped (mid-JSON fragments never parse). Only
/// complete records are returned here. This mirrors the `encode_responses_buffered`
/// reassembly in the StreamPumpCore path — tool names / call ids / argument
/// fragments are lost without it.
#[derive(Default)]
pub struct ResponsesSseAssembler {
    pending: Vec<u8>,
}

impl ResponsesSseAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one upstream chunk; returns every COMPLETE SSE record it contains.
    /// A record whose terminator hasn't arrived yet is buffered for the next call.
    ///
    /// Takes raw bytes, not `&str`: a TCP/HTTP chunk boundary may fall inside a
    /// UTF-8 codepoint (common with 3-byte CJK text), so callers must not gate
    /// on `str::from_utf8`.  Bytes are buffered and only COMPLETE records are
    /// decoded, so a mid-codepoint split is reassembled across calls.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut records = Vec::new();
        while let Some(end) = crate::protocol::codec::sse::record_end(&self.pending) {
            let record: Vec<u8> = self.pending.drain(..end).collect();
            records.push(String::from_utf8_lossy(&record).into_owned());
        }
        records
    }

    /// Whether bytes are still buffered awaiting a record terminator.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Flush any trailing bytes at EOF as a final record (a record that
    /// terminated exactly at EOF must not be lost).
    pub fn flush(&mut self) -> Vec<String> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let tail = std::mem::take(&mut self.pending);
        vec![String::from_utf8_lossy(&tail).into_owned()]
    }
}

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
///
/// For reasoning (DeepSeek R1, OpenAI o1/o3, etc.):
///   response.output_item.added(type=reasoning) →
///   response.reasoning_summary_part.added →
///   response.reasoning_summary_text.delta (per chunk) →
///   response.reasoning_summary_text.done →
///   response.reasoning_summary_part.done →
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
    /// Whether the reasoning output_item.added has been sent.
    pub reasoning_item_added: bool,
    /// Whether the reasoning summary_part.added has been sent.
    pub reasoning_part_added: bool,
    /// Whether the reasoning summary_part.done has been sent.
    pub reasoning_part_done: bool,
    /// Whether the reasoning output_item.done has been sent.
    pub reasoning_item_done: bool,
    /// The output_index assigned to the reasoning item.
    pub reasoning_output_index: u32,
    /// Full concatenated reasoning text accumulated so far.
    pub accumulated_reasoning: String,
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
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str())
                    {
                        if !reasoning.is_empty() {
                            // Announce the reasoning item (output_item.added) and its
                            // summary part BEFORE the first delta. Clients only persist
                            // items they saw "added" — without this the reasoning never
                            // enters the conversation and thinking-mode providers reject
                            // the next turn.
                            if !state.reasoning_item_added {
                                let reasoning_output_index = state.next_output_index;
                                state.reasoning_output_index = reasoning_output_index;
                                let seq = next_seq(state);
                                let item = serde_json::json!({
                                    "id": reasoning_id,
                                    "type": "reasoning",
                                    "status": "in_progress",
                                    "summary": [],
                                    "content": []
                                });
                                let item_event = serde_json::json!({
                                    "type": "response.output_item.added",
                                    "output_index": reasoning_output_index,
                                    "item": item,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.output_item.added\ndata: {}\n\n",
                                    item_event
                                ));

                                let seq = next_seq(state);
                                let part = serde_json::json!({
                                    "type": "reasoning_summary_text",
                                    "text": ""
                                });
                                let part_event = serde_json::json!({
                                    "type": "response.reasoning_summary_part.added",
                                    "item_id": reasoning_id,
                                    "output_index": reasoning_output_index,
                                    "summary_index": 0,
                                    "part": part,
                                    "sequence_number": seq
                                });
                                events.push(format!(
                                    "event: response.reasoning_summary_part.added\ndata: {}\n\n",
                                    part_event
                                ));

                                state.reasoning_item_added = true;
                                state.reasoning_part_added = true;
                                state.next_output_index += 1;
                            }

                            state.accumulated_reasoning.push_str(reasoning);
                            let reasoning_output_index = state.reasoning_output_index;
                            let seq = next_seq(state);
                            let event = serde_json::json!({
                                "type": "response.reasoning_summary_text.delta",
                                "item_id": reasoning_id,
                                "output_index": reasoning_output_index,
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
                            let name = func
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("");
                            let arguments = func
                                .and_then(|f| f.get("arguments"))
                                .and_then(|a| a.as_str())
                                .unwrap_or("");

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

                            // Emit arguments delta if we have arguments content — but never
                            // after the .done has already been sent (some upstreams re-send
                            // or deliver a trailing chunk after finish_reason).
                            if !arguments.is_empty() && !tc_state.arguments_done_sent {
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
                if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    if !finish.is_empty() && finish != "null" {
                        // Close reasoning item if it was opened and not yet closed
                        if state.reasoning_item_added && !state.reasoning_item_done {
                            let reasoning_output_index = state.reasoning_output_index;
                            let seq = next_seq(state);
                            let text_done = serde_json::json!({
                                "type": "response.reasoning_summary_text.done",
                                "item_id": reasoning_id,
                                "output_index": reasoning_output_index,
                                "summary_index": 0,
                                "text": state.accumulated_reasoning,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.reasoning_summary_text.done\ndata: {}\n\n",
                                text_done
                            ));

                            let seq = next_seq(state);
                            let part = serde_json::json!({
                                "type": "reasoning_summary_text",
                                "text": state.accumulated_reasoning
                            });
                            let part_done = serde_json::json!({
                                "type": "response.reasoning_summary_part.done",
                                "item_id": reasoning_id,
                                "output_index": reasoning_output_index,
                                "summary_index": 0,
                                "part": part,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.reasoning_summary_part.done\ndata: {}\n\n",
                                part_done
                            ));

                            let seq = next_seq(state);
                            let completed_item = serde_json::json!({
                                "id": reasoning_id,
                                "type": "reasoning",
                                "status": "completed",
                                "summary": [{
                                    "type": "summary_text",
                                    "text": state.accumulated_reasoning
                                }],
                                "content": []
                            });
                            let item_done = serde_json::json!({
                                "type": "response.output_item.done",
                                "output_index": reasoning_output_index,
                                "item": completed_item,
                                "sequence_number": seq
                            });
                            events.push(format!(
                                "event: response.output_item.done\ndata: {}\n\n",
                                item_done
                            ));

                            state.reasoning_part_done = true;
                            state.reasoning_item_done = true;
                        }

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
                        let tool_calls_data: Vec<(
                            u64,
                            String,
                            String,
                            String,
                            String,
                            bool,
                            bool,
                            bool,
                        )> = state
                            .tool_calls
                            .iter()
                            .map(|(_, tc)| {
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
                            })
                            .collect();

                        for (
                            output_index,
                            item_id,
                            call_id,
                            name,
                            accumulated_args,
                            _item_added,
                            arguments_done,
                            output_item_done,
                        ) in &tool_calls_data
                        {
                            if !arguments_done {
                                let seq = next_seq(state);
                                let args_done = serde_json::json!({
                                    "type": "response.function_call_arguments.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "name": name,
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

    // Close reasoning item if it was opened and not yet closed
    if state.reasoning_item_added && !state.reasoning_item_done {
        let reasoning_output_index = state.reasoning_output_index;
        let reasoning_id = if response_id.starts_with("resp_") {
            format!("rs_{}", &response_id[5..])
        } else {
            format!("rs_{}", response_id)
        };

        let s = next_seq!();
        let text_done = serde_json::json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": reasoning_id,
            "output_index": reasoning_output_index,
            "summary_index": 0,
            "text": state.accumulated_reasoning,
            "sequence_number": s
        });
        events.push(format!(
            "event: response.reasoning_summary_text.done\ndata: {}\n\n",
            text_done
        ));

        let s = next_seq!();
        let part = serde_json::json!({
            "type": "reasoning_summary_text",
            "text": state.accumulated_reasoning
        });
        let part_done = serde_json::json!({
            "type": "response.reasoning_summary_part.done",
            "item_id": reasoning_id,
            "output_index": reasoning_output_index,
            "summary_index": 0,
            "part": part,
            "sequence_number": s
        });
        events.push(format!(
            "event: response.reasoning_summary_part.done\ndata: {}\n\n",
            part_done
        ));

        let s = next_seq!();
        let completed_item = serde_json::json!({
            "id": reasoning_id,
            "type": "reasoning",
            "status": "completed",
            "summary": [{
                "type": "summary_text",
                "text": state.accumulated_reasoning
            }],
            "content": []
        });
        let item_done = serde_json::json!({
            "type": "response.output_item.done",
            "output_index": reasoning_output_index,
            "item": completed_item,
            "sequence_number": s
        });
        events.push(format!(
            "event: response.output_item.done\ndata: {}\n\n",
            item_done
        ));
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
        events.push(format!(
            "event: response.output_text.done\ndata: {}\n\n",
            text_done
        ));

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
        events.push(format!(
            "event: response.content_part.done\ndata: {}\n\n",
            part_done
        ));

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
        events.push(format!(
            "event: response.output_item.done\ndata: {}\n\n",
            item_done
        ));
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
                "name": tc_state.name,
                "arguments": tc_state.accumulated_arguments,
                "sequence_number": s
            });
            events.push(format!(
                "event: response.function_call_arguments.done\ndata: {}\n\n",
                args_done
            ));
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
            events.push(format!(
                "event: response.output_item.done\ndata: {}\n\n",
                item_done
            ));
        }
    }

    // Build the output array for response.completed
    let mut output_items: Vec<Value> = Vec::new();

    // Add reasoning item to output if it was added
    if state.reasoning_item_added {
        let reasoning_id = if response_id.starts_with("resp_") {
            format!("rs_{}", &response_id[5..])
        } else {
            format!("rs_{}", response_id)
        };
        output_items.push(serde_json::json!({
            "id": reasoning_id,
            "type": "reasoning",
            "status": "completed",
            "summary": [{
                "type": "summary_text",
                "text": state.accumulated_reasoning
            }],
            "content": []
        }));
    }

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
                "input_tokens_details": {
                    "cached_tokens": 0,
                    "cache_write_tokens": 0
                },
                "output_tokens": usage_completion,
                "output_tokens_details": {
                    "reasoning_tokens": 0
                },
                "total_tokens": usage_prompt + usage_completion
            }
        },
        "sequence_number": s
    });
    events.push(format!(
        "event: response.completed\ndata: {}\n\n",
        completed
    ));

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

    /// 63 raw upstream fragments captured from `handle_responses_stream` via
    /// `WALIAPI_DEBUG_SSE` instrumentation (deepseek-v4-flash / OpenCode-GO
    /// channel, 2026-08-08). Every SSE record is split across multiple TCP
    /// chunks — often mid-JSON, with the `\n\n` terminator landing in a fragment
    /// that starts mid-record. This is the real-world fragmentation that used to
    /// drop tool names / call ids / argument fragments.
    const REAL_FRAGMENTS: &[&str] = &[
        "data: {\"id\":\"adba4265-1f45-4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning_content\":\"\"}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk",
        "\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\"I\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk",
        "\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\"'ll\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f4",
        "5-4b6f-a564-ef2ca7a6e353\",",
        "\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\" read\",\"reasoning_content\":null}}],\"usage\":null}\n\ndata: {\"id\":\"adba4265-1f45-4b6f-a564-ef2ca7a6e353\",\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\" the\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f4",
        "5-4b6f-a564-ef2ca7a6e353\",\"",
        "object\":\"chat.completion.chu",
        "nk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\" file\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk",
        "\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\" at\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f4",
        "5-4b6f-a564-ef2ca7a6e353\",",
        "\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\" that\",\"reasoning_content\":null}}],\"usage\":null}\n\ndata: {\"id\":\"adba4265-1f45-4b6f-a564-ef2ca7a6e353\",\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\" path\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f4",
        "5-4b6f-a564-ef2ca7a6e353\",\"",
        "object\":\"chat.completion.chu",
        "nk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"content\":\".\",\"reasoning_content\":null}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"o",
        "bject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_00_ET_qpwrSuOGqdNVOyDYESq94260\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f4",
        "5-4b6f-a564-ef2ca7a6e353\",",
        "\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"\"}}]}}],\"usage\":null}\n\ndata: {\"id\":\"adba4265-1f45-4b6f-a564-ef2ca7a6e353\",\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"path\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45",
        "-",
        "4b6f-a564-ef2ca7a6e353\",\"",
        "object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\": \"}}]}}],\"usage\":null}\n\ndata: {\"id\":\"adba4265-1f45-4b6f-a564-ef2ca7a6e353\",\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f4",
        "5-4b6f-a564-ef2ca7a6e353\",\"",
        "object\":\"chat.completion.chu",
        "nk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"/\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"tmp\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45",
        "-",
        "4b6f-a564-ef2ca7a6e353\",\"",
        "object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"/x\"}}]}}],\"usage\":null}\n\ndata: {\"id\":\"adba4265-1f45-4b6f-a564-ef2ca7a6e353\",\"object\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"ob",
        "ject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"logprobs\":null,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]}}],\"usage\":null}\n\n",
        "data: {\"id\":\"adba4265-1f45-",
        "4b6f-a564-ef2ca7a6e353\",\"o",
        "bject\":\"chat.completion.chunk\",\"created\":1786166337,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"logprobs\":null,\"delta\":{\"content\":\"\",\"reasoning_content\":null}}],\"usage\":{\"prompt_tokens\":348,\"completion_tokens\":53,\"total_tokens\":401,\"prompt_cache_hit_tokens\":256,\"prompt_cache_miss_tokens\":92,\"prompt_tokens_details\":{\"cached_tokens\":256},\"completion_tokens_details\":{\"reasoning_tokens\":0}}}\n\n",
        "data: [DONE]\n\n",
        "data: {\"choices\":[],\"cost\":\"0\"}\n\n",
    ];

    /// Feed each raw fragment through the record-reassembly seam (the same logic
    /// the handler uses), then convert each complete record. This reproduces the
    /// real upstream fragmentation; without reassembly the tool-call announcement
    /// record (carrying name + id) and most argument-delta records are dropped.
    fn run_reassembled(fragments: &[&str]) -> Vec<String> {
        let mut state = StreamState::default();
        let mut events = Vec::new();
        let mut asm = ResponsesSseAssembler::new();
        for frag in fragments {
            for record in asm.push(frag.as_bytes()) {
                events.extend(convert_openai_sse_to_responses(
                    &record,
                    "deepseek-v4-flash",
                    "resp_test",
                    "",
                    &mut state,
                ));
            }
        }
        for record in asm.flush() {
            events.extend(convert_openai_sse_to_responses(
                &record,
                "deepseek-v4-flash",
                "resp_test",
                "",
                &mut state,
            ));
        }
        events
    }

    #[test]
    fn reassembled_tool_call_survives_real_fragmentation() {
        let events = run_reassembled(REAL_FRAGMENTS);
        let types = extract_event_types(&events);

        // The text message must come through intact.
        assert!(
            types.contains(&"response.output_item.added".to_string()),
            "expected a tool call item to be added"
        );

        // The function_call output_item.added must carry the real name + call_id.
        let added = events
            .iter()
            .filter(|e| {
                extract_event_types(&[(*e).clone()])
                    == vec!["response.output_item.added".to_string()]
            })
            .map(|e| extract_event_data(e))
            .find(|d| d["item"]["type"] == "function_call")
            .expect("a function_call output_item.added must be emitted");

        assert_eq!(
            added["item"]["name"], "read",
            "tool call name lost by fragmentation: got {}",
            added["item"]["name"]
        );
        assert_eq!(
            added["item"]["call_id"], "call_00_ET_qpwrSuOGqdNVOyDYESq94260",
            "tool call id lost by fragmentation: got {}",
            added["item"]["call_id"]
        );
        assert_eq!(
            added["item"]["id"], "call_00_ET_qpwrSuOGqdNVOyDYESq94260",
            "tool call item id must not fall back to fc_0"
        );

        // The final function_call output_item.done must carry full arguments.
        let done = events
            .iter()
            .filter(|e| {
                extract_event_types(&[(*e).clone()])
                    == vec!["response.output_item.done".to_string()]
            })
            .map(|e| extract_event_data(e))
            .find(|d| d["item"]["type"] == "function_call")
            .expect("a function_call output_item.done must be emitted");

        assert_eq!(
            done["item"]["arguments"], "{\"path\": \"/tmp/x\"}",
            "tool call arguments truncated by fragmentation: got {}",
            done["item"]["arguments"]
        );
        assert_eq!(done["item"]["name"], "read");
        assert_eq!(
            done["item"]["call_id"], "call_00_ET_qpwrSuOGqdNVOyDYESq94260",
            "tool call id must not fall back to call_1"
        );
    }

    #[test]
    fn test_text_only_stream() {
        let mut state = StreamState::default();
        let response_id = "resp_test123";

        // Chunk 1: text delta
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let events1 =
            convert_openai_sse_to_responses(chunk1, "gpt-4", response_id, "Hello", &mut state);
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
            chunk2,
            "gpt-4",
            response_id,
            "Hello world",
            &mut state,
        );
        let types2 = extract_event_types(&events2);
        assert_eq!(types2, vec!["response.output_text.delta"]);

        // Chunk 3: finish
        let chunk3 =
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let events3 = convert_openai_sse_to_responses(
            chunk3,
            "gpt-4",
            response_id,
            "Hello world",
            &mut state,
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
        let events1 = convert_openai_sse_to_responses(chunk1, "gpt-4", response_id, "", &mut state);
        let types1 = extract_event_types(&events1);
        assert_eq!(types1, vec!["response.output_item.added"]);

        // Verify it's a function_call item
        let added_data = extract_event_data(&events1[0]);
        assert_eq!(added_data["item"]["type"], "function_call");
        assert_eq!(added_data["item"]["call_id"], "call_abc");
        assert_eq!(added_data["item"]["name"], "get_weather");
        assert_eq!(added_data["output_index"], 0);

        // Chunk 2: arguments delta
        let chunk2 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"SF\"}"}}]},"finish_reason":null}]}"#;
        let events2 = convert_openai_sse_to_responses(chunk2, "gpt-4", response_id, "", &mut state);
        let types2 = extract_event_types(&events2);
        assert_eq!(types2, vec!["response.function_call_arguments.delta"]);

        // Verify output_index in arguments delta
        let args_delta_data = extract_event_data(&events2[0]);
        assert_eq!(args_delta_data["output_index"], 0);
        assert_eq!(args_delta_data["delta"], "{\"city\":\"SF\"}");

        // Chunk 3: finish with tool_calls
        let chunk3 =
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        let events3 = convert_openai_sse_to_responses(chunk3, "gpt-4", response_id, "", &mut state);
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

        // Chunk 4: stray trailing arguments delta AFTER .done — must be suppressed.
        let chunk4 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"EXTRA"}}]},"finish_reason":null}]}"#;
        let events4 = convert_openai_sse_to_responses(chunk4, "gpt-4", response_id, "", &mut state);
        assert!(
            extract_event_types(&events4).is_empty(),
            "no delta may be re-emitted after function_call_arguments.done: {:?}",
            events4
        );
        // The stray arguments must not leak into the accumulated result either.
        assert_eq!(
            state.tool_calls[&0].accumulated_arguments,
            "{\"city\":\"SF\"}"
        );
    }

    #[test]
    fn test_text_then_tool_call_stream() {
        let mut state = StreamState::default();
        let response_id = "resp_test789";

        // Chunk 1: text
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Let me check"},"finish_reason":null}]}"#;
        let _ = convert_openai_sse_to_responses(
            chunk1,
            "gpt-4",
            response_id,
            "Let me check",
            &mut state,
        );
        assert_eq!(state.text_output_index, 0);
        assert_eq!(state.next_output_index, 1);

        // Chunk 2: tool call start
        let chunk2 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_xyz","function":{"name":"search","arguments":""}}]},"finish_reason":null}]}"#;
        let events2 = convert_openai_sse_to_responses(
            chunk2,
            "gpt-4",
            response_id,
            "Let me check",
            &mut state,
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
            chunk3,
            "gpt-4",
            response_id,
            "Let me check",
            &mut state,
        );
        let types3 = extract_event_types(&events3);
        assert_eq!(types3, vec!["response.function_call_arguments.delta"]);

        // Chunk 4: finish
        let chunk4 =
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        let events4 = convert_openai_sse_to_responses(
            chunk4,
            "gpt-4",
            response_id,
            "Let me check",
            &mut state,
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

        // function_call_arguments.done carries the required `name`
        let fc_args_done = extract_event_data(&events4[3]);
        assert_eq!(
            fc_args_done["type"],
            "response.function_call_arguments.done"
        );
        assert_eq!(fc_args_done["name"], "search");

        // Verify function_call done uses index 1
        let fc_item_done = extract_event_data(&events4[4]);
        assert_eq!(fc_item_done["output_index"], 1);
        assert_eq!(fc_item_done["item"]["type"], "function_call");
    }

    #[test]
    fn test_reasoning_item_full_lifecycle() {
        // A reasoning_content delta must announce a `reasoning` item with
        // output_item.added BEFORE any delta, and close it with
        // output_item.done. Without the item lifecycle, Codex never persists a
        // reasoning item, so the next turn omits reasoning_content and
        // DeepSeek rejects it ("must be passed back to the API").
        let mut state = StreamState::default();
        let response_id = "resp_rs123";

        // Chunk 1: reasoning delta
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"Let me"},"finish_reason":null}]}"#;
        let events1 = convert_openai_sse_to_responses(
            chunk1,
            "deepseek-v4-flash",
            response_id,
            "",
            &mut state,
        );
        let types1 = extract_event_types(&events1);
        assert_eq!(
            types1,
            vec![
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
            ]
        );

        // reasoning item announced with type=reasoning
        let added = extract_event_data(&events1[0]);
        assert_eq!(added["item"]["type"], "reasoning");
        assert_eq!(added["item"]["id"], "rs_rs123");
        assert_eq!(added["output_index"], 0);

        // summary part added before deltas
        let part_added = extract_event_data(&events1[1]);
        assert_eq!(part_added["part"]["type"], "reasoning_summary_text");

        // delta carries the reasoning text on item rs_
        let delta = extract_event_data(&events1[2]);
        assert_eq!(delta["delta"], "Let me");
        assert_eq!(delta["item_id"], "rs_rs123");

        // Chunk 2: more reasoning
        let chunk2 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":" think."},"finish_reason":null}]}"#;
        let events2 = convert_openai_sse_to_responses(
            chunk2,
            "deepseek-v4-flash",
            response_id,
            "",
            &mut state,
        );
        let types2 = extract_event_types(&events2);
        assert_eq!(types2, vec!["response.reasoning_summary_text.delta"]);

        // Chunk 3: content text
        let chunk3 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Answer"},"finish_reason":null}]}"#;
        let events3 = convert_openai_sse_to_responses(
            chunk3,
            "deepseek-v4-flash",
            response_id,
            "Answer",
            &mut state,
        );
        let types3 = extract_event_types(&events3);
        assert_eq!(
            types3,
            vec![
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ]
        );
        // text item gets output_index 1 (reasoning took 0)
        let text_added = extract_event_data(&events3[0]);
        assert_eq!(text_added["output_index"], 1);
        assert_eq!(text_added["item"]["type"], "message");

        // Chunk 4: finish
        let chunk4 =
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let events4 = convert_openai_sse_to_responses(
            chunk4,
            "deepseek-v4-flash",
            response_id,
            "Answer",
            &mut state,
        );
        let types4 = extract_event_types(&events4);
        assert_eq!(
            types4,
            vec![
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
            ]
        );

        // reasoning_summary_text.done carries the full accumulated text
        let rs_text_done = extract_event_data(&events4[0]);
        assert_eq!(rs_text_done["type"], "response.reasoning_summary_text.done");
        assert_eq!(rs_text_done["text"], "Let me think.");
        assert_eq!(rs_text_done["summary_index"], 0);
        assert_eq!(rs_text_done["item_id"], "rs_rs123");

        // reasoning item closed with the full text in the summary
        let rs_done = extract_event_data(&events4[2]);
        assert_eq!(rs_done["item"]["type"], "reasoning");
        assert_eq!(rs_done["item"]["status"], "completed");
        assert_eq!(rs_done["item"]["summary"][0]["text"], "Let me think.");
        assert_eq!(rs_done["output_index"], 0);

        // response.completed output array must contain the reasoning item too
        let synthetic = create_synthetic_completed_events(
            "deepseek-v4-flash",
            response_id,
            "Answer",
            &state,
            10,
            5,
        );
        let completed_event = synthetic
            .iter()
            .find(|e| {
                e.lines()
                    .next()
                    .map(|l| l == "event: response.completed")
                    .unwrap_or(false)
            })
            .unwrap();
        let completed = extract_event_data(completed_event);
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "Let me think.");
        assert_eq!(output[1]["type"], "message");
    }

    #[test]
    fn test_synthetic_completed_with_tool_calls() {
        let mut state = StreamState::default();
        let response_id = "resp_test_syn";

        // Simulate: tool call only, no finish_reason in stream
        let chunk1 = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"test","arguments":"{\"x\":1}"}}]},"finish_reason":null}]}"#;
        let _ = convert_openai_sse_to_responses(chunk1, "gpt-4", response_id, "", &mut state);

        // Stream ends without finish_reason — call synthetic completed
        let synth = create_synthetic_completed_events("gpt-4", response_id, "", &state, 10, 20);
        let synth_types = extract_event_types(&synth);
        assert_eq!(
            synth_types,
            vec![
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        // function_call_arguments.done now carries the required `name`
        let args_done = extract_event_data(&synth[0]);
        assert_eq!(args_done["type"], "response.function_call_arguments.done");
        assert_eq!(args_done["name"], "test");

        // Verify response.completed has function_call in output
        let completed_data = extract_event_data(&synth[2]);
        assert_eq!(
            completed_data["response"]["output"][0]["type"],
            "function_call"
        );
        assert_eq!(completed_data["response"]["usage"]["input_tokens"], 10);
        assert_eq!(completed_data["response"]["usage"]["output_tokens"], 20);
        // usage now carries the official details sub-objects
        assert_eq!(
            completed_data["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            0
        );
        assert_eq!(
            completed_data["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            0
        );
        assert_eq!(completed_data["response"]["usage"]["total_tokens"], 30);
    }
}
