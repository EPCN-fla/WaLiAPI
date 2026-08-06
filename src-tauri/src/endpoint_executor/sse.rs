//! SSE framing + streaming commit-barrier pump (T06).
//!
//! [`StreamPumpCore`] is the pure, testable engine behind the streaming plan
//! driver.  It owns a [`StreamSupervisor`] and drives the commit barrier:
//!
//! ```text
//! FirstFrameBufferedAndValidated → commit_downstream → begin_streaming →
//!   (per-frame) → complete | abort
//! ```
//!
//! Native streams pass raw bytes through (validating the first complete record
//! before commit); conversion streams run the correct versioned codec decoder
//! so the downstream only ever sees its own protocol.  The pump performs no
//! network I/O — the driver feeds it bytes.

use crate::core::stream_supervisor::{StreamSupervisor, StreamTransitionError};
use crate::protocol::codec::registry::StreamDecoder;
use serde_json::Value;

/// Which transform a streaming attempt needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseMode {
    /// Raw byte passthrough (OpenAI Chat SSE / OpenAI Responses SSE /
    /// Anthropic Messages SSE).  First-frame validation = one well-formed SSE
    /// record; terminal markers `[DONE]` / `message_stop` are exactly-once.
    Native,
    /// Chat SSE -> Messages SSE (downstream Messages, upstream Chat).
    ChatToMessages,
    /// Messages SSE -> Chat SSE (downstream Chat, upstream Messages).
    MessagesToChat,
    /// Chat SSE -> Responses SSE (downstream Responses, upstream Chat via the
    /// per-record `responses_via_chat_v1` legacy debt).
    ResponsesViaChat,
}

impl SseMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SseMode::Native => "native",
            SseMode::ChatToMessages => "chat_to_messages_v1",
            SseMode::MessagesToChat => "messages_to_chat_v1",
            SseMode::ResponsesViaChat => "responses_via_chat_v1",
        }
    }
}

/// A pump failure.  Before commit these allow upstream swap; after commit they
/// become a protocol-representable downstream error (never a retry).
#[derive(Debug, Clone)]
pub enum PumpError {
    /// Codec/upstream protocol error (undecodable frame, invalid SSE, etc.).
    Protocol(String),
    /// The supervisor rejected a transition (e.g. terminal already emitted).
    Supervisor(String),
}

impl From<StreamTransitionError> for PumpError {
    fn from(e: StreamTransitionError) -> Self {
        PumpError::Supervisor(format!("{e:?}"))
    }
}

impl PumpError {
    pub fn message(&self) -> &str {
        match self {
            PumpError::Protocol(m) | PumpError::Supervisor(m) => m,
        }
    }
}

/// Locate the terminating sequence of the next full SSE record (shared with the
/// codec's framing helpers so native validation matches conversion validation).
pub fn record_end(input: &[u8]) -> Option<usize> {
    crate::protocol::codec::sse::record_end(input)
}

/// Parse one raw SSE record into its `data:` payload.
pub fn parse_data_payload(record: &[u8]) -> Result<String, String> {
    crate::protocol::codec::sse::parse_data_payload(record).map_err(|e| e.message)
}

fn sse_record_text(record: &[u8]) -> String {
    String::from_utf8_lossy(record).into_owned()
}

/// Validate a first native SSE record (pre-commit).  A record is valid when it
/// has a `data:` payload (or an `event:` field) and, if the payload is JSON, it
/// parses.  `[DONE]` is valid too.
pub fn validate_native_first_record(record: &[u8]) -> Result<(), String> {
    let text = sse_record_text(record);
    if text.contains("event:") {
        return Ok(()); // Anthropic-style event record
    }
    let payload = parse_data_payload(record)?;
    if payload.is_empty() || payload == "[DONE]" {
        return Ok(());
    }
    serde_json::from_str::<Value>(&payload)
        .map(|_| ())
        .map_err(|e| format!("first SSE data frame is not valid JSON: {e}"))
}

