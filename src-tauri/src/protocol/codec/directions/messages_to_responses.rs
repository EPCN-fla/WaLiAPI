//! Direct Messages request -> Responses request codec.
//!
//! Request and response shapes are translated in one pass.  This is kept
//! independent from the other protocol direction so adding a field to either
//! wire format cannot accidentally reintroduce an intermediate conversion.

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

pub static MESSAGES_TO_RESPONSES_V2: MessagesToResponses = MessagesToResponses;
pub struct MessagesToResponses;
impl CodecDirection for MessagesToResponses {
    fn id(&self) -> CodecId {
        CodecId::MessagesToResponsesV2
    }
    fn downstream(&self) -> Protocol {
        Protocol::Messages
    }
    fn upstream(&self) -> Protocol {
        Protocol::Responses
    }
    fn encode_request(
        &self,
        r: &Value,
        m: &str,
    ) -> Result<(Value, ConversionContext), PrepareError> {
        encode_request(r, m).map_err(PrepareError::from)
    }
    fn new_response_decoder(
        &self,
        c: &ConversionContext,
    ) -> Box<dyn NonStreamDecoder + Send + Sync> {
        Box::new(ResponsesMessageDecoder { context: c.clone() })
    }
    fn new_stream_response_decoder(
        &self,
        c: &ConversionContext,
    ) -> Box<dyn StreamDecoder + Send + Sync> {
        Box::new(ResponsesMessagesStream::new(c))
    }
}
fn bad(k: FeatureKind, p: impl Into<String>, m: impl Into<String>) -> UnsupportedFeatures {
    UnsupportedFeatures::single(k, p, m)
}
fn required<'a>(v: &'a Value, k: &str, p: &str) -> Result<&'a str, UnsupportedFeatures> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            bad(
                FeatureKind::MissingToolField,
                format!("{p}/{k}"),
                format!("{k} is required"),
            )
        })
}

