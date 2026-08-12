use std::fmt;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    core::attempt::FailureClass,
    db::models::{AuthAccount, ModelState, ModelStates, QuotaState},
};

/// Provider names accepted by the local registry.  The database intentionally
/// has no corresponding CHECK constraint so new providers do not require a
/// schema migration; registration remains the runtime authority.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ProviderKind {
    Codex,
    Other(String),
}

impl ProviderKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Codex => "codex",
            Self::Other(value) => value,
        }
    }
}

impl From<&str> for ProviderKind {
    fn from(value: &str) -> Self {
        match value {
            "codex" => Self::Codex,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Provider-owned credential material.  It must never cross command, log, or
/// debug boundaries.  Provider implementations can inspect the JSON through
/// `as_value`; every formatter emits only a redacted marker.
#[derive(Clone)]
pub struct ProviderPayload(Value);

impl ProviderPayload {
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }

    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.0
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    }
}

impl fmt::Debug for ProviderPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderPayload(<redacted>)")
    }
}

/// The account shape visible beyond the auth service.  It deliberately has no
/// raw JSON credential field: route and executor code can select an account by
/// this summary but cannot parse `payload_json`.
#[derive(Clone, serde::Serialize)]
pub struct AuthAccountSummary {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub account_id: String,
    pub status: String,
    pub disabled: bool,
    pub priority: i64,
    pub weight: i64,
    pub quota: Option<QuotaState>,
    pub models: ModelStates,
    pub model_mapping: serde_json::Value,
    pub attributes: Value,
    pub expires_at: Option<String>,
    pub has_refresh_token: bool,
    pub last_refreshed_at: Option<String>,
    pub last_models_sync_at: Option<String>,
    pub next_refresh_after: Option<String>,
    pub next_retry_after: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl AuthAccountSummary {
    pub fn from_account(account: &AuthAccount) -> Result<Self, ProviderError> {
        let payload: Value = serde_json::from_str(&account.payload_json)
            .map_err(|_| ProviderError::InvalidPayload)?;
        let models = account
            .model_states()
            .map_err(|_| ProviderError::InvalidPayload)?;
        let quota = account
            .quota_state()
            .map_err(|_| ProviderError::InvalidPayload)?;
        let attributes = serde_json::from_str(&account.attributes_json)
            .map_err(|_| ProviderError::InvalidPayload)?;
        let model_mapping = account.model_mapping()
            .map_err(|_| ProviderError::InvalidPayload)?;
        Ok(Self {
            id: account.id.clone(),
            provider: account.provider.clone(),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            status: account.status.clone(),
            disabled: account.disabled != 0,
            priority: account.priority,
            weight: account.weight,
            quota,
            models,
            model_mapping,
            attributes,
            expires_at: payload
                .get("expires_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            has_refresh_token: payload
                .get("refresh_token")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty()),
            last_refreshed_at: account.last_refreshed_at.clone(),
            last_models_sync_at: account.last_models_sync_at.clone(),
            next_refresh_after: account.next_refresh_after.clone(),
            next_retry_after: account.next_retry_after.clone(),
            created_at: account.created_at.clone(),
            updated_at: account.updated_at.clone(),
        })
    }
}

impl fmt::Debug for AuthAccountSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthAccountSummary")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("label", &self.label)
            .field("account_id", &self.account_id)
            .field("status", &self.status)
            .field("disabled", &self.disabled)
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("quota", &self.quota)
            .field("models", &self.models)
            .field("attributes", &"<omitted>")
            .field("expires_at", &self.expires_at)
            .field("has_refresh_token", &self.has_refresh_token)
            .finish()
    }
}

/// Provider result used by both interactive login and local auth-file import.
#[derive(Clone)]
pub struct LoginResult {
    pub account_id: String,
    pub label: String,
    pub attributes: Value,
    pub payload: ProviderPayload,
    pub last_refreshed_at: Option<String>,
    pub next_refresh_after: Option<String>,
    pub next_retry_after: Option<String>,
}