/// Extract the last token-usage triple observed in native OpenAI SSE chunks.
/// Mirrors the legacy `parse_usage_from_chunk` behaviour (best effort).
pub fn scan_usage_from_chunk(text: &str) -> Option<(i64, i64, i64)> {
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
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let completion = usage
                    .get("completion_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let total = usage
                    .get("total_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if total > 0 || prompt > 0 || completion > 0 {
                    return Some((prompt, completion, total));
                }
            }
        }
    }
    None
}

/// Pure streaming pump: drives the supervisor + decoder as bytes arrive.
pub struct StreamPumpCore {
    supervisor: StreamSupervisor,
    mode: SseMode,
    decoder: Option<Box<dyn StreamDecoder>>,
    /// Bytes to emit on the first poll (raw first record, or the first encoded
    /// downstream events for conversion modes).
    first_frame: Vec<u8>,
    first_done: bool,
    terminal_registered: bool,
    finished: bool,
    /// Mapped upstream model (for responses/chat conversion synthesis).
    model: String,
    /// Responses-via-chat streaming state (responses::StreamState).
    responses_state: Option<crate::protocol::responses::StreamState>,
    responses_id: String,
    accumulated_content: String,
    /// Last native usage observed.
    usage: Option<(i64, i64, i64)>,
}

impl StreamPumpCore {
    /// Build a pump.  `supervisor` must already be in
    /// `FirstFrameBufferedAndValidated`.
    ///
    /// `first_frame` is the first complete upstream SSE record (already
    /// validated by the driver).  `carry` is the RAW remainder of the same
    /// upstream chunk that arrived after the first record — records 2..N.
    ///
    /// For CONVERSION modes BOTH `first_frame` and `carry` are fed through the
    /// codec decoder BEFORE commit (decision 6: the first downstream event must
    /// be successfully encoded before the 200 is committed), so `start()`
    /// returns only ENCODED downstream bytes — NEVER raw upstream protocol
    /// bytes, even when the first TCP chunk spans several records.  The decoder
    /// consumes records 1..N in order; its state is NOT left at record N+1.
    /// For native passthrough, `first_frame` + `carry` are preserved raw.
    ///
    /// Returns `Err` on a codec rejection of any first-chunk record.  This is a
    /// pre-commit failover: the downstream 200 was NOT committed and the next
    /// candidate may be tried.
    pub fn new(
        supervisor: StreamSupervisor,
        mode: SseMode,
        decoder: Option<Box<dyn StreamDecoder>>,
        first_frame: Vec<u8>,
        carry: Vec<u8>,
        model: String,
    ) -> Result<Self, PumpError> {
        let mut core = Self {
            supervisor,
            mode,
            decoder,
            first_frame: Vec::new(),
            first_done: false,
            terminal_registered: false,
            finished: false,
            model,
            responses_state: if mode == SseMode::ResponsesViaChat {
                Some(crate::protocol::responses::StreamState::default())
            } else {
                None
            },
            responses_id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            accumulated_content: String::new(),
            usage: None,
        };
        match mode {
            SseMode::Native => {
                let mut buf = first_frame;
                buf.extend_from_slice(&carry);
                // T10 integration fix: a short SSE upstream can deliver the WHOLE
                // stream in the first TCP burst, so records 2..N land in `carry`
                // and are never pushed through `encode_native_chunk`.  Scan usage
                // and terminal markers from the combined first-chunk bytes so a
                // first-burst stream still records token usage and terminates
                // exactly once.
                let text = String::from_utf8_lossy(&buf);
                if let Some(u) = scan_usage_from_chunk(&text) {
                    core.usage = Some(u);
                }
                if !core.terminal_registered
                    && (text.contains("data: [DONE]") || text.contains("message_stop"))
                {
                    core.terminal_registered = core.supervisor.register_terminal();
                }
                core.first_frame = buf;
            }
            SseMode::ChatToMessages | SseMode::MessagesToChat => {
                let mut out = Vec::new();
                if !first_frame.is_empty() {
                    out.extend(core.encode_conversion_chunk(&first_frame)?);
                }
                if !carry.is_empty() {
                    out.extend(core.encode_conversion_chunk(&carry)?);
                }
                core.first_frame = out;
            }
            SseMode::ResponsesViaChat => {
                let mut out = Vec::new();
                if !first_frame.is_empty() {
                    out.extend(
                        core.encode_responses_chunk(&String::from_utf8_lossy(&first_frame))?,
                    );
                }
                if !carry.is_empty() {
                    out.extend(core.encode_responses_chunk(&String::from_utf8_lossy(&carry))?);
                }
                core.first_frame = out;
            }
        }
        Ok(core)
    }