pub fn encode_request(
    request: &Value,
    mapped_model: &str,
) -> Result<(Value, ConversionContext), UnsupportedFeatures> {
    let o = request.as_object().ok_or_else(|| {
        bad(
            FeatureKind::UnsupportedField,
            "/",
            "Messages request must be an object",
        )
    })?;
    let mut normalized = Vec::new();
    for k in o.keys() {
        match k.as_str() {
            "model" | "messages" | "system" | "tools" | "tool_choice" | "thinking"
            | "output_config" | "max_tokens" | "stream" | "temperature" | "top_p"
            | "stop_sequences" => {}
            "metadata" | "container" | "context_management" | "context_management_config" => {
                normalized.push(format!("/{k}"))
            }
            other => {
                return Err(bad(
                    FeatureKind::UnsupportedField,
                    format!("/{other}"),
                    "Messages field is not representable by Responses",
                ))
            }
        }
    }
    let mut input = Vec::new();
    if let Some(system) = o.get("system") {
        let texts = system_text(system, "/system")?;
        if !texts.is_empty() {
            input.push(serde_json::json!({"type":"message","role":"developer","content":texts.into_iter().map(|text|serde_json::json!({"type":"input_text","text":text})).collect::<Vec<_>>() }));
        }
    }
    let messages = o.get("messages").and_then(Value::as_array).ok_or_else(|| {
        bad(
            FeatureKind::UnknownRole,
            "/messages",
            "Messages request requires messages array",
        )
    })?;
    for (i, msg) in messages.iter().enumerate() {
        input.extend(message_input(msg, &format!("/messages/{i}"))?)
    }
    let mut out = Map::new();
    out.insert("model".into(), Value::String(mapped_model.into()));
    out.insert("input".into(), Value::Array(input));
    out.insert(
        "max_output_tokens".into(),
        o.get("max_tokens")
            .cloned()
            .unwrap_or_else(|| Value::from(4096)),
    );
    out.insert(
        "stream".into(),
        Value::Bool(o.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    for k in ["temperature", "top_p"] {
        if let Some(v) = o.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(v) = o.get("stop_sequences") {
        out.insert("stop".into(), v.clone());
    }
    if let Some(v) = o.get("tools") {
        out.insert("tools".into(), tools(v, "/tools")?);
    }
    if let Some(v) = o.get("tool_choice") {
        let (choice, parallel) = tool_choice(v, "/tool_choice")?;
        out.insert("tool_choice".into(), choice);
        if !parallel {
            out.insert("parallel_tool_calls".into(), Value::Bool(false));
        }
    }
    if let Some(e) = thinking_effort(o) {
        out.insert("reasoning".into(), serde_json::json!({"effort":e}));
        normalized.push("/thinking".into());
    }
    let mut c = ConversionContext::new(
        format!("msg_{}", uuid::Uuid::new_v4().simple()),
        mapped_model,
        o.get("stream").and_then(Value::as_bool).unwrap_or(false),
    );
    c.normalized = normalized;
    Ok((Value::Object(out), c))
}
fn system_text(v: &Value, p: &str) -> Result<Vec<String>, UnsupportedFeatures> {
    match v {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if b.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(bad(
                        FeatureKind::UnknownBlock,
                        format!("{p}/{i}/type"),
                        "system blocks must be text",
                    ));
                }
                b.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownBlock,
                            format!("{p}/{i}/text"),
                            "system text is required",
                        )
                    })
            })
            .collect(),
        _ => Err(bad(
            FeatureKind::UnknownBlock,
            p,
            "system must be text or text-block array",
        )),
    }
}
fn message_input(msg: &Value, p: &str) -> Result<Vec<Value>, UnsupportedFeatures> {
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .filter(|r| matches!(*r, "user" | "assistant"))
        .ok_or_else(|| {
            bad(
                FeatureKind::UnknownRole,
                format!("{p}/role"),
                "role must be user or assistant",
            )
        })?;
    let parts = match msg.get("content") {
        Some(Value::String(s)) => vec![serde_json::json!({"type":"text","text":s})],
        Some(Value::Array(a)) => a.clone(),
        _ => {
            return Err(bad(
                FeatureKind::UnknownBlock,
                format!("{p}/content"),
                "content must be text or array",
            ))
        }
    };
    let mut out = Vec::new();
    for (i, b) in parts.iter().enumerate() {
        let bp = format!("{p}/content/{i}");
        match b.get("type").and_then(Value::as_str) {
            Some("text") => out.push(serde_json::json!({
                "type":"message", "role":role,
                "content":[{"type":if role=="assistant"{"output_text"}else{"input_text"},"text":b.get("text").and_then(Value::as_str).unwrap_or("")}]
            })),
            Some("image") if role == "user" => out.push(serde_json::json!({
                "type":"message", "role":role, "content":[image_input(b, &bp)?]
            })),
            Some("tool_use") if role == "assistant" => {
                let id=required(b,"id",&bp)?; let name=required(b,"name",&bp)?;
                let input=b.get("input").ok_or_else(||bad(FeatureKind::MissingToolField,format!("{bp}/input"),"tool input is required"))?;
                if !input.is_object(){return Err(bad(FeatureKind::InvalidToolArguments,format!("{bp}/input"),"tool input must be an object"))}
                out.push(serde_json::json!({"type":"function_call","call_id":id,"name":name,"arguments":serde_json::to_string(input).map_err(|_|bad(FeatureKind::InvalidToolArguments,format!("{bp}/input"),"tool input cannot be serialized"))?}));
            }
            Some("tool_result") if role == "user" => {
                let id=required(b,"tool_use_id",&bp)?;
                out.push(serde_json::json!({"type":"function_call_output","call_id":id,"output":tool_result_output(b.get("content"), &format!("{bp}/content"))?}));
            }
            Some("thinking") if role == "assistant" => {
                let text = b.get("thinking").and_then(Value::as_str).ok_or_else(|| bad(FeatureKind::UnknownBlock, format!("{bp}/thinking"), "thinking text is required"))?;
                out.push(serde_json::json!({"type":"reasoning","summary":[{"type":"summary_text","text":text}]}));
            }
            Some("redacted_thinking") => {},
            Some(x)=>return Err(bad(FeatureKind::UnknownBlock,format!("{bp}/type"),format!("content type {x:?} has no direct mapping"))),
            None=>return Err(bad(FeatureKind::UnknownBlock,format!("{bp}/type"),"content block type is required")),
        }
    }
    Ok(out)
}

