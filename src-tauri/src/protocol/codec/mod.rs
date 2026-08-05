//! T04 — Strict, versioned Chat Completions ↔ Anthropic Messages codec.
//!
//! Every conversion returns [`Result<Converted, UnsupportedFeatures>`]; there is
//! no raw JSON passthrough, no silent field dropping, no invented tool
//! arguments, and no downgrading of an unknown finish reason to a normal
//! stop/end_turn.  Unsupported features are rejected with a stable error code
//! and JSON pointer *before* any upstream access (zero upstream calls).
//!
//! Directions implemented in this first version:
//!   - [`chat_to_messages_v1`]:   Chat Completions request  → Messages request
//!   - [`messages_to_chat_v1`]:   Messages request         → Chat request
//! plus the response directions:
//!   - [`chat::NonStreamResponseDecoder`]  (Chat  → Messages non-stream body)
//!   - [`messages::NonStreamResponseDecoder`] (Messages → Chat non-stream body)
//!   - [`chat::StreamDecoder`]  (OpenAI Chat SSE  → Messages SSE)
//!   - [`messages::StreamDecoder`] (Anthropic Messages SSE → Chat SSE)

pub mod chat;
pub mod error;
pub mod messages;
pub mod registry;
pub mod report;
pub mod request;
pub mod sse;

pub use error::{
    CodecError, FeatureKind, UnsupportedFeatures, CODEC_UNSUPPORTED_FEATURE,
    CODEC_UNSUPPORTED_MEDIA,
};
pub use registry::{
    CodecRegistry, Downstream, NonStreamDecoder, PreparedConversion, StreamDecoder, Upstream,
    Version,
};
pub use report::{CodecVersion, ConversionReport, ConversionContext, Usage, FieldStatus};

#[cfg(test)]
mod chat_messages_codec;