    /// Commit the downstream 200 + first frame.  Idempotent.  For conversion
    /// modes the first frame is already the ENCODED downstream events (from
    /// [`StreamPumpCore::new`]); for native it is the raw validated record.
    pub fn start(&mut self) -> Result<Vec<u8>, PumpError> {
        if self.first_done {
            return Ok(Vec::new());
        }
        self.supervisor.commit_downstream()?;
        self.supervisor.begin_streaming()?;
        self.first_done = true;
        Ok(std::mem::take(&mut self.first_frame))
    }

    /// Encode one native (passthrough) chunk: scan usage / terminal markers
    /// and return the raw bytes.
    fn encode_native_chunk(&mut self, bytes: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(bytes);
        if let Some(u) = scan_usage_from_chunk(&text) {
            self.usage = Some(u);
        }
        if !self.terminal_registered
            && (text.contains("data: [DONE]") || text.contains("message_stop"))
        {
            self.terminal_registered = self.supervisor.register_terminal();
        }
        bytes.to_vec()
    }

    /// Encode one ChatToMessages / MessagesToChat chunk through the decoder.
    fn encode_conversion_chunk(&mut self, bytes: &[u8]) -> Result<Vec<u8>, PumpError> {
        let decoder = self.decoder.as_mut().ok_or_else(|| {
            PumpError::Protocol("conversion stream is missing its decoder".to_string())
        })?;
        let events = decoder.feed(bytes).map_err(|e| {
            PumpError::Protocol(format!(
                "upstream stream could not be converted ({}): {}",
                self.mode.as_str(),
                e.message
            ))
        })?;
        let mut out = Vec::new();
        for ev in events {
            if ev.contains("message_stop") {
                self.terminal_registered = self.supervisor.register_terminal();
            }
            // The codec event strings already end in "\n\n" (sse::event /
            // sse::data_frame); do NOT append another newline.
            out.extend_from_slice(ev.as_bytes());
        }
        Ok(out)
    }