/// Responses `function_call_output.output` is text-only.  Messages permits
/// structured tool-result content, so translate readable text blocks
/// explicitly and reject everything else instead of emitting an invalid
/// Responses block.
fn tool_result_output(
    content: Option<&Value>,
    pointer: &str,
) -> Result<Value, UnsupportedFeatures> {
    match content.unwrap_or(&Value::Null) {
        Value::Null => Ok(Value::String(String::new())),
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(blocks) => {
            let mut text = String::new();
            for (index, block) in blocks.iter().enumerate() {
                let p = format!("{pointer}/{index}");
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => text.push_str(
                        block.get("text").and_then(Value::as_str).ok_or_else(|| {
                            bad(
                                FeatureKind::UnknownBlock,
                                format!("{p}/text"),
                                "tool-result text is required",
                            )
                        })?,
                    ),
                    Some(other) => {
                        return Err(bad(
                            FeatureKind::UnknownBlock,
                            format!("{p}/type"),
                            format!(
                                "tool-result block {other:?} is not representable by Responses"
                            ),
                        ))
                    }
                    None => {
                        return Err(bad(
                            FeatureKind::UnknownBlock,
                            format!("{p}/type"),
                            "tool-result block type is required",
                        ))
                    }
                }
            }
            Ok(Value::String(text))
        }
        _ => Err(bad(
            FeatureKind::UnknownBlock,
            pointer,
            "tool-result content must be text or text blocks",
        )),
    }
}
fn image_input(v: &Value, p: &str) -> Result<Value, UnsupportedFeatures> {
    let source = v.get("source").ok_or_else(|| {
        bad(
            FeatureKind::Media,
            format!("{p}/source"),
            "image source is required",
        )
    })?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = required(source, "media_type", &format!("{p}/source"))?;
            let data = required(source, "data", &format!("{p}/source"))?;
            if data.len() > super::super::request::MAX_IMAGE_BYTES {
                return Err(bad(
                    FeatureKind::Media,
                    format!("{p}/source/data"),
                    "image exceeds supported maximum",
                ));
            }
            Ok(
                serde_json::json!({"type":"input_image","image_url":format!("data:{media};base64,{data}")}),
            )
        }
        Some("url") => {
            let url = required(source, "url", &format!("{p}/source"))?;
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(bad(
                    FeatureKind::Media,
                    format!("{p}/source/url"),
                    "image URL must be http(s)",
                ));
            }
            Ok(serde_json::json!({"type":"input_image","image_url":url}))
        }
        _ => Err(bad(
            FeatureKind::Media,
            format!("{p}/source/type"),
            "unsupported image source",
        )),
    }
}
fn tools(v: &Value, p: &str) -> Result<Value, UnsupportedFeatures> {
    v.as_array()
        .ok_or_else(|| bad(FeatureKind::UnsupportedField, p, "tools must be an array"))?
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let q = format!("{p}/{i}");
            if matches!(t.get("type").and_then(Value::as_str),Some(x)if x!="custom") {
                return Err(bad(
                    FeatureKind::BuiltinTool,
                    format!("{q}/type"),
                    "built-in tool has no direct mapping",
                ));
            }
            let name = required(t, "name", &q)?;
            let schema = t.get("input_schema").ok_or_else(|| {
                bad(
                    FeatureKind::InvalidToolArguments,
                    format!("{q}/input_schema"),
                    "input_schema is required",
                )
            })?;
            if !schema.is_object() {
                return Err(bad(
                    FeatureKind::InvalidToolArguments,
                    format!("{q}/input_schema"),
                    "input_schema must be an object",
                ));
            }
            let mut o = serde_json::json!({"type":"function","name":name,"parameters":schema});
            if let Some(d) = t.get("description") {
                o["description"] = d.clone();
            }
            Ok(o)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}
fn tool_choice(v: &Value, p: &str) -> Result<(Value, bool), UnsupportedFeatures> {
    let o = v.as_object().ok_or_else(|| {
        bad(
            FeatureKind::UnsupportedField,
            p,
            "tool_choice must be an object",
        )
    })?;
    let disable = o
        .get("disable_parallel_tool_use")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let r = match o.get("type").and_then(Value::as_str) {
        Some("auto") => Value::String("auto".into()),
        Some("any") => Value::String("required".into()),
        Some("none") => Value::String("none".into()),
        Some("tool") => serde_json::json!({"type":"function","name":required(v,"name",p)?}),
        _ => {
            return Err(bad(
                FeatureKind::UnsupportedField,
                format!("{p}/type"),
                "unsupported tool_choice",
            ))
        }
    };
    Ok((r, !disable))
}
fn thinking_effort(o: &Map<String, Value>) -> Option<&'static str> {
    let budget = o
        .get("thinking")
        .and_then(|thinking| thinking.get("budget_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| {
            o.get("output_config")
                .and_then(|config| config.get("effort"))
                .and_then(Value::as_str)
                .map(|e| match e {
                    "low" => 1024,
                    "medium" => 8192,
                    "high" => 24576,
                    _ => 32768,
                })
        })?;
    crate::protocol::thinking::budget_to_level(budget)
}

