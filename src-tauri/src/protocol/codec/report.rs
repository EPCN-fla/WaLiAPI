//! Conversion report produced by every codec direction.

use super::types::CodecId;
use serde::Serialize;

/// Stable, versioned identifier of the codec implementation.
///
/// The pair of functions (e.g. `chat_to_messages_v1`) share one codec version
/// because the request encoder and the response decoder must be deployed
/// together and always agree on semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct CodecVersion {
    pub major: u32,
    pub minor: u32,
}

impl CodecVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
    pub fn label(&self) -> String {
        format!("{}.{}.0", self.major, self.minor)
    }
}

/// Per-field audit status inside the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldStatus {
    /// The field was validated and mapped 1:1 with no information loss.
    Preserved,
    /// A lossless annotation was stripped (e.g. Anthropic `cache_control`).
    Normalized,
    /// The field was rejected with an error before any upstream access.
    Rejected,
}

/// What the codec did to the request/response it converted.
/// Token usage observed from a real upstream response.
///
/// `usage_unknown` is set when the upstream did not report a value; callers
/// must never treat `0` as an exact count when this flag is set.  Only the
/// protocol-mandated field (e.g. Anthropic `usage`) emits a compatible `0`,
/// and even then the gateway logs `usage_unknown=true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache tokens are only a billing annotation on the Anthropic side and
    /// are surfaced into OpenAI `usage_details` without double-counting.
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub usage_unknown: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversionReport {
    /// Fields rejected (codes + pointers); empty on success.
    pub rejected: Vec<RejectedReportEntry>,
    /// Fields that were normalized (kept semantically, changed representation).
    pub normalized: Vec<String>,
    /// The exact directed codec selected for this conversion.
    pub codec_id: CodecId,
}

#[derive(Debug, Clone, Serialize)]
pub struct RejectedReportEntry {
    pub code: String,
    pub pointer: String,
}

impl ConversionReport {
    /// Legacy constructor. New preparation code must record its selected codec
    /// with [`Self::for_codec`].
    pub fn new(rejected: Vec<RejectedReportEntry>, normalized: Vec<String>) -> Self {
        Self::for_codec(CodecId::Native, rejected, normalized)
    }

    pub fn for_codec(
        codec_id: CodecId,
        rejected: Vec<RejectedReportEntry>,
        normalized: Vec<String>,
    ) -> Self {
        Self {
            rejected,
            normalized,
            codec_id,
        }
    }

    pub fn ok() -> Self {
        Self::for_codec(CodecId::Native, vec![], vec![])
    }
}

/// Result of a request conversion: the encoded upstream body plus the report.
#[derive(Debug, Clone)]
pub struct ConvertedRequest {
    pub encoded_request: serde_json::Value,
    pub context: ConversionContext,
    pub report: ConversionReport,
}
/// Context handed to the response decoder so the response can be expressed in
/// the downstream protocol (message ids, mapped upstream model, stream flag).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConversionContext {
    /// Downstream request id (message id for the messages side, `chatcmpl-` for chat).
    pub request_id: String,
    /// The mapped upstream model as passed into the codec (never re-mapped
    /// inside the codec).
    pub upstream_model: String,
    pub stream: bool,
    /// JSON pointers (e.g. `/container`) of fields the encoder dropped or
    /// transformed in a fail-open way during request encoding.  Populated by
    /// the encoder and surfaced through the [`ConversionReport`].
    pub normalized: Vec<String>,
}

impl ConversionContext {
    pub fn new(
        request_id: impl Into<String>,
        upstream_model: impl Into<String>,
        stream: bool,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            upstream_model: upstream_model.into(),
            stream,
            normalized: Vec::new(),
        }
    }
}