impl fmt::Debug for LoginResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginResult")
            .field("account_id", &self.account_id)
            .field("label", &self.label)
            .field("attributes", &"<omitted>")
            .field("payload", &self.payload)
            .finish()
    }
}

/// Credential material returned after a successful provider refresh.
#[derive(Clone, Debug)]
pub struct RefreshedPayload {
    pub payload: ProviderPayload,
    pub last_refreshed_at: Option<String>,
    pub next_refresh_after: Option<String>,
    pub next_retry_after: Option<String>,
}

/// A non-secret outbound request.  The service injects the persisted account
/// and decrypted-in-memory provider payload immediately before dispatch.
pub struct ProviderRequest<'a> {
    pub account: &'a crate::db::models::AuthAccount,
    pub payload: &'a ProviderPayload,
    pub body: &'a Value,
    pub headers: &'a reqwest::header::HeaderMap,
}

impl fmt::Debug for ProviderRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRequest")
            .field("account_id", &self.account.id)
            .field("provider", &self.account.provider)
            .field("body", &"<caller payload omitted>")
            .finish()
    }
}

/// Typed, deliberately non-secret provider failures.  Diagnostic details stay
/// internal to the provider implementation; user-facing formatting is stable
/// and never includes tokens, OAuth codes, request bodies, or auth.json bytes.
#[derive(Clone, PartialEq, Eq)]
pub enum ProviderError {
    UnknownProvider { provider: String },
    InvalidPayload,
    LoginFailed,
    LoginCancelled,
    LoginTimeout,
    BrowserOpenFailed,
    CallbackFailed,
    TokenExchangeFailed,
    ImportFailed,
    Unauthorized,
    UnsupportedFeatures { pointer: String },
    Retryable,
    Storage,
    Protocol,
}

impl ProviderError {
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::UnsupportedFeatures { .. } | Self::InvalidPayload => FailureClass::CallerTerminal,
            Self::Unauthorized => FailureClass::ChannelAuthTerminal,
            Self::Protocol => FailureClass::UpstreamProtocolError,
            Self::UnknownProvider { .. }
            | Self::LoginFailed
            | Self::LoginCancelled
            | Self::LoginTimeout
            | Self::BrowserOpenFailed
            | Self::CallbackFailed
            | Self::TokenExchangeFailed
            | Self::ImportFailed
            | Self::Retryable
            | Self::Storage => FailureClass::Retryable,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Provider strings can originate outside this process.  Keep even
            // this diagnostic generic rather than echoing untrusted text.
            Self::UnknownProvider { .. } => formatter.write_str("unknown auth provider"),
            Self::InvalidPayload => formatter.write_str("invalid provider credential payload"),
            Self::LoginFailed => formatter.write_str("provider login failed"),
            Self::LoginCancelled => formatter.write_str("provider login was cancelled"),
            Self::LoginTimeout => formatter.write_str("provider login timed out"),
            Self::BrowserOpenFailed => formatter.write_str("could not open provider login browser"),
            Self::CallbackFailed => formatter.write_str("provider login callback failed"),
            Self::TokenExchangeFailed => formatter.write_str("provider token exchange failed"),
            Self::ImportFailed => formatter.write_str("provider credential import failed"),
            Self::Unauthorized => formatter.write_str("provider credentials were rejected"),
            Self::UnsupportedFeatures { pointer } => {
                write!(formatter, "unsupported provider request field at {pointer}")
            }
            Self::Retryable => formatter.write_str("provider request failed; retry later"),
            Self::Storage => formatter.write_str("auth account storage operation failed"),
            Self::Protocol => formatter.write_str("provider response protocol error"),
        }
    }
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug is intentionally the same redacted classification as Display.
        write!(formatter, "ProviderError({self})")
    }
}

impl std::error::Error for ProviderError {}

pub type ProviderModels = Vec<ModelState>;