struct ResponsesMessageDecoder {
    context: ConversionContext,
}
impl NonStreamDecoder for ResponsesMessageDecoder {
    fn decode(&self, b: &Value) -> Result<DecodedResponse, DecodeError> {
        decode_response(b, &self.context).map_err(DecodeError::from)
    }
}
pub fn decode_response(
    body: &Value,
    c: &ConversionContext,
) -> Result<DecodedResponse, UnsupportedFeatures> {
    let r = body
        .get("response")
        .filter(|v| v.is_object())
        .unwrap_or(body);
    let output = r.get("output").and_then(Value::as_array).ok_or_else(|| {
        bad(
            FeatureKind::UnknownEvent,
            "/output",
            "Responses body requires output array",
        )
    })?;
    let mut content = Vec::new();
    for (i, item) in output.iter().enumerate() {
        let p = format!("/output/{i}");
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownBlock,
                            format!("{p}/content"),
                            "message content is required",
                        )
                    })?
                {
                    match part.get("type").and_then(Value::as_str){Some("output_text")|Some("text")=>content.push(serde_json::json!({"type":"text","text":part.get("text").and_then(Value::as_str).unwrap_or("")})),Some(x)=>return Err(bad(FeatureKind::UnknownBlock,format!("{p}/content/type"),format!("response content {x:?} is unsupported"))),None=>return Err(bad(FeatureKind::UnknownBlock,format!("{p}/content/type"),"content type is required"))}
                }
            }
            Some("reasoning") => {
                let text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.get("text").and_then(Value::as_str))
                    .collect::<String>();
                if !text.is_empty() {
                    content.push(serde_json::json!({"type":"thinking","thinking":text}));
                }
            }
            Some("function_call") => {
                let id = required(item, "call_id", &p)?;
                let name = required(item, "name", &p)?;
                let args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::InvalidToolArguments,
                            format!("{p}/arguments"),
                            "arguments are required",
                        )
                    })?;
                let input: Value = serde_json::from_str(args).map_err(|_| {
                    bad(
                        FeatureKind::InvalidToolArguments,
                        format!("{p}/arguments"),
                        "arguments must be valid JSON",
                    )
                })?;
                if !input.is_object() {
                    return Err(bad(
                        FeatureKind::InvalidToolArguments,
                        format!("{p}/arguments"),
                        "arguments must be an object",
                    ));
                }
                content
                    .push(serde_json::json!({"type":"tool_use","id":id,"name":name,"input":input}));
            }
            Some(x) => {
                return Err(bad(
                    FeatureKind::UnknownEvent,
                    format!("{p}/type"),
                    format!("response item {x:?} is unsupported"),
                ))
            }
            None => {
                return Err(bad(
                    FeatureKind::UnknownEvent,
                    format!("{p}/type"),
                    "response item type is required",
                ))
            }
        }
    }
    if content.is_empty() {
        content.push(serde_json::json!({"type":"text","text":""}));
    }
    let usage = usage(r);
    let (status, stop) = match r.get("status").and_then(Value::as_str) {
        Some("completed") | None => (
            "message",
            if content
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
            {
                "tool_use"
            } else {
                "end_turn"
            },
        ),
        Some("incomplete") => (
            "message",
            match r
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            {
                Some("max_output_tokens") | Some("max_tokens") | None => "max_tokens",
                Some("content_filter") | Some("safety") => "refusal",
                Some(x) => {
                    return Err(bad(
                        FeatureKind::UnknownFinishReason,
                        "/incomplete_details/reason",
                        format!("unknown incomplete reason {x:?}"),
                    ))
                }
            },
        ),
        Some("failed") => {
            return Err(bad(
                FeatureKind::UnknownEvent,
                "/status",
                "Responses response failed",
            ))
        }
        Some(x) => {
            return Err(bad(
                FeatureKind::UnknownEvent,
                "/status",
                format!("unknown status {x:?}"),
            ))
        }
    };
    Ok(DecodedResponse {
        body: serde_json::json!({"id":r.get("id").and_then(Value::as_str).unwrap_or(&c.request_id),"type":status,"role":"assistant","model":r.get("model").and_then(Value::as_str).unwrap_or(&c.upstream_model),"content":content,"stop_reason":stop,"stop_sequence":null,"usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_creation_input_tokens":usage.cache_creation_input_tokens,"cache_read_input_tokens":usage.cache_read_input_tokens}}),
        usage: Some(usage),
    })
}
fn usage(r: &Value) -> Usage {
    let i = r.pointer("/usage/input_tokens").and_then(Value::as_u64);
    let o = r.pointer("/usage/output_tokens").and_then(Value::as_u64);
    Usage {
        input_tokens: i.unwrap_or(0),
        output_tokens: o.unwrap_or(0),
        cache_creation_input_tokens: r
            .pointer("/usage/cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_input_tokens: r
            .pointer("/usage/cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        usage_unknown: i.is_none() || o.is_none(),
    }
}

struct ResponsesMessagesStream {
    pending: Vec<u8>,
    id: String,
    model: String,
    started: bool,
    terminal: bool,
    usage: Usage,
    blocks: BTreeMap<u64, StreamBlock>,
}
#[derive(Default)]
struct StreamBlock {
    kind: String,
    id: String,
    name: String,
    text: String,
    args: String,
    closed: bool,
}
impl ResponsesMessagesStream {
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
        }
    }
    fn frame(t: &str, v: Value) -> String {
        sse::event(t, v)
    }
    fn start(&mut self, out: &mut Vec<String>) {
        if !self.started {
            self.started = true;
            out.push(Self::frame("message_start",serde_json::json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],"usage":{"input_tokens":0,"output_tokens":0}}})))
        }
    }
}
impl StreamDecoder for ResponsesMessagesStream {
    fn feed(&mut self, b: &[u8]) -> Result<Vec<String>, DecodeError> {
        self.pending.extend_from_slice(b);
        let mut out = Vec::new();
        while let Some(end) = sse::record_end(&self.pending) {
            let r: Vec<u8> = self.pending.drain(..end).collect();
            out.extend(self.record(&r).map_err(DecodeError::from)?)
        }
        Ok(out)
    }
    fn finish(&mut self) -> Result<Vec<String>, DecodeError> {
        if !self.pending.is_empty() {
            return Err(DecodeError::from(bad(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE ended mid-record",
            )));
        }
        if !self.terminal {
            return Err(DecodeError::from(bad(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE ended without response.completed",
            )));
        }
        Ok(Vec::new())
    }
    fn usage(&self) -> Option<Usage> {
        Some(self.usage)
    }
}
impl ResponsesMessagesStream {
    fn record(&mut self, r: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        let p = sse::parse_data_payload(r)?;
        if p.is_empty() || p == "[DONE]" {
            return Ok(Vec::new());
        }
        let e: Value = serde_json::from_str(&p).map_err(|_| {
            bad(
                FeatureKind::UnknownEvent,
                "/",
                "Responses SSE data is not JSON",
            )
        })?;
        if e.get("type").and_then(Value::as_str) == Some("codex.rate_limits") {
            return Ok(vec![String::from_utf8_lossy(r).into_owned()]);
        }
        let ty = e.get("type").and_then(Value::as_str).ok_or_else(|| {
            bad(
                FeatureKind::UnknownEvent,
                "/type",
                "Responses SSE type is required",
            )
        })?;
        let mut out = Vec::new();
        match ty {
            "response.created" | "response.in_progress" => {
                if let Some(x) = e.get("response") {
                    self.id = x
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(&self.id)
                        .into();
                    self.model = x
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or(&self.model)
                        .into()
                }
                self.start(&mut out)
            }
            "response.output_item.added" => {
                self.start(&mut out);
                let ix = e.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                let item = e.get("item").unwrap_or(&Value::Null);
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                let mut b = StreamBlock {
                    kind: kind.into(),
                    ..Default::default()
                };
                if kind == "function_call" {
                    b.id = required(item, "call_id", "/item")?.into();
                    b.name = required(item, "name", "/item")?.into();
                }
                self.blocks.insert(ix, b);
                let block = match kind {
                    "message" => serde_json::json!({"type":"text","text":""}),
                    "reasoning" => serde_json::json!({"type":"thinking","thinking":""}),
                    "function_call" => {
                        serde_json::json!({"type":"tool_use","id":self.blocks[&ix].id,"name":self.blocks[&ix].name,"input":{}})
                    }
                    _ => {
                        return Err(bad(
                            FeatureKind::UnknownEvent,
                            "/item/type",
                            "unsupported Responses output item",
                        ))
                    }
                };
                out.push(Self::frame("content_block_start",serde_json::json!({"type":"content_block_start","index":ix,"content_block":block})))
            }
            "response.output_text.delta" => {
                self.start(&mut out);
                let ix = e.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                let delta = e.get("delta").and_then(Value::as_str).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/delta",
                        "output text delta is required",
                    )
                })?;
                let block = self.blocks.get_mut(&ix).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "text delta before message item",
                    )
                })?;
                if block.kind != "message" {
                    return Err(bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "output text delta targets a non-message item",
                    ));
                }
                block.text.push_str(delta);
                out.push(Self::frame("content_block_delta",serde_json::json!({"type":"content_block_delta","index":ix,"delta":{"type":"text_delta","text":delta}})))
            }
            "response.reasoning_summary_text.delta" => {
                self.start(&mut out);
                let ix = e.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                let delta = e.get("delta").and_then(Value::as_str).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/delta",
                        "reasoning summary delta is required",
                    )
                })?;
                let block = self.blocks.get_mut(&ix).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "reasoning delta before reasoning item",
                    )
                })?;
                if block.kind != "reasoning" {
                    return Err(bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "reasoning delta targets a non-reasoning item",
                    ));
                }
                block.text.push_str(delta);
                out.push(Self::frame("content_block_delta",serde_json::json!({"type":"content_block_delta","index":ix,"delta":{"type":"thinking_delta","thinking":delta}})))
            }
            // These are standard Responses lifecycle records.  Deltas carry
            // the incremental payload, while the `*.done` records verify (and
            // backfill for compatible backends) the final complete text.
            "response.content_part.added" => {
                self.validate_part_lifecycle(&e, "message", "output_text", "content_index")?;
            }
            "response.content_part.done" => {
                let ix =
                    self.validate_part_lifecycle(&e, "message", "output_text", "content_index")?;
                let text = e
                    .pointer("/part/text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownEvent,
                            "/part/text",
                            "completed output text is required",
                        )
                    })?;
                self.emit_completed_text(ix, "message", text, false, &mut out)?;
            }
            "response.output_text.done" => {
                let ix = e
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownEvent,
                            "/output_index",
                            "output text completion requires output_index",
                        )
                    })?;
                let text = e.get("text").and_then(Value::as_str).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/text",
                        "completed output text is required",
                    )
                })?;
                self.emit_completed_text(ix, "message", text, false, &mut out)?;
            }
            "response.reasoning_summary_part.added" => {
                self.validate_part_lifecycle(
                    &e,
                    "reasoning",
                    "reasoning_summary_text",
                    "summary_index",
                )?;
            }
            "response.reasoning_summary_part.done" => {
                let ix = self.validate_part_lifecycle(
                    &e,
                    "reasoning",
                    "reasoning_summary_text",
                    "summary_index",
                )?;
                let text = e
                    .pointer("/part/text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownEvent,
                            "/part/text",
                            "completed reasoning text is required",
                        )
                    })?;
                self.emit_completed_text(ix, "reasoning", text, true, &mut out)?;
            }
            "response.reasoning_summary_text.done" => {
                let ix = e
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownEvent,
                            "/output_index",
                            "reasoning completion requires output_index",
                        )
                    })?;
                let text = e.get("text").and_then(Value::as_str).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/text",
                        "completed reasoning text is required",
                    )
                })?;
                self.emit_completed_text(ix, "reasoning", text, true, &mut out)?;
            }
            "response.function_call_arguments.delta" => {
                let ix = e.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                let delta = e.get("delta").and_then(Value::as_str).unwrap_or("");
                let b = self.blocks.get_mut(&ix).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "argument delta before function item",
                    )
                })?;
                b.args.push_str(delta);
                out.push(Self::frame("content_block_delta",serde_json::json!({"type":"content_block_delta","index":ix,"delta":{"type":"input_json_delta","partial_json":delta}})))
            }
            "response.function_call_arguments.done" => {
                let ix = e.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                let a = e.get("arguments").and_then(Value::as_str).ok_or_else(|| {
                    bad(
                        FeatureKind::InvalidToolArguments,
                        "/arguments",
                        "arguments are required",
                    )
                })?;
                let p: Value = serde_json::from_str(a).map_err(|_| {
                    bad(
                        FeatureKind::InvalidToolArguments,
                        "/arguments",
                        "arguments must be valid JSON",
                    )
                })?;
                if !p.is_object() {
                    return Err(bad(
                        FeatureKind::InvalidToolArguments,
                        "/arguments",
                        "arguments must be an object",
                    ));
                }
                if let Some(b) = self.blocks.get_mut(&ix) {
                    b.args = a.into();
                }
            }
            "response.output_item.done" => {
                let ix = e.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                // Some compatible Responses backends emit only the terminal
                // item record.  It is still a complete source item, so create
                // the corresponding target block instead of treating this as
                // a malformed ordering.  This is not an invented tool call:
                // id, name and validated arguments all come from `item`.
                if !self.blocks.contains_key(&ix) {
                    self.start(&mut out);
                    let item = e.get("item").unwrap_or(&Value::Null);
                    let kind = item.get("type").and_then(Value::as_str).ok_or_else(|| {
                        bad(
                            FeatureKind::UnknownEvent,
                            "/item/type",
                            "completed item type is required",
                        )
                    })?;
                    let mut synthesized = StreamBlock {
                        kind: kind.into(),
                        ..Default::default()
                    };
                    let content_block = match kind {
                        "message" => serde_json::json!({"type":"text","text":""}),
                        "reasoning" => serde_json::json!({"type":"thinking","thinking":""}),
                        "function_call" => {
                            synthesized.id = required(item, "call_id", "/item")?.into();
                            synthesized.name = required(item, "name", "/item")?.into();
                            let arguments = item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    bad(
                                        FeatureKind::InvalidToolArguments,
                                        "/item/arguments",
                                        "function arguments are required",
                                    )
                                })?;
                            let parsed: Value = serde_json::from_str(arguments).map_err(|_| {
                                bad(
                                    FeatureKind::InvalidToolArguments,
                                    "/item/arguments",
                                    "function arguments must be valid JSON",
                                )
                            })?;
                            if !parsed.is_object() {
                                return Err(bad(
                                    FeatureKind::InvalidToolArguments,
                                    "/item/arguments",
                                    "function arguments must be an object",
                                ));
                            }
                            synthesized.args = arguments.into();
                            serde_json::json!({"type":"tool_use","id":synthesized.id,"name":synthesized.name,"input":{}})
                        }
                        _ => {
                            return Err(bad(
                                FeatureKind::UnknownEvent,
                                "/item/type",
                                "unsupported completed output item",
                            ))
                        }
                    };
                    self.blocks.insert(ix, synthesized);
                    out.push(Self::frame("content_block_start", serde_json::json!({"type":"content_block_start","index":ix,"content_block":content_block})));
                    if kind == "function_call" {
                        let arguments = self.blocks[&ix].args.clone();
                        out.push(Self::frame("content_block_delta", serde_json::json!({"type":"content_block_delta","index":ix,"delta":{"type":"input_json_delta","partial_json":arguments}})));
                    }
                }
                let b = self.blocks.get_mut(&ix).ok_or_else(|| {
                    bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "item done without item start",
                    )
                })?;
                if b.closed {
                    return Err(bad(
                        FeatureKind::UnknownEvent,
                        "/output_index",
                        "duplicate output item completion",
                    ));
                }
                if b.kind == "function_call" && !b.args.is_empty() {
                    let p: Value = serde_json::from_str(&b.args).map_err(|_| {
                        bad(
                            FeatureKind::InvalidToolArguments,
                            "/arguments",
                            "arguments must be valid JSON",
                        )
                    })?;
                    if !p.is_object() {
                        return Err(bad(
                            FeatureKind::InvalidToolArguments,
                            "/arguments",
                            "arguments must be object",
                        ));
                    }
                }
                b.closed = true;
                out.push(Self::frame(
                    "content_block_stop",
                    serde_json::json!({"type":"content_block_stop","index":ix}),
                ))
            }
            "response.completed" => {
                if self.terminal {
                    return Err(bad(
                        FeatureKind::UnknownEvent,
                        "/type",
                        "duplicate response.completed",
                    ));
                }
                if self.blocks.values().any(|b| !b.closed) {
                    return Err(bad(
                        FeatureKind::UnknownEvent,
                        "/output",
                        "response completed with open content block",
                    ));
                }
                let response = e.get("response").unwrap_or(&e);
                self.usage = usage(response);
                self.start(&mut out);
                let stop = match response.get("status").and_then(Value::as_str) {
                    Some("incomplete") => "max_tokens",
                    Some("completed") | None => {
                        if self.blocks.values().any(|b| b.kind == "function_call") {
                            "tool_use"
                        } else {
                            "end_turn"
                        }
                    }
                    Some(x) => {
                        return Err(bad(
                            FeatureKind::UnknownFinishReason,
                            "/status",
                            format!("unsupported final status {x:?}"),
                        ))
                    }
                };
                out.push(Self::frame("message_delta",serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"input_tokens":self.usage.input_tokens,"output_tokens":self.usage.output_tokens}})));
                out.push(Self::frame(
                    "message_stop",
                    serde_json::json!({"type":"message_stop"}),
                ));
                self.terminal = true
            }
            "response.failed" | "response.incomplete" => {
                return Err(bad(
                    FeatureKind::UnknownEvent,
                    "/type",
                    "Responses upstream reported failure",
                ))
            }
            _ => {
                return Err(bad(
                    FeatureKind::UnknownEvent,
                    "/type",
                    format!("unknown Responses SSE event {ty:?}"),
                ))
            }
        }
        Ok(out)
    }
}