    /// Encode one ResponsesViaChat chunk through the Responses SSE converter.
    fn encode_responses_chunk(&mut self, text: &str) -> Result<Vec<u8>, PumpError> {
        if let Some(u) = scan_usage_from_chunk(text) {
            self.usage = Some(u);
        }
        // accumulate content for the synthetic completed events
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
                if let Some(choices) = json.get("choices").and_then(Value::as_array) {
                    if let Some(choice) = choices.first() {
                        if let Some(delta) = choice.get("delta") {
                            if let Some(c) = delta.get("content").and_then(Value::as_str) {
                                self.accumulated_content.push_str(c);
                            }
                        }
                    }
                }
            }
        }
        let events = crate::protocol::responses::convert_openai_sse_to_responses(
            text,
            &self.model,
            &self.responses_id,
            &self.accumulated_content,
            self.responses_state.as_mut().unwrap(),
        );
        let mut out = Vec::new();
        for ev in events {
            if ev.contains("response.completed") {
                self.terminal_registered = self.supervisor.register_terminal();
            }
            out.extend_from_slice(ev.as_bytes());
        }
        Ok(out)
    }

    /// Feed an upstream chunk.  Returns downstream bytes to emit.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, PumpError> {
        let mut out = self.start()?;
        match self.mode {
            SseMode::Native => {
                out.extend_from_slice(&self.encode_native_chunk(bytes));
            }
            SseMode::ChatToMessages | SseMode::MessagesToChat => {
                out.extend_from_slice(&self.encode_conversion_chunk(bytes)?);
            }
            SseMode::ResponsesViaChat => {
                out.extend_from_slice(
                    &self.encode_responses_chunk(&String::from_utf8_lossy(bytes))?,
                );
            }
        }
        Ok(out)
    }

    /// End-of-stream: flush any decoder final events and complete the stream.
    pub fn finish(&mut self) -> Result<Vec<u8>, PumpError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut out = self.start()?;
        match self.mode {
            SseMode::Native => {
                if !self.terminal_registered {
                    // A clean EOF without an explicit terminal marker still
                    // terminates the stream exactly once.
                    self.terminal_registered = self.supervisor.register_terminal();
                }
            }
            SseMode::ChatToMessages | SseMode::MessagesToChat => {
                let decoder = self.decoder.as_mut().ok_or_else(|| {
                    PumpError::Protocol("conversion stream is missing its decoder".to_string())
                })?;
                let events = decoder.finish().map_err(|e| {
                    PumpError::Protocol(format!(
                        "upstream stream ended with an incomplete conversion ({}): {}",
                        self.mode.as_str(),
                        e.message
                    ))
                })?;
                for ev in events {
                    if ev.contains("message_stop") || ev.contains("[DONE]") {
                        self.terminal_registered = self.supervisor.register_terminal();
                    }
                    // Codec event strings already end in "\n\n"; no extra newline.
                    out.extend_from_slice(ev.as_bytes());
                }
            }
            SseMode::ResponsesViaChat => {
                let synth = crate::protocol::responses::create_synthetic_completed_events(
                    &self.model,
                    &self.responses_id,
                    &self.accumulated_content,
                    self.responses_state.as_ref().unwrap(),
                    self.usage.map(|u| u.0).unwrap_or(0),
                    self.usage.map(|u| u.1).unwrap_or(0),
                );
                for ev in synth {
                    if ev.contains("response.completed") {
                        self.terminal_registered = self.supervisor.register_terminal();
                    }
                    out.extend_from_slice(ev.as_bytes());
                }
                out.extend_from_slice(b"data: [DONE]\n\n");
                if !self.terminal_registered {
                    self.terminal_registered = self.supervisor.register_terminal();
                }
            }
        }
        self.supervisor.complete()?;
        Ok(out)
    }

    /// Whether the stream terminated (Completed/Aborted).
    #[allow(dead_code)] // part of the pump's public API (used by the driver's
                        // future client-cancellation path)
    pub fn terminated(&self) -> bool {
        matches!(
            self.supervisor.state(),
            crate::core::stream_supervisor::StreamState::Completed
                | crate::core::stream_supervisor::StreamState::Aborted
        )
    }

    #[allow(dead_code)]
    pub fn committed(&self) -> bool {
        self.supervisor.committed()
    }

    #[allow(dead_code)]
    pub fn client_cancelled(&self) -> bool {
        self.supervisor.client_cancelled()
    }

    /// Token usage observed so far (best effort).
    pub fn usage(&self) -> (i64, i64, i64) {
        if let Some(u) = self.usage {
            return u;
        }
        if let Some(d) = self.decoder.as_ref() {
            if let Some(u) = d.usage() {
                return (
                    u.input_tokens as i64,
                    u.output_tokens as i64,
                    (u.input_tokens + u.output_tokens) as i64,
                );
            }
        }
        (0, 0, 0)
    }

    #[allow(dead_code)]
    pub fn abort(&mut self, reason: impl Into<String>) -> Result<(), PumpError> {
        self.supervisor.abort(reason).map_err(PumpError::from)
    }

    #[allow(dead_code)]
    pub fn client_cancel(&mut self) -> Result<(), PumpError> {
        self.supervisor.client_cancel().map_err(PumpError::from)
    }
}

