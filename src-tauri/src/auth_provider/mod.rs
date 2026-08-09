//! Provider-neutral authentication boundary.  Provider implementations own
//! OAuth/import/HTTP details; this module owns object-safe dispatch and lookup.

pub mod codex_backend;
pub mod codex_login;
pub mod maintenance;
pub mod service;
pub mod types;

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

pub use crate::db::models::{AuthAccount, QuotaState};
pub use types::{
    AuthAccountSummary, LoginResult, ProviderError, ProviderKind, ProviderModels, ProviderPayload,
    ProviderRequest, RefreshedPayload,
};

/// Minimal host capability needed by an interactive provider login.  Specific
/// providers may add their own local callback handling without coupling that
/// logic to Tauri commands.
#[async_trait]
pub trait LoginRuntime: Send + Sync {
    async fn open_browser(&self, url: &str) -> Result<(), ProviderError>;
}

/// Object-safe provider contract.  Credentials only cross this boundary as a
/// `ProviderPayload`, whose debug output is redacted.
#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn login(&self, runtime: &dyn LoginRuntime) -> Result<LoginResult, ProviderError>;

    async fn import(&self, bytes: &[u8]) -> Result<LoginResult, ProviderError>;

    async fn refresh(&self, payload: &ProviderPayload) -> Result<RefreshedPayload, ProviderError>;

    async fn outbound(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<reqwest::Response, ProviderError>;

    async fn list_models(
        &self,
        account: &AuthAccount,
        payload: &ProviderPayload,
    ) -> Result<ProviderModels, ProviderError>;

    /// Probe the provider's dedicated quota endpoint.  `Ok(None)` means no quota
    /// data is currently available (callers preserve previously persisted state).
    /// The default is a no-op so providers without a dedicated endpoint stay
    /// header/cooldown-only.
    async fn fetch_quota(
        &self,
        _account: &AuthAccount,
        _payload: &ProviderPayload,
    ) -> Result<Option<QuotaState>, ProviderError> {
        Ok(None)
    }
}

/// Runtime registry, deliberately separate from persisted provider strings.
/// An account is usable only when its provider is registered.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderKind, Arc<dyn Provider>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        let mut registry = Self {
            providers: HashMap::new(),
        };
        registry.register(Arc::new(codex_backend::CodexProvider::new()));
        registry
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.providers.insert(provider.kind(), provider);
    }

    pub fn get(&self, kind: &ProviderKind) -> Result<Arc<dyn Provider>, ProviderError> {
        self.providers
            .get(kind)
            .cloned()
            .ok_or_else(|| ProviderError::UnknownProvider {
                provider: kind.to_string(),
            })
    }

    pub fn provider_for_name(&self, provider: &str) -> Result<Arc<dyn Provider>, ProviderError> {
        self.get(&ProviderKind::from(provider))
    }
}
