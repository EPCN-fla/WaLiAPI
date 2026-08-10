use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use tokio::sync::Mutex;

use crate::{
    auth_provider::{
        AuthAccountSummary, LoginRuntime, ProviderError, ProviderKind, ProviderPayload,
        ProviderRegistry, ProviderRequest,
    },
    db::{
        models::{AuthAccount, AuthAccountUpsert, ModelStates},
        repository::Repository,
    },
};

/// Injectable time source: refresh decisions are deterministic in tests and do
/// not require a background timer in the service itself.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Repository orchestration around generic providers.  One mutex per account
/// serializes refresh-token rotation.  The lock is held through re-read and
/// persistence so concurrent callers cannot overwrite a newer refresh token.
pub struct AuthService {
    repository: Arc<Repository>,
    registry: ProviderRegistry,
    clock: Arc<dyn Clock>,
    refresh_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl AuthService {
    pub fn new(repository: Arc<Repository>, registry: ProviderRegistry) -> Self {
        Self::with_clock(repository, registry, Arc::new(SystemClock))
    }

    pub fn with_clock(
        repository: Arc<Repository>,
        registry: ProviderRegistry,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repository,
            registry,
            clock,
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn login(
        &self,
        kind: ProviderKind,
        runtime: &dyn LoginRuntime,
    ) -> Result<AuthAccountSummary, ProviderError> {
        let provider = self.registry.get(&kind)?;
        let result = provider.login(runtime).await?;
        self.upsert_login_result(kind, result).await
    }

    pub async fn import(
        &self,
        kind: ProviderKind,
        bytes: &[u8],
    ) -> Result<AuthAccountSummary, ProviderError> {
        let provider = self.registry.get(&kind)?;
        let result = provider.import(bytes).await?;
        self.upsert_login_result(kind, result).await
    }

    async fn upsert_login_result(
        &self,
        kind: ProviderKind,
        result: crate::auth_provider::LoginResult,
    ) -> Result<AuthAccountSummary, ProviderError> {
        let account = self
            .repository
            .upsert_by_provider_account_id(&AuthAccountUpsert {
                provider: kind.to_string(),
                label: result.label,
                account_id: result.account_id,
                attributes: result.attributes,
                payload: result.payload.into_value(),
                last_refreshed_at: result.last_refreshed_at,
                next_refresh_after: result.next_refresh_after,
                next_retry_after: result.next_retry_after,
            })
            .await
            .map_err(|_| ProviderError::Storage)?;
        AuthAccountSummary::from_account(&account)
    }

    /// Refresh an account only if its token is missing an expiry or expires in
    /// the next five minutes.  This is the normal request-path entrypoint.
    pub async fn refresh_account(
        &self,
        account_id: &str,
    ) -> Result<AuthAccountSummary, ProviderError> {
        self.refresh_account_if_due(account_id, self.clock.now() + Duration::minutes(5))
            .await
    }

    pub async fn refresh_account_if_due(
        &self,
        account_id: &str,
        refresh_before: DateTime<Utc>,
    ) -> Result<AuthAccountSummary, ProviderError> {
        self.refresh_with_lock(account_id, refresh_before, false, true)
            .await
    }

    /// Used only by explicit user refresh and the one permitted 401 retry.
    pub async fn force_refresh_account(
        &self,
        account_id: &str,
    ) -> Result<AuthAccountSummary, ProviderError> {
        self.refresh_with_lock(account_id, self.clock.now(), true, true)
            .await
    }

    async fn refresh_with_lock(
        &self,
        account_id: &str,
        refresh_before: DateTime<Utc>,
        force: bool,
        probe_quota: bool,
    ) -> Result<AuthAccountSummary, ProviderError> {
        let lock = {
            let mut locks = self.refresh_locks.lock().await;
            locks
                .entry(account_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Re-read while holding the lock: a preceding caller may have rotated
        // credentials, making this caller's refresh unnecessary.
        let account = self.get_account(account_id).await?;
        let payload = Self::payload_for(&account)?;
        if !force && !Self::needs_refresh(&payload, refresh_before) {
            return AuthAccountSummary::from_account(&account);
        }

        let provider = self.registry.provider_for_name(&account.provider)?;
        let refreshed = provider.refresh(&payload).await?;
        self.repository
            .update_tokens(
                &account.id,
                refreshed.payload.as_value(),
                refreshed.last_refreshed_at.as_deref(),
                refreshed.next_refresh_after.as_deref(),
                refreshed.next_retry_after.as_deref(),
            )
            .await
            .map_err(|_| ProviderError::Storage)?;
        if probe_quota {
            // A user-invoked refresh is a natural moment to refresh quota state too.
            // The refreshed payload is re-read below so the probe always carries the
            // newest access token.
            self.sync_quota(account_id).await;
        }
        let account = self.get_account(account_id).await?;
        AuthAccountSummary::from_account(&account)
    }

    pub async fn sync_models(&self, account_id: &str) -> Result<AuthAccountSummary, ProviderError> {
        // A due token is refreshed before the model snapshot so the listing is
        // never taken with an expired access token.  The refresh is lock-guarded
        // and re-reads the persisted payload, so a concurrent rotation cannot be
        // overwritten.  A refresh failure aborts before any model request.
        let account = self.get_account(account_id).await?;
        if Self::has_refresh_token(&Self::payload_for(&account)?) {
            self.refresh_with_lock(
                account_id,
                self.clock.now() + Duration::minutes(5),
                false,
                false,
            )
            .await?;
        }
        let account = self.get_account(account_id).await?;
        let payload = Self::payload_for(&account)?;
        let provider = self.registry.provider_for_name(&account.provider)?;
        let models = provider.list_models(&account, &payload).await?;
        self.repository
            .update_models_if_success(
                account_id,
                &ModelStates { version: 1, models },
                &self.clock.now().to_rfc3339(),
            )
            .await
            .map_err(|_| ProviderError::Storage)?;
        // Model sync is the login/import/refresh-triggered path; probe quota
        // alongside so a freshly added account shows its limits without waiting
        // for traffic or the 12h maintenance cycle.
        self.sync_quota(account_id).await;
        let account = self.get_account(account_id).await?;
        AuthAccountSummary::from_account(&account)
    }

    /// Probe the provider's dedicated quota endpoint and persist the result.
    /// Failures are silent and preserve whatever quota was previously stored —
    /// a quota probe never turns a successful account operation into an error,
    /// and never wipes known quota when the probe is unavailable.
    pub async fn sync_quota(&self, account_id: &str) {
        let Ok(account) = self.get_account(account_id).await else {
            return;
        };
        if account.provider != "codex" {
            return;
        }
        let Ok(payload) = Self::payload_for(&account) else {
            return;
        };
        let Ok(provider) = self.registry.provider_for_name(&account.provider) else {
            return;
        };
        let Ok(Some(quota)) = provider.fetch_quota(&account, &payload).await else {
            return;
        };
        if let Err(error) = self.repository.update_quota(account_id, Some(&quota)).await {
            tracing::warn!(
                account_id,
                "failed to persist auth account quota state: {error}"
            );
        }
    }

    /// Perform one serialized maintenance pass.  It intentionally works from
    /// persisted accounts only: active credentials are refreshed only when
    /// close to expiry, while invalid credentials are retried after their
    /// persisted backoff.  A failure on one account never prevents the next
    /// account from being considered.
    pub async fn run_maintenance_cycle(&self) {
        let accounts = match self.repository.list_auth_accounts().await {
            Ok(accounts) => accounts,
            Err(_) => {
                tracing::warn!("failed to load auth accounts for maintenance");
                return;
            }
        };
        let now = self.clock.now();

        for account in accounts {
            if account.disabled != 0 {
                continue;
            }
            let payload = match Self::payload_for(&account) {
                Ok(payload) => payload,
                Err(_) => {
                    tracing::warn!(account_id = %account.id, "skipping auth account with invalid credential payload");
                    continue;
                }
            };
            let refresh_result = match account.status.as_str() {
                "active" => {
                    // Every active account syncs models on the 12h cycle.  The
                    // refresh inside `sync_models` is lazy: a fresh token skips
                    // it, a near-expiry token is refreshed first, and a refresh
                    // failure aborts before any model request (preserving the
                    // previous snapshot).  This keeps the model list — and thus
                    // routeability of new models — fresh even for imported /
                    // long-lived tokens (ADR-8, design §7 step 3).
                    if let Err(error) = self.sync_models(&account.id).await {
                        tracing::warn!(account_id = %account.id, "auth account model sync failed during maintenance: {error}");
                    }
                    continue;
                }
                "invalid"
                    if Self::has_refresh_token(&payload)
                        && Self::retry_is_due(account.next_retry_after.as_deref(), now) =>
                {
                    self.force_refresh_account(&account.id).await
                }
                _ => continue,
            };

            if refresh_result.is_err() {
                self.schedule_maintenance_retry(&account.id).await;
                continue;
            }

            if self.sync_models(&account.id).await.is_err() {
                // `sync_models` writes only after a successful provider result,
                // so an error naturally preserves the previous model snapshot.
                tracing::warn!(account_id = %account.id, "auth account model sync failed during maintenance");
            }
        }
    }

    /// Dispatch with a fresh persisted payload.  Provider-specific HTTP policy
    /// remains in the implementation; callers never parse `payload_json`.
    pub async fn outbound(
        &self,
        account_id: &str,
        body: &serde_json::Value,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<reqwest::Response, ProviderError> {
        // Refresh through the summary-only API, then load raw credentials only
        // inside this module for the provider call.
        if let Err(error) = self.refresh_account(account_id).await {
            // A due refresh is part of preparing this account for outbound use.
            // Never leave a rejected credential in the active route pool, and
            // keep the maintenance retry cadence consistent with its failure path.
            self.schedule_maintenance_retry(account_id).await;
            return Err(error);
        }
        let response = self
            .send_with_persisted_account(account_id, body, headers)
            .await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            self.persist_quota_if_present(account_id, &response).await;
            return Ok(response);
        }

        // A 401 is the one and only internal retry.  The force refresh shares
        // the same per-account lock used by the lazy path, then re-reads the
        // persisted payload before retrying so rotated refresh tokens cannot be
        // overwritten by concurrent callers.
        if self.force_refresh_account(account_id).await.is_err() {
            // Rejected credentials get the same backoff as a failed lazy
            // refresh, so the maintenance loop does not hammer the provider
            // on the very next pass.
            self.schedule_maintenance_retry(account_id).await;
            return Err(ProviderError::Unauthorized);
        }
        let retry = self
            .send_with_persisted_account(account_id, body, headers)
            .await?;
        if retry.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.schedule_maintenance_retry(account_id).await;
            return Err(ProviderError::Unauthorized);
        }
        self.persist_quota_if_present(account_id, &retry).await;
        Ok(retry)
    }

    async fn send_with_persisted_account(
        &self,
        account_id: &str,
        body: &serde_json::Value,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<reqwest::Response, ProviderError> {
        let account = self.get_account(account_id).await?;
        let payload = Self::payload_for(&account)?;
        let provider = self.registry.provider_for_name(&account.provider)?;
        provider
            .outbound(ProviderRequest {
                account: &account,
                payload: &payload,
                body,
                headers,
            })
            .await
    }

    async fn schedule_maintenance_retry(&self, account_id: &str) {
        let next_retry_after = (self.clock.now() + Duration::hours(12)).to_rfc3339();
        if self
            .repository
            .mark_invalid(account_id, Some(&next_retry_after))
            .await
            .is_err()
        {
            tracing::warn!(
                account_id,
                "failed to schedule auth account maintenance retry"
            );
        }
    }

    async fn persist_quota_if_present(&self, account_id: &str, response: &reqwest::Response) {
        let Ok(account) = self.get_account(account_id).await else {
            return;
        };
        if account.provider != "codex" {
            return;
        }
        let previous = match account.quota_state() {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(account_id, "invalid persisted auth quota state");
                None
            }
        };
        let Some(quota) = crate::auth_provider::codex_backend::quota_from_headers(
            response.headers(),
            response.status(),
            previous.as_ref(),
            self.clock.now(),
        ) else {
            return;
        };
        if self
            .repository
            .update_quota(account_id, Some(&quota))
            .await
            .is_err()
        {
            // Quota observability must never turn an already successful upstream
            // response into a request failure.
            tracing::warn!(account_id, "failed to persist auth account quota state");
        }
    }

    async fn get_account(&self, account_id: &str) -> Result<AuthAccount, ProviderError> {
        self.repository
            .get_auth_account(account_id)
            .await
            .map_err(|_| ProviderError::Storage)
    }

    fn payload_for(account: &AuthAccount) -> Result<ProviderPayload, ProviderError> {
        serde_json::from_str(&account.payload_json)
            .map(ProviderPayload::new)
            .map_err(|_| ProviderError::InvalidPayload)
    }

    fn needs_refresh(payload: &ProviderPayload, refresh_before: DateTime<Utc>) -> bool {
        payload
            .expires_at()
            .is_none_or(|expires_at| expires_at <= refresh_before)
    }

    fn has_refresh_token(payload: &ProviderPayload) -> bool {
        payload
            .as_value()
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
    }

    fn retry_is_due(next_retry_after: Option<&str>, now: DateTime<Utc>) -> bool {
        next_retry_after
            .map(|value| {
                DateTime::parse_from_rfc3339(value)
                    .map(|value| value.with_timezone(&Utc) <= now)
                    .unwrap_or(false)
            })
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use reqwest::header::HeaderMap;
    use serde_json::json;

    use super::*;
    use crate::auth_provider::{LoginResult, Provider, ProviderModels, RefreshedPayload};
    use crate::db::models::QuotaState;

    const ACCESS: &str = "fixture-access-token";
    const REFRESH: &str = "fixture-refresh-token";
    const ID: &str = "fixture-id-token";

    struct FixedClock(DateTime<Utc>);
    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    struct FakeProvider {
        refreshes: AtomicUsize,
        quota_hits: AtomicUsize,
        operations: AtomicUsize,
        models_fail: bool,
        refresh_fails: bool,
        quota_fail: bool,
    }
    #[async_trait]
    impl Provider for FakeProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Codex
        }
        async fn login(&self, _: &dyn LoginRuntime) -> Result<LoginResult, ProviderError> {
            self.operations.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::LoginFailed)
        }
        async fn import(&self, _: &[u8]) -> Result<LoginResult, ProviderError> {
            self.operations.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::ImportFailed)
        }
        async fn refresh(&self, _: &ProviderPayload) -> Result<RefreshedPayload, ProviderError> {
            self.operations.fetch_add(1, Ordering::SeqCst);
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            if self.refresh_fails {
                return Err(ProviderError::Unauthorized);
            }
            Ok(RefreshedPayload {
                payload: ProviderPayload::new(
                    json!({"access_token": "new-access", "refresh_token": REFRESH, "id_token": ID, "expires_at": "2026-08-09T02:00:00Z"}),
                ),
                last_refreshed_at: Some("2026-08-09T00:00:00Z".into()),
                next_refresh_after: None,
                next_retry_after: None,
            })
        }
        async fn outbound(
            &self,
            _: ProviderRequest<'_>,
        ) -> Result<reqwest::Response, ProviderError> {
            self.operations.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::Retryable)
        }
        async fn list_models(
            &self,
            _account: &crate::db::models::AuthAccount,
            _payload: &ProviderPayload,
        ) -> Result<ProviderModels, ProviderError> {
            self.operations.fetch_add(1, Ordering::SeqCst);
            if self.models_fail {
                return Err(ProviderError::Retryable);
            }
            Ok(vec![])
        }
        async fn fetch_quota(
            &self,
            _account: &crate::db::models::AuthAccount,
            _payload: &ProviderPayload,
        ) -> Result<Option<QuotaState>, ProviderError> {
            self.operations.fetch_add(1, Ordering::SeqCst);
            self.quota_hits.fetch_add(1, Ordering::SeqCst);
            if self.quota_fail {
                return Err(ProviderError::Retryable);
            }
            Ok(Some(QuotaState {
                version: 1,
                exceeded: false,
                reason: None,
                next_recover_at: None,
                backoff_level: 0,
                limits: vec![crate::db::models::QuotaLimit {
                    limit_id: "codex".into(),
                    limit_name: None,
                    primary: Some(crate::db::models::QuotaWindow {
                        used_percent: Some(25.0),
                        window_minutes: Some(10_080),
                        reset_at: None,
                    }),
                    secondary: None,
                    credits: None,
                }],
            }))
        }
    }

    async fn repository() -> Arc<Repository> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        Arc::new(Repository::new(pool))
    }

    async fn account(repository: &Repository) -> AuthAccount {
        repository.upsert_by_provider_account_id(&AuthAccountUpsert {
            provider: "codex".into(), label: "Fixture".into(), account_id: "account-1".into(), attributes: json!({}),
            payload: json!({"access_token": ACCESS, "refresh_token": REFRESH, "id_token": ID, "expires_at": "2026-08-09T00:01:00Z"}),
            last_refreshed_at: None, next_refresh_after: None, next_retry_after: None,
        }).await.unwrap()
    }

    #[tokio::test]
    async fn concurrent_due_refreshes_are_single_flight_and_re_read_the_new_payload() {
        let repository = repository().await;
        let account = account(&repository).await;
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: false,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake.clone());
        let service = Arc::new(AuthService::with_clock(
            repository,
            registry,
            Arc::new(FixedClock("2026-08-09T00:00:00Z".parse().unwrap())),
        ));

        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let service = service.clone();
            let id = account.id.clone();
            calls.spawn(async move { service.refresh_account(&id).await.unwrap() });
        }
        let mut payloads = Vec::new();
        while let Some(result) = calls.join_next().await {
            payloads.push(result.unwrap().expires_at);
        }
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
        assert!(payloads
            .iter()
            .all(|expires_at| expires_at.as_deref() == Some("2026-08-09T02:00:00Z")));
    }

    #[tokio::test]
    async fn due_model_sync_probes_quota_once_after_refresh() {
        let repository = repository().await;
        let account = account(&repository).await;
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: false,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake.clone());
        let service = AuthService::with_clock(
            repository,
            registry,
            Arc::new(FixedClock("2026-08-09T00:00:00Z".parse().unwrap())),
        );

        service.sync_models(&account.id).await.unwrap();

        assert_eq!(fake.quota_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_provider_fails_before_any_provider_operation() {
        let repository = repository().await;
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: false,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake.clone());
        let service = AuthService::new(repository, registry);
        let error = service
            .import(ProviderKind::from("unknown"), b"fixture")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ProviderError::UnknownProvider {
                provider: "unknown".into()
            }
        );
        assert_eq!(fake.operations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_errors_are_safe_to_display_and_debug() {
        let error = ProviderError::UnsupportedFeatures {
            pointer: "/metadata".into(),
        };
        let rendered = format!("{error} {error:?}");
        for secret in [ACCESS, REFRESH, ID] {
            assert!(!rendered.contains(secret));
        }
    }

    #[tokio::test]
    async fn account_summary_never_serializes_provider_credentials() {
        let repository = repository().await;
        let account = account(&repository).await;
        let summary = AuthAccountSummary::from_account(&account).unwrap();
        let rendered = serde_json::to_string(&summary).unwrap();
        for secret in [ACCESS, REFRESH, ID] {
            assert!(!rendered.contains(secret));
        }
        assert!(!rendered.contains("payload_json"));
    }

    #[tokio::test]
    async fn sync_models_refreshes_a_due_account_before_syncing() {
        let repository = repository().await;
        // expires_at 2026-08-09T00:01:00Z vs fixed now 2026-08-09T00:00:00Z:
        // refresh_before = now + 5min = 00:05Z, so 00:01Z is due for refresh.
        let account = account(&repository).await;
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: false,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake.clone());
        let service = AuthService::with_clock(
            repository.clone(),
            registry,
            Arc::new(FixedClock("2026-08-09T00:00:00Z".parse().unwrap())),
        );

        let summary = service.sync_models(&account.id).await.unwrap();
        // The due account is refreshed once before the model snapshot is taken.
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
        // One refresh + one list_models + one quota probe: the listing runs
        // exactly once on the refreshed payload, then the quota probe follows.
        assert_eq!(fake.operations.load(Ordering::SeqCst), 3);
        assert_eq!(summary.expires_at.as_deref(), Some("2026-08-09T02:00:00Z"));
        let stored = repository.get_auth_account(&account.id).await.unwrap();
        assert_eq!(
            stored.payload_json,
            serde_json::to_string(&json!({"access_token":"new-access","refresh_token":REFRESH,"id_token":ID,"expires_at":"2026-08-09T02:00:00Z"})).unwrap()
        );
    }

    #[tokio::test]
    async fn failed_model_sync_keeps_the_existing_snapshot_and_timestamp() {
        let repository = repository().await;
        let account = account(&repository).await;
        let old_models = ModelStates {
            version: 1,
            models: vec![crate::db::models::ModelState {
                id: "gpt-old".into(),
                status: "available".into(),
                unavailable: false,
                next_retry_after: None,
                last_error: None,
            }],
        };
        let old_sync = "2026-08-08T00:00:00Z";
        repository
            .update_models_if_success(&account.id, &old_models, old_sync)
            .await
            .unwrap();
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: true,
            refresh_fails: false,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake);
        let service = AuthService::new(repository.clone(), registry);
        assert_eq!(
            service.sync_models(&account.id).await.unwrap_err(),
            ProviderError::Retryable
        );
        let stored = repository.get_auth_account(&account.id).await.unwrap();
        assert_eq!(stored.model_states().unwrap(), old_models);
        assert_eq!(stored.last_models_sync_at.as_deref(), Some(old_sync));
    }

    #[tokio::test]
    async fn sync_quota_persists_probe_result() {
        let repository = repository().await;
        let account = account(&repository).await;
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: false,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake);
        let service = AuthService::new(repository.clone(), registry);

        service.sync_quota(&account.id).await;
        let stored = repository.get_auth_account(&account.id).await.unwrap();
        let quota = stored.quota_state().unwrap().unwrap();
        assert!(!quota.exceeded);
        assert_eq!(quota.limits.len(), 1);
        assert_eq!(
            quota.limits[0].primary.as_ref().unwrap().window_minutes,
            Some(10_080)
        );
    }

    #[tokio::test]
    async fn sync_quota_silently_preserves_existing_on_probe_failure() {
        let repository = repository().await;
        let account = account(&repository).await;
        // Persist an existing quota first.
        let existing = QuotaState {
            version: 1,
            exceeded: true,
            reason: Some("quota".into()),
            next_recover_at: Some("2026-08-10T00:00:00Z".into()),
            backoff_level: 2,
            limits: vec![],
        };
        repository
            .update_quota(&account.id, Some(&existing))
            .await
            .unwrap();
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: false,
            quota_fail: true,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake);
        let service = AuthService::new(repository.clone(), registry);

        // The failed probe is silent: the account still reports the old quota
        // rather than being wiped or erroring.
        service.sync_quota(&account.id).await;
        let stored = repository.get_auth_account(&account.id).await.unwrap();
        let quota = stored.quota_state().unwrap().unwrap();
        assert!(quota.exceeded);
        assert_eq!(quota.backoff_level, 2);
        assert_eq!(
            quota.next_recover_at.as_deref(),
            Some("2026-08-10T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn failed_lazy_refresh_invalidates_account_before_any_outbound_request() {
        let repository = repository().await;
        let account = account(&repository).await;
        repository
            .update_models_if_success(
                &account.id,
                &ModelStates {
                    version: 1,
                    models: vec![crate::db::models::ModelState {
                        id: "gpt-test".into(),
                        status: "available".into(),
                        unavailable: false,
                        next_retry_after: None,
                        last_error: None,
                    }],
                },
                "2026-08-09T00:00:00Z",
            )
            .await
            .unwrap();
        let fake = Arc::new(FakeProvider {
            refreshes: AtomicUsize::new(0),
            quota_hits: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            models_fail: false,
            refresh_fails: true,
            quota_fail: false,
        });
        let mut registry = ProviderRegistry::new();
        registry.register(fake.clone());
        let service = AuthService::with_clock(
            repository.clone(),
            registry,
            Arc::new(FixedClock("2026-08-09T00:00:00Z".parse().unwrap())),
        );

        assert_eq!(
            service
                .outbound(
                    &account.id,
                    &json!({"model": "gpt-test"}),
                    &HeaderMap::new()
                )
                .await
                .unwrap_err(),
            ProviderError::Unauthorized
        );
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(fake.operations.load(Ordering::SeqCst), 1);

        let stored = repository.get_auth_account(&account.id).await.unwrap();
        assert_eq!(stored.status, "invalid");
        assert_eq!(
            stored.next_retry_after.as_deref(),
            Some("2026-08-09T12:00:00+00:00")
        );
        assert!(repository
            .list_route_accounts("2026-08-09T00:00:00Z")
            .await
            .unwrap()
            .is_empty());
    }
}