/// Build the decoder for a conversion mode (correct direction, NOT the registry
/// wiring whose response orientation is the encoder's).
///
/// * `ChatToMessages`  = Chat SSE → Messages SSE (downstream Messages, upstream
///   Chat) uses `chat::ChatStreamDecoder` — the Chat SSE stream is converted to
///   Messages SSE events.
/// * `MessagesToChat`  = Messages SSE → Chat SSE (downstream Chat, upstream
///   Messages) uses `messages::MessagesStreamDecoder`.
pub fn decoder_for(mode: SseMode, model: &str, message_id: &str) -> Option<Box<dyn StreamDecoder>> {
    use crate::protocol::codec::report::ConversionContext;
    match mode {
        SseMode::ChatToMessages => {
            let context = ConversionContext::new(message_id, model, true);
            Some(crate::protocol::codec::chat::ChatStreamDecoder::boxed(
                &context,
            ))
        }
        SseMode::MessagesToChat => {
            let context = ConversionContext::new(message_id, model, true);
            Some(crate::protocol::codec::messages::MessagesStreamDecoder::boxed(&context))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_supervisor() -> StreamSupervisor {
        let mut s = StreamSupervisor::new();
        s.begin_connect().unwrap();
        s.on_upstream_headers().unwrap();
        s.on_first_frame_validated().unwrap();
        s
    }

    #[test]
    fn native_passthrough_commits_and_terminates_exactly_once() {
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::Native,
            None,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
            Vec::new(),
            "m".to_string(),
        )
        .unwrap();
        let first = core.start().unwrap();
        assert!(core.committed());
        assert!(String::from_utf8_lossy(&first).contains("data: {"));
        let out = core
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n"
        );
        let done = core.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(String::from_utf8_lossy(&done), "data: [DONE]\n\n");
        assert!(core.terminal_registered);
        let fin = core.finish().unwrap();
        assert!(core.terminated());
        assert!(fin.is_empty());
    }

    #[test]
    fn native_first_record_validation() {
        assert!(validate_native_first_record(b"data: {\"a\":1}\n\n").is_ok());
        assert!(validate_native_first_record(b"data: [DONE]\n\n").is_ok());
        assert!(validate_native_first_record(b"event: message_start\ndata: {}\n\n").is_ok());
        assert!(validate_native_first_record(b"data: not-json\n\n").is_err());
    }

    #[test]
    fn usage_scan_finds_openai_usage() {
        let chunk = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":6,\"total_tokens\":10}}\n\n";
        assert_eq!(scan_usage_from_chunk(chunk), Some((4, 6, 10)));
    }

    #[test]
    fn messages_to_chat_pump_converts_stream() {
        // downstream Chat, upstream Messages: Messages SSE -> Chat SSE.
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::MessagesToChat,
            decoder_for(SseMode::MessagesToChat, "up-model", ""),
            Vec::new(),
            Vec::new(),
            "up-model".to_string(),
        )
        .unwrap();
        let first = core.start().unwrap();
        assert!(first.is_empty(), "no first frame until a chunk is fed");

        let msg_start = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"model\":\"up-model\",\"content\":[]}}\n\n";
        let out = core.push(msg_start).unwrap();
        assert!(
            !out.is_empty(),
            "message_start must produce the Chat role frame"
        );
        assert!(String::from_utf8_lossy(&out).contains("data: {"));

        let delta = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
        let out = core.push(delta).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("\"content\":\"hello\""));

        // A real Messages stream carries message_delta with stop_reason before
        // message_stop; without it the codec fails closed (unknown finish reason).
        let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}\n\n";
        let _ = core.push(delta).unwrap();
        let stop = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let _ = core.push(stop).unwrap();
        let fin = core.finish().unwrap();
        assert!(String::from_utf8_lossy(&fin).contains("[DONE]"));
        assert!(core.terminated());
    }

    #[test]
    fn chat_to_messages_pump_converts_stream() {
        // downstream Messages, upstream Chat: Chat SSE -> Messages SSE.
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::ChatToMessages,
            decoder_for(SseMode::ChatToMessages, "up-model", ""),
            Vec::new(),
            Vec::new(),
            "up-model".to_string(),
        )
        .unwrap();
        let chunk =
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n";
        let out = core.push(chunk).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("message_start"));
        // A real Chat stream ends with a finish_reason frame; without it the
        // codec fails closed (unknown finish reason).
        let done = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n";
        let _ = core.push(done).unwrap();
        let fin = core.finish().unwrap();
        let text = String::from_utf8_lossy(&fin);
        assert!(text.contains("message_stop"));
        assert!(core.terminated());
    }

    #[test]
    fn empty_upstream_native_completes_without_error() {
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::Native,
            None,
            Vec::new(),
            Vec::new(),
            "m".to_string(),
        )
        .unwrap();
        let fin = core.finish().unwrap();
        assert!(core.terminated());
        assert!(fin.is_empty());
    }

    /// C-1 regression: a NON-EMPTY raw upstream first record fed to a
    /// conversion-mode pump must be encoded through the decoder BEFORE commit —
    /// `start()` must emit only converted downstream bytes, never the raw
    /// upstream protocol bytes, and the decoder must not start at record 2.
    #[test]
    fn conversion_first_frame_is_encoded_not_raw() {
        // upstream Messages record (Anthropic) → downstream Chat client.
        let raw_first = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"model\":\"up-model\",\"content\":[]}}\n\n";
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::MessagesToChat,
            decoder_for(SseMode::MessagesToChat, "up-model", ""),
            raw_first.to_vec(),
            Vec::new(),
            "up-model".to_string(),
        )
        .unwrap();
        let first = core.start().unwrap();
        assert!(
            !first.is_empty(),
            "start() must emit the converted first event"
        );
        let text = String::from_utf8_lossy(&first);
        assert!(
            !text.contains("event: message_start"),
            "downstream must never see raw upstream Anthropic bytes: {text}"
        );
        assert!(
            text.contains("data: {"),
            "downstream must see Chat SSE role frame"
        );
        assert!(core.committed());

        // The decoder state must already be past the first record: a second
        // Anthropic delta converts normally (not as a first frame).
        let delta = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
        let out = core.push(delta).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("\"content\":\"hello\""));
    }

    /// C-1 carry regression: when the first upstream chunk contains MULTIPLE
    /// records, `new()` must encode the carry (records 2..N) through the decoder
    /// too, so `start()` emits converted output for ALL records — never raw
    /// upstream protocol bytes for the carry.
    #[test]
    fn conversion_carry_is_encoded_not_raw() {
        // Two Anthropic records in one chunk (downstream Chat client): the
        // message_start first record AND a content_block_delta carry record.
        let first = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"model\":\"up-model\",\"content\":[]}}\n\n";
        let carry = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"carried\"}}\n\n";
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::MessagesToChat,
            decoder_for(SseMode::MessagesToChat, "up-model", ""),
            first.to_vec(),
            carry.to_vec(),
            "up-model".to_string(),
        )
        .unwrap();
        let out = core.start().unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            !text.contains("event: message_start") && !text.contains("event: content_block_delta"),
            "downstream must never see raw upstream Anthropic bytes (carry included): {text}"
        );
        assert!(
            text.contains("\"content\":\"carried\""),
            "carry record (record 2) must be decoded into downstream output: {text}"
        );
        assert!(core.committed());
    }

    /// C-1: a conversion-mode first record carrying reasoning is now fail-open:
    /// the ChatToMessages decoder maps it to a Messages `thinking` block and
    /// `new()` succeeds (no pre-commit rejection).
    #[test]
    fn conversion_first_frame_thinking_fail_open_before_commit() {
        let raw_first =
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"secret chain\"}}]}\n\n";
        let mut core = StreamPumpCore::new(
            native_supervisor(),
            SseMode::ChatToMessages,
            decoder_for(SseMode::ChatToMessages, "up-model", ""),
            raw_first.to_vec(),
            Vec::new(),
            "up-model".to_string(),
        )
        .expect("thinking first frame must be accepted fail-open");
        let first = core.start().unwrap();
        let text = String::from_utf8_lossy(&first);
        assert!(text.contains("message_start"), "first frame emits message_start: {text}");
        // serde_json sorts object keys (no preserve_order), so assert on
        // order-independent fragments rather than `"type":"thinking"` adjacency.
        assert!(
            text.contains("\"type\":\"thinking\"") &&
                text.contains("\"thinking\":\"secret chain\""),
            "reasoning must surface as a Messages thinking block: {text}"
        );
    }
}
