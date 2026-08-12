//! SSE framing + the streaming commit-barrier pump.
//!
//! Protocol conversion is deliberately absent from this module.  A selected
//! [`PreparedCodec`](crate::protocol::codec::PreparedCodec) creates the decoder
//! before entering the pump, so this type only owns framing, commit state and
//! decoder driving.

use crate::core::stream_supervisor::{StreamSupervisor, StreamTransitionError};
use crate::protocol::codec::StreamDecoder;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum PumpError {
    Protocol(String),
    Supervisor(String),
}

impl From<StreamTransitionError> for PumpError {
    fn from(error: StreamTransitionError) -> Self {
        Self::Supervisor(format!("{error:?}"))
    }
}

impl PumpError {
    pub fn message(&self) -> &str {
        match self {
            Self::Protocol(message) | Self::Supervisor(message) => message,
        }
    }
}

pub fn record_end(input: &[u8]) -> Option<usize> {
    crate::protocol::codec::sse::record_end(input)
}

pub fn parse_data_payload(record: &[u8]) -> Result<String, String> {
    crate::protocol::codec::sse::parse_data_payload(record).map_err(|error| error.message)
}

/// Validate only enough framing to retain the pre-commit failover barrier.
/// Full protocol validation belongs to the decoder factory selected at prepare
/// time and therefore runs inside [`StreamPumpCore::new`].
pub fn validate_native_first_record(record: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(record);
    if text.contains("event:") {
        return Ok(());
    }
    let payload = parse_data_payload(record)?;
    if payload.is_empty() || payload == "[DONE]" {
        return Ok(());
    }
    serde_json::from_str::<Value>(&payload)
        .map(|_| ())
        .map_err(|error| format!("first SSE data frame is not valid JSON: {error}"))
}

/// A protocol-agnostic pump. The decoder is mandatory: identity directions use
/// the same path as conversions, which prevents an executor-side "native"
/// branch from bypassing validation or usage collection.
pub struct StreamPumpCore {
    supervisor: StreamSupervisor,
    decoder: Box<dyn StreamDecoder + Send + Sync>,
    first_frame: Vec<u8>,
    first_done: bool,
    terminal_registered: bool,
    finished: bool,
    accumulated_content: String,
}

impl StreamPumpCore {
    /// Feed the complete first record and any bytes read past it into the same
    /// fresh decoder before committing downstream. This makes an invalid first
    /// response (including an identity response) retryable without leaking raw
    /// bytes to the client.
    pub fn new(
        supervisor: StreamSupervisor,
        mut decoder: Box<dyn StreamDecoder + Send + Sync>,
        first_frame: Vec<u8>,
        carry: Vec<u8>,
    ) -> Result<Self, PumpError> {
        let mut output = Vec::new();
        for bytes in [&first_frame[..], &carry[..]] {
            if bytes.is_empty() {
                continue;
            }
            let events = decoder.feed(bytes).map_err(|error| {
                PumpError::Protocol(format!("upstream stream could not be decoded: {error}"))
            })?;
            for event in events {
                output.extend_from_slice(event.as_bytes());
            }
        }
        Ok(Self {
            supervisor,
            decoder,
            first_frame: output,
            first_done: false,
            terminal_registered: false,
            finished: false,
            accumulated_content: String::new(),
        })
    }

    pub fn start(&mut self) -> Result<Vec<u8>, PumpError> {
        if self.first_done {
            return Ok(Vec::new());
        }
        self.supervisor.commit_downstream()?;
        self.supervisor.begin_streaming()?;
        self.first_done = true;
        Ok(std::mem::take(&mut self.first_frame))
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, PumpError> {
        let mut output = self.start()?;
        let events = self.decoder.feed(bytes).map_err(|error| {
            PumpError::Protocol(format!("upstream stream could not be decoded: {error}"))
        })?;
        for event in events {
            output.extend_from_slice(event.as_bytes());
        }
        Ok(output)
    }

    pub fn finish(&mut self) -> Result<Vec<u8>, PumpError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut output = self.start()?;
        let events = self.decoder.finish().map_err(|error| {
            PumpError::Protocol(format!(
                "upstream stream ended with an incomplete decode: {error}"
            ))
        })?;
        for event in events {
            output.extend_from_slice(event.as_bytes());
        }
        // Decoder correctness includes protocol terminal validation. The pump
        // owns only the supervisor's exactly-once terminal transition.
        if !self.terminal_registered {
            self.terminal_registered = self.supervisor.register_terminal();
        }
        Ok(output)
    }

    pub fn committed(&self) -> bool {
        self.supervisor.committed()
    }

    pub fn terminated(&self) -> bool {
        self.supervisor.terminal_emitted()
    }

    pub fn usage(&self) -> (i64, i64, i64) {
        self.decoder.usage().map_or((0, 0, 0), |usage| {
            let prompt = usage.input_tokens as i64;
            let completion = usage.output_tokens as i64;
            (prompt, completion, prompt + completion)
        })
    }

    pub fn accumulated_content(&self) -> &str {
        &self.accumulated_content
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::codec::{DecodeError, Usage};

    struct Decoder {
        usage: Option<Usage>,
    }

    impl StreamDecoder for Decoder {
        fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, DecodeError> {
            let text = String::from_utf8_lossy(bytes);
            if text.contains("bad") {
                return Err(DecodeError::new("/", "bad upstream event"));
            }
            if text.contains("usage") {
                self.usage = Some(Usage {
                    input_tokens: 2,
                    output_tokens: 3,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    usage_unknown: false,
                });
            }
            Ok(vec![format!("out:{text}")])
        }

        fn finish(&mut self) -> Result<Vec<String>, DecodeError> {
            Ok(vec!["done".into()])
        }

        fn usage(&self) -> Option<Usage> {
            self.usage
        }
    }

    fn supervisor() -> StreamSupervisor {
        let mut supervisor = StreamSupervisor::new();
        supervisor.begin_connect().unwrap();
        supervisor.on_upstream_headers().unwrap();
        supervisor.on_first_frame_validated().unwrap();
        supervisor
    }

    #[test]
    fn first_record_and_carry_are_decoded_before_commit() {
        let mut pump = StreamPumpCore::new(
            supervisor(),
            Box::new(Decoder { usage: None }),
            b"first".to_vec(),
            b"carry".to_vec(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(pump.start().unwrap()).unwrap(),
            "out:firstout:carry"
        );
        assert!(pump.committed());
    }

    #[test]
    fn decoder_usage_reaches_the_pump() {
        let mut pump = StreamPumpCore::new(
            supervisor(),
            Box::new(Decoder { usage: None }),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        pump.push(b"usage").unwrap();
        assert_eq!(pump.usage(), (2, 3, 5));
        assert_eq!(pump.finish().unwrap(), b"done");
        assert!(pump.terminated());
    }

    #[test]
    fn first_decoder_failure_stays_precommit() {
        let result = StreamPumpCore::new(
            supervisor(),
            Box::new(Decoder { usage: None }),
            b"bad".to_vec(),
            Vec::new(),
        );
        let error = match result {
            Ok(_) => panic!("bad first event must fail before commit"),
            Err(error) => error,
        };
        assert!(error.message().contains("bad upstream event"));
    }
}
