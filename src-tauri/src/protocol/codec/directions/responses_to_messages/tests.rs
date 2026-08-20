use super::stream::MessagesResponsesStream;
use super::{decode_messages_response, encode_request};
use crate::protocol::codec::ports::StreamDecoder;
use crate::protocol::codec::report::ConversionContext;

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