impl ResponsesMessagesStream {
    fn validate_part_lifecycle(
        &self,
        event: &Value,
        expected_kind: &str,
        expected_part_type: &str,
        part_index_field: &str,
    ) -> Result<u64, UnsupportedFeatures> {
        let ix = event
            .get("output_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                bad(
                    FeatureKind::UnknownEvent,
                    "/output_index",
                    "part lifecycle requires output_index",
                )
            })?;
        if event.get(part_index_field).and_then(Value::as_u64) != Some(0) {
            return Err(bad(
                FeatureKind::UnknownEvent,
                format!("/{part_index_field}"),
                "only the first textual part is representable by this stream codec",
            ));
        }
        let block = self.blocks.get(&ix).ok_or_else(|| {
            bad(
                FeatureKind::UnknownEvent,
                "/output_index",
                "part lifecycle before output item",
            )
        })?;
        if block.kind != expected_kind {
            return Err(bad(
                FeatureKind::UnknownEvent,
                "/output_index",
                "part lifecycle targets the wrong output item type",
            ));
        }
        let part = event.get("part").ok_or_else(|| {
            bad(
                FeatureKind::UnknownEvent,
                "/part",
                "part lifecycle requires part",
            )
        })?;
        if part.get("type").and_then(Value::as_str) != Some(expected_part_type) {
            return Err(bad(
                FeatureKind::UnknownEvent,
                "/part/type",
                "unexpected Responses part type",
            ));
        }
        Ok(ix)
    }

    fn emit_completed_text(
        &mut self,
        ix: u64,
        expected_kind: &str,
        complete: &str,
        thinking: bool,
        out: &mut Vec<String>,
    ) -> Result<(), UnsupportedFeatures> {
        let block = self.blocks.get_mut(&ix).ok_or_else(|| {
            bad(
                FeatureKind::UnknownEvent,
                "/output_index",
                "text completion before output item",
            )
        })?;
        if block.kind != expected_kind {
            return Err(bad(
                FeatureKind::UnknownEvent,
                "/output_index",
                "text completion targets the wrong output item type",
            ));
        }
        let suffix = complete.strip_prefix(&block.text).ok_or_else(|| {
            bad(
                FeatureKind::UnknownEvent,
                "/text",
                "completed text conflicts with prior deltas",
            )
        })?;
        if !suffix.is_empty() {
            block.text.push_str(suffix);
            out.push(Self::frame("content_block_delta", serde_json::json!({
                "type":"content_block_delta", "index":ix,
                "delta": if thinking { serde_json::json!({"type":"thinking_delta", "thinking":suffix}) } else { serde_json::json!({"type":"text_delta", "text":suffix}) }
            })));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_preserves_tool_result_id() {
        let(out,_)=encode_request(&serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"ok"}]}]}),"m").unwrap();
        assert_eq!(out["input"][0]["call_id"], "call_1");
    }

    #[test]
    fn request_preserves_interleaved_message_content_order_and_rejects_non_text_tool_output() {
        let (out, _) = encode_request(
            &serde_json::json!({"messages":[{
                "role":"assistant", "content":[
                    {"type":"text", "text":"before"},
                    {"type":"tool_use", "id":"call_1", "name":"lookup", "input":{}},
                    {"type":"text", "text":"after"}
                ]
            }]}),
            "m",
        )
        .unwrap();
        assert_eq!(
            out["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["message", "function_call", "message"]
        );

        let err = encode_request(&serde_json::json!({"messages":[{
            "role":"user", "content":[{"type":"tool_result", "tool_use_id":"call_1", "content":[{"type":"image"}]}]
        }]}), "m").unwrap_err();
        assert!(err
            .features
            .iter()
            .any(|feature| feature == "unsupported_feature.unknown_block"));
    }
    #[test]
    fn response_preserves_function_id() {
        let c = ConversionContext::new("x", "m", false);
        let d=decode_response(&serde_json::json!({"status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"weather","arguments":"{}"}],"usage":{"input_tokens":1,"output_tokens":2}}),&c).unwrap();
        assert_eq!(d.body["content"][0]["id"], "call_1");
    }

    #[test]
    fn stream_is_split_invariant_and_closes_message_once() {
        let source = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"你好\"}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n"
        );
        let context = ConversionContext::new("msg_1", "m", true);
        let mut expected = None;
        for split in 0..=source.len() {
            let mut decoder = ResponsesMessagesStream::new(&context);
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
                .filter(|event| event.contains("message_stop"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.contains("message_start"))
                .count(),
            1
        );
    }

    #[test]
    fn stream_accepts_complete_responses_text_reasoning_and_tool_lifecycles() {
        let source = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"model\":\"m\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.content_part.added\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"text\":\"hello\"}\n\n",
            "data: {\"type\":\"response.content_part.done\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"hello\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"reasoning\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"output_index\":1,\"summary_index\":0,\"part\":{\"type\":\"reasoning_summary_text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":1,\"delta\":\"think\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.done\",\"output_index\":1,\"text\":\"think\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\",\"output_index\":1,\"summary_index\":0,\"part\":{\"type\":\"reasoning_summary_text\",\"text\":\"think\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"reasoning\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"{}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":2,\"arguments\":\"{}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"function_call\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n"
        );
        let context = ConversionContext::new("r", "m", true);
        let mut expected = None;
        for split in 0..=source.len() {
            let mut decoder = ResponsesMessagesStream::new(&context);
            let mut events = decoder.feed(&source.as_bytes()[..split]).unwrap();
            events.extend(decoder.feed(&source.as_bytes()[split..]).unwrap());
            events.extend(decoder.finish().unwrap());
            assert_eq!(decoder.usage().unwrap().output_tokens, 1);
            if let Some(previous) = &expected {
                assert_eq!(&events, previous);
            } else {
                expected = Some(events);
            }
        }
        let output = expected.unwrap().join("");
        assert!(output.contains("hello") && output.contains("think") && output.contains("call_1"));
    }
}
