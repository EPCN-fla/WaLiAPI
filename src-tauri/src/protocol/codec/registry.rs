//! Versioned, directed codec registry (T00 decision 8).
//!
//! A codec is a directed implementation of `(downstream_endpoint,
//! upstream_endpoint, version)` returning `Result<Converted, UnsupportedFeatures>`.
//! This first version registers only the four directions that pair
//! `chat_completions` (OpenAI Chat Completions) and `messages` (Anthropic
//! Messages) at version `chat_to_messages_v1` / `messages_to_chat_v1`.
//!
//! A request for a direction that has no codec is an error; the gateway never
//! passes a raw payload through when no codec exists.

use super::error::{CodecError, UnsupportedFeatures};
use super::report::{ConversionContext, ConversionReport};
use super::{chat, messages};

/// Downstream endpoint protocol kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Downstream {
    ChatCompletions,
    Messages,
}

/// Upstream endpoint protocol kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Upstream {
    ChatCompletions,
    Messages,
}

/// A codec version identifier for registry lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version(String);

impl Version {
    pub fn v1_0() -> Self {
        Self("v1".to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Result of preparing a request through a codec direction, including the
/// encoders/decoders the gateway needs.
pub struct PreparedConversion {
    /// Encoded upstream request body.
    pub encoded_request: serde_json::Value,
    /// Per-request context (request id, upstream model, stream).
    pub context: ConversionContext,
    /// What the codec rejected/normalized.
    pub report: ConversionReport,
    /// Non-stream response decoder for this direction.
    pub non_stream: Box<dyn NonStreamDecoder + Send + Sync>,
    /// Streaming response decoder for this direction.
    pub streaming: Box<dyn StreamDecoder + Send + Sync>,
}

/// Non-stream response decoder for a given direction.
pub trait NonStreamDecoder: Send + Sync {
    /// Decode an upstream non-stream response body into the downstream
    /// protocol.  The body is supplied by the caller.
    fn decode(&self, body: &serde_json::Value) -> Result<serde_json::Value, UnsupportedFeatures>;
}

/// Streaming response decoder for a given direction.
pub trait StreamDecoder: Send + Sync {
    /// Feed an arbitrary byte chunk (SSE bytes).  Returns downstream-protocol
    /// event strings produced so far.  A first-frame validation failure is
    /// returned as an error for pre-commit failover.
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures>;
    /// Flush end-of-stream and return the exactly-once final sequence.
    fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures>;
    /// The token usage observed so far, if the protocol reports any.
    fn usage(&self) -> Option<super::report::Usage>;
}

/// A registered direction implementation.
struct Direction {
    encode: fn(
        &serde_json::Value,
        &str,
    ) -> Result<(serde_json::Value, ConversionContext), UnsupportedFeatures>,
    non_stream: fn(&ConversionContext) -> Box<dyn NonStreamDecoder + Send + Sync>,
    streaming: fn(&ConversionContext) -> Box<dyn StreamDecoder + Send + Sync>,
}

/// Simple registry over the two endpoints.  In this first version exactly one
/// implementation is registered per supported direction; an unregistered
/// direction is an error (never a raw passthrough).
pub struct CodecRegistry;

impl CodecRegistry {
    /// The codec version this registry speaks.
    pub fn version() -> Version {
        Version::v1_0()
    }

    fn direction(
        downstream: Downstream,
        upstream: Upstream,
    ) -> Result<&'static Direction, CodecError> {
        static V1: Direction = Direction {
            encode: chat::encode_chat_to_messages,
            non_stream: chat::NonStreamResponseDecoder::boxed,
            streaming: chat::ChatStreamDecoder::boxed,
        };
        static V2: Direction = Direction {
            encode: messages::encode_messages_to_chat,
            non_stream: messages::NonStreamResponseDecoder::boxed,
            streaming: messages::MessagesStreamDecoder::boxed,
        };
        match (downstream, upstream) {
            (Downstream::ChatCompletions, Upstream::Messages) => Ok(&V1),
            (Downstream::Messages, Upstream::ChatCompletions) => Ok(&V2),
            _ => Err(CodecError::new(format!(
                "no codec registered for downstream {:?} -> upstream {:?}",
                downstream, upstream
            ))),
        }
    }

    /// Prepare a directed conversion: encode the downstream request into the
    /// upstream protocol (running full feature validation, which rejects
    /// unsupported features before any upstream access) and wire the matching
    /// response decoders.  `model` is the mapped upstream model passed in by
    /// the caller — the codec never re-maps models.  Returns a `CodecError`
    /// when no codec exists for the path (the gateway must then fail closed,
    /// never pass through raw).
    pub fn prepare(
        downstream: Downstream,
        upstream: Upstream,
        _version: &Version,
        model: &str,
        request: &serde_json::Value,
    ) -> Result<PreparedConversion, UnsupportedFeatures> {
        let dir = Self::direction(downstream, upstream).map_err(|e| {
            UnsupportedFeatures::single(
                super::error::FeatureKind::UnsupportedField,
                "/",
                e.to_string(),
            )
        })?;
        let (encoded_request, context) = (dir.encode)(request, model)?;
        // The encoder records fail-open drops/transforms on the context; fold
        // them into the report so callers can observe what was normalized.
        let report = ConversionReport::new(vec![], context.normalized.clone());
        let non_stream = (dir.non_stream)(&context);
        let streaming = (dir.streaming)(&context);
        Ok(PreparedConversion {
            encoded_request,
            context,
            report,
            non_stream,
            streaming,
        })
    }

    /// Convenience: prepare a Chat → Messages conversion (`chat_to_messages_v1`).
    pub fn chat_to_messages(
        model: &str,
        request: &serde_json::Value,
    ) -> Result<PreparedConversion, UnsupportedFeatures> {
        Self::prepare(
            Downstream::ChatCompletions,
            Upstream::Messages,
            &Self::version(),
            model,
            request,
        )
    }

    /// Convenience: prepare a Messages → Chat conversion (`messages_to_chat_v1`).
    pub fn messages_to_chat(
        model: &str,
        request: &serde_json::Value,
    ) -> Result<PreparedConversion, UnsupportedFeatures> {
        Self::prepare(
            Downstream::Messages,
            Upstream::ChatCompletions,
            &Self::version(),
            model,
            request,
        )
    }
}

impl std::fmt::Debug for PreparedConversion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedConversion")
            .field("encoded_request", &self.encoded_request)
            .field("context", &self.context)
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}
