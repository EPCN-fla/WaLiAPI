//! Response decoder ports exposed by the codec facade.

use super::error::{DecodeError, FeatureKind, UnsupportedFeatures};
use super::report::Usage;
use serde_json::Value;
use std::ops::Deref;

/// A fully decoded upstream response and the usage observed while decoding it.
///
/// Returning usage with the body prevents consumers from reparsing the raw
/// provider response through protocol-specific side channels.
#[derive(Debug, Clone)]
pub struct DecodedResponse {
    pub body: Value,
    pub usage: Option<Usage>,
}

/// Temporary source compatibility for callers that consumed a decoder result
/// as raw JSON. New consumers should access `.body` explicitly.
impl Deref for DecodedResponse {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

/// Factory-produced decoder for one non-stream upstream response.
pub trait NonStreamDecoder: Send + Sync {
    fn decode(&self, body: &Value) -> Result<DecodedResponse, DecodeError>;
}

/// Factory-produced, stateful SSE decoder for one upstream stream.
pub trait StreamDecoder: Send + Sync {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, DecodeError>;
    fn finish(&mut self) -> Result<Vec<String>, DecodeError>;
    fn usage(&self) -> Option<Usage>;
}

/// Compatibility port for the pre-factory registry API. It returns only the
/// decoded body; new callers must consume usage from [`DecodedResponse`].
pub trait LegacyNonStreamDecoder: Send + Sync {
    fn decode(&self, body: &Value) -> Result<Value, UnsupportedFeatures>;
}

/// Compatibility port for the pre-factory registry API.
pub trait LegacyStreamDecoder: Send + Sync {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures>;
    fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures>;
    fn usage(&self) -> Option<Usage>;
}

pub(crate) struct LegacyNonStreamDecoderAdapter {
    inner: Box<dyn NonStreamDecoder + Send + Sync>,
}

impl LegacyNonStreamDecoderAdapter {
    pub(crate) fn new(inner: Box<dyn NonStreamDecoder + Send + Sync>) -> Self {
        Self { inner }
    }
}

impl LegacyNonStreamDecoder for LegacyNonStreamDecoderAdapter {
    fn decode(&self, body: &Value) -> Result<Value, UnsupportedFeatures> {
        self.inner
            .decode(body)
            .map(|decoded| decoded.body)
            .map_err(decode_error)
    }
}

pub(crate) struct LegacyStreamDecoderAdapter {
    inner: Box<dyn StreamDecoder + Send + Sync>,
}

impl LegacyStreamDecoderAdapter {
    pub(crate) fn new(inner: Box<dyn StreamDecoder + Send + Sync>) -> Self {
        Self { inner }
    }
}

impl LegacyStreamDecoder for LegacyStreamDecoderAdapter {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, UnsupportedFeatures> {
        self.inner.feed(bytes).map_err(decode_error)
    }

    fn finish(&mut self) -> Result<Vec<String>, UnsupportedFeatures> {
        self.inner.finish().map_err(decode_error)
    }

    fn usage(&self) -> Option<Usage> {
        self.inner.usage()
    }
}

fn decode_error(error: DecodeError) -> UnsupportedFeatures {
    UnsupportedFeatures::single(FeatureKind::UnsupportedField, error.pointer, error.message)
}
