//! Tauri-facing Auth Account commands.
//!
//! This is the final boundary before data reaches the webview.  Keep the DTOs
//! deliberately explicit: database credential JSON and all OAuth token fields
//! must remain on the native side of this module.

use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

use crate::{
    auth_provider::{
        codex_login::{CodexLogin, TauriLoginRuntime, CODEX_IMPORT_NOTICE},
        AuthAccountSummary, LoginRuntime, ProviderError, ProviderKind, ProviderPayload,
    },
    db::{
        models::{ModelState, QuotaState},
        repository::Repository,
    },
    AppState,
};

/// A carefully projected account representation.  Do not replace this with
/// `AuthAccount` or `AuthAccountSummary`: both can carry fields unsuitable for
/// a renderer-facing contract.
#[derive(Debug, Clone, Serialize)]
pub struct AuthAccountDto {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub account_id: String,
    pub status: String,
    pub disabled: bool,
    pub priority: i64,
    pub weight: i64,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub models: Vec<ModelState>,
    pub quota: Option<QuotaState>,
    pub model_mapping: serde_json::Value,
    pub expires_at: Option<String>,
    #[serde(rename = "hasRefreshToken")]
    pub has_refresh_token: bool,
    pub last_refreshed_at: Option<String>,
    pub last_models_sync_at: Option<String>,
    pub next_refresh_after: Option<String>,
    pub next_retry_after: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl TryFrom<AuthAccountSummary> for AuthAccountDto {
    type Error = ProviderError;

    fn try_from(value: AuthAccountSummary) -> Result<Self, Self::Error> {
        let email = value
            .attributes
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let plan_type = value
            .attributes
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            id: value.id,
            provider: value.provider,
            label: value.label,
            account_id: value.account_id,
            status: value.status,
            disabled: value.disabled,
            priority: value.priority,
            weight: value.weight,
            email,
            plan_type,
            models: value.models.models,
            quota: value.quota,
            model_mapping: value.model_mapping,
            expires_at: value.expires_at,
            has_refresh_token: value.has_refresh_token,
            last_refreshed_at: value.last_refreshed_at,
            last_models_sync_at: value.last_models_sync_at,
            next_refresh_after: value.next_refresh_after,
            next_retry_after: value.next_retry_after,
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthMutationResult {
    pub account: AuthAccountDto,
    /// Set when persistence succeeded but the requested follow-up operation
    /// (currently initial model sync) did not.
    pub warning: Option<String>,
    /// Import-specific non-secret operational notice.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthLogoutResult {
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthExportResult {
    pub path: String,
    pub backup_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthQuotaStatus {
    pub quota: Option<QuotaState>,
    pub available: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthUpdateInput {
    pub id: String,
    pub label: String,
    pub priority: i64,
    pub weight: i64,
    pub model_mapping: Option<serde_json::Value>,
}

/// Renderer-safe interactive-login session.  It intentionally contains no
/// callback URL, OAuth code, or credential material.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthLoginStart {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthLoginSessionStatus {
    pub session_id: String,
    /// pending | saving | syncing | succeeded | cancelled | failed
    pub state: String,
    /// The UI maps this to a concrete progress item; it is never time-based.
    pub step: Option<String>,
    pub result: Option<AuthMutationResult>,
    /// A stable, non-secret failure category for retry guidance.
    pub error_code: Option<String>,
    pub error: Option<String>,
}

struct LoginSession {
    cancel: watch::Sender<bool>,
    status: AuthLoginSessionStatus,
}

/// Process-local tombstones make repeated cancel safe and prevent a late
/// callback/token exchange from reviving a cancelled session.
pub struct LoginSessions {
    sessions: Mutex<HashMap<String, LoginSession>>,
}

struct SessionLoginRuntime {
    inner: TauriLoginRuntime,
    sessions: Arc<LoginSessions>,
    session_id: String,
}

#[async_trait::async_trait]
impl LoginRuntime for SessionLoginRuntime {
    async fn open_browser(&self, url: &str) -> Result<(), ProviderError> {
        self.inner.open_browser(url).await?;
        // This is an actual opener success, not a timer-driven estimate.
        let _ = self.sessions.set_step(&self.session_id, "callback").await;
        Ok(())
    }
}

impl LoginSessions {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start(&self) -> (String, watch::Receiver<bool>) {
        let session_id = Uuid::new_v4().to_string();
        let (cancel, receiver) = watch::channel(false);
        let status = AuthLoginSessionStatus {
            session_id: session_id.clone(),
            state: "pending".into(),
            step: Some("listener".into()),
            result: None,
            error_code: None,
            error: None,
        };
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), LoginSession { cancel, status });
        (session_id, receiver)
    }

    pub async fn status(&self, session_id: &str) -> Result<AuthLoginSessionStatus, String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|session| session.status.clone())
            .ok_or_else(|| "Auth login session not found".to_owned())
    }

    pub async fn cancel(&self, session_id: &str) -> Result<AuthLoginSessionStatus, String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| "Auth login session not found".to_owned())?;
        // Terminal records are tombstones: DELETE/cancel is deliberately idempotent.
        if matches!(
            session.status.state.as_str(),
            "succeeded" | "cancelled" | "failed" | "saving" | "syncing"
        ) {
            return Ok(session.status.clone());
        }
        let _ = session.cancel.send(true);
        session.status.state = "cancelled".into();
        session.status.step = None;
        session.status.error_code = Some("cancelled".into());
        session.status.error = Some("登录已取消，可以重新开始。".into());
        Ok(session.status.clone())
    }

    async fn set_step(&self, session_id: &str, step: &str) -> bool {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return false;
        };
        if session.status.state != "pending" || *session.cancel.borrow() {
            return false;
        }
        session.status.step = Some(step.to_owned());
        true
    }

    /// This transition is the commit gate: cancellation and entering the DB
    /// write phase are serialized under one mutex.
    async fn begin_save(&self, session_id: &str) -> bool {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return false;
        };
        if session.status.state != "pending" || *session.cancel.borrow() {
            return false;
        }
        session.status.state = "saving".into();
        session.status.step = Some("saving".into());
        true
    }

    async fn set_syncing(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(session_id) {
            if session.status.state == "saving" {
                session.status.state = "syncing".into();
                session.status.step = Some("syncing".into());
            }
        }
    }

    async fn finish(&self, session_id: &str, result: Result<AuthMutationResult, ProviderError>) {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        if session.status.state == "cancelled" {
            return;
        }
        match result {
            Ok(result) => {
                session.status.state = "succeeded".into();
                session.status.step = None;
                session.status.result = Some(result);
            }
            Err(error) => {
                session.status.state = "failed".into();
                session.status.step = None;
                session.status.error_code = Some(login_error_code(&error).into());
                session.status.error = Some(login_error_message(&error).into());
            }
        }
    }
}

fn login_error_code(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::LoginCancelled => "cancelled",
        ProviderError::LoginTimeout => "timeout",
        ProviderError::BrowserOpenFailed => "browser_open",
        ProviderError::CallbackFailed => "callback_state",
        ProviderError::TokenExchangeFailed => "token_exchange",
        _ => "login_failed",
    }
}

fn login_error_message(error: &ProviderError) -> &'static str {
    match login_error_code(error) {
        "cancelled" => "登录已取消，可以重新开始。",
        "timeout" => "等待浏览器授权超时，请重新开始登录。",
        "browser_open" => "无法打开浏览器授权页，请检查默认浏览器后重试。",
        "callback_state" => "授权回调无效或被拒绝，请重新开始登录。",
        "token_exchange" => "授权完成，但令牌交换失败，请重新开始登录。",
        _ => "登录未完成，请检查浏览器授权后重试。",
    }
}

fn safe_error(_: ProviderError) -> String {
    // ProviderError::Display is already redacted.  Keep this extra command
    // boundary stable so filesystem/SQL/OAuth diagnostics never cross it.
    "Auth operation failed".to_owned()
}

fn storage_error() -> String {
    "Auth account storage operation failed".to_owned()
}

fn validate_account_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("Auth account id is required".to_owned());
    }
    Ok(())
}

fn provider_kind(provider: Option<String>) -> Result<ProviderKind, String> {
    let provider = provider.unwrap_or_else(|| "codex".to_owned());
    if provider.trim() != "codex" {
        return Err("Unsupported auth provider".to_owned());
    }
    Ok(ProviderKind::Codex)
}

fn validate_update(input: &AuthUpdateInput) -> Result<(), String> {
    validate_account_id(&input.id)?;
    if input.label.trim().is_empty() {
        return Err("Auth account label must not be empty".to_owned());
    }
    if input.priority < 0 {
        return Err("Auth account priority must be at least zero".to_owned());
    }
    if input.weight < 1 {
        return Err("Auth account weight must be at least one".to_owned());
    }
    Ok(())
}

fn dto_from_account(account: crate::db::models::AuthAccount) -> Result<AuthAccountDto, String> {
    AuthAccountSummary::from_account(&account)
        .and_then(AuthAccountDto::try_from)
        .map_err(safe_error)
}

async fn sync_after_login(
    service: &crate::auth_provider::service::AuthService,
    summary: AuthAccountSummary,
    notice: Option<String>,
) -> Result<AuthMutationResult, String> {
    let account_id = summary.id.clone();
    let account = AuthAccountDto::try_from(summary).map_err(safe_error)?;
    let warning = match service.sync_models(&account_id).await {
        Ok(_) => None,
        Err(_) => Some(
            "Account saved, but model sync failed; it will not route until sync succeeds."
                .to_owned(),
        ),
    };
    Ok(AuthMutationResult {
        account,
        warning,
        notice,
    })
}

async fn persist_login_result(
    repository: &Repository,
    result: crate::auth_provider::LoginResult,
) -> Result<AuthAccountSummary, ProviderError> {
    let account = repository
        .upsert_by_provider_account_id(&crate::db::models::AuthAccountUpsert {
            provider: ProviderKind::Codex.to_string(),
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

async fn run_codex_login_session(
    sessions: Arc<LoginSessions>,
    session_id: String,
    mut cancellation: watch::Receiver<bool>,
    app: tauri::AppHandle,
    db: Arc<crate::db::Database>,
    service: Arc<crate::auth_provider::service::AuthService>,
) {
    let runtime = SessionLoginRuntime {
        inner: TauriLoginRuntime::new(app),
        sessions: sessions.clone(),
        session_id: session_id.clone(),
    };
    let login = CodexLogin::new();
    let _ = sessions.set_step(&session_id, "browser").await;
    let login_result = login.login_cancellable(&runtime, &mut cancellation).await;
    let result = match login_result {
        Ok(login_result) => {
            // The second half of the cancellation guarantee.  A callback or
            // token response alone can never create an account after cancel.
            if !sessions.begin_save(&session_id).await {
                Err(ProviderError::LoginCancelled)
            } else {
                let repository = Repository::new(db.pool.clone());
                match persist_login_result(&repository, login_result).await {
                    Ok(summary) => {
                        sessions.set_syncing(&session_id).await;
                        sync_after_login(&service, summary, None)
                            .await
                            .map_err(|_| ProviderError::Storage)
                    }
                    Err(error) => Err(error),
                }
            }
        }
        Err(error) => Err(error),
    };
    sessions.finish(&session_id, result).await;
}

async fn logout_local(repository: &Repository, id: &str) -> Result<AuthLogoutResult, String> {
    validate_account_id(id)?;
    // Confirm the record exists before reporting a successful local deletion.
    repository
        .get_auth_account(id)
        .await
        .map_err(|_| storage_error())?;
    // ADR-38: deletion is local-only — remove the row (payload and model
    // snapshot live in it), no provider revoke endpoint is called.
    repository
        .delete_auth_account(id)
        .await
        .map_err(|_| storage_error())?;
    Ok(AuthLogoutResult { deleted: true })
}

fn quota_is_available(quota: Option<&QuotaState>, now: DateTime<Utc>) -> bool {
    let Some(quota) = quota else {
        return true;
    };
    if !quota.exceeded {
        return true;
    }
    quota
        .next_recover_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|recover_at| recover_at.with_timezone(&Utc) <= now)
}

fn import_path(path: Option<String>) -> Result<PathBuf, String> {
    match path {
        Some(path) if !path.trim().is_empty() => Ok(PathBuf::from(path)),
        _ => CodexLogin::default_auth_json_path().map_err(safe_error),
    }
}

/// Return the default Codex CLI auth file path for the native file picker.
/// Reads no secrets; path logic stays in `CodexLogin::default_auth_json_path`.
#[tauri::command]
pub async fn auth_default_import_path() -> Result<String, String> {
    CodexLogin::default_auth_json_path()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(safe_error)
}

#[tauri::command]
pub async fn auth_accounts_list(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<AuthAccountDto>, String> {
    let repository = Repository::new(state.db.pool.clone());
    repository
        .list_auth_accounts()
        .await
        .map_err(|_| storage_error())?
        .into_iter()
        .map(dto_from_account)
        .collect()
}

#[tauri::command]
pub async fn auth_login(
    provider: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthMutationResult, String> {
    let kind = provider_kind(Some(provider))?;
    let runtime = TauriLoginRuntime::new(app);
    let summary = state
        .auth_service
        .login(kind, &runtime)
        .await
        .map_err(safe_error)?;
    sync_after_login(&state.auth_service, summary, None).await
}

#[tauri::command]
pub async fn auth_login_start(
    provider: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthLoginStart, String> {
    provider_kind(Some(provider))?;
    let (session_id, cancellation) = state.login_sessions.start().await;
    let sessions = state.login_sessions.clone();
    let db = state.db.clone();
    let service = state.auth_service.clone();
    let task_id = session_id.clone();
    tauri::async_runtime::spawn(async move {
        run_codex_login_session(sessions, task_id, cancellation, app, db, service).await;
    });
    Ok(AuthLoginStart { session_id })
}

#[tauri::command]
pub async fn auth_login_status(
    session_id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthLoginSessionStatus, String> {
    state.login_sessions.status(&session_id).await
}

#[tauri::command]
pub async fn auth_login_cancel(
    session_id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthLoginSessionStatus, String> {
    state.login_sessions.cancel(&session_id).await
}

#[tauri::command]
pub async fn auth_login_import(
    provider: Option<String>,
    path: Option<String>,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthMutationResult, String> {
    let kind = provider_kind(provider)?;
    let path = import_path(path)?;
    let bytes = fs::read(path).map_err(|_| "Unable to read auth file".to_owned())?;
    let summary = state
        .auth_service
        .import(kind, &bytes)
        .await
        .map_err(safe_error)?;
    sync_after_login(
        &state.auth_service,
        summary,
        Some(CODEX_IMPORT_NOTICE.to_owned()),
    )
    .await
}

#[tauri::command]
pub async fn auth_logout(
    id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthLogoutResult, String> {
    let repository = Repository::new(state.db.pool.clone());
    // ADR-38: v1 deletion is local-only, no provider revoke endpoint.
    logout_local(&repository, &id).await
}

#[tauri::command]
pub async fn auth_refresh_token(
    id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthAccountDto, String> {
    validate_account_id(&id)?;
    match state.auth_service.force_refresh_account(&id).await {
        Ok(summary) => AuthAccountDto::try_from(summary).map_err(safe_error),
        Err(error) => {
            let repository = Repository::new(state.db.pool.clone());
            let _ = repository.mark_invalid(&id, None).await;
            Err(safe_error(error))
        }
    }
}

#[tauri::command]
pub async fn auth_sync_models(
    id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthAccountDto, String> {
    validate_account_id(&id)?;
    state
        .auth_service
        .sync_models(&id)
        .await
        .map_err(safe_error)
        .and_then(|summary| AuthAccountDto::try_from(summary).map_err(safe_error))
}

#[tauri::command]
pub async fn auth_export_json(
    id: String,
    path: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthExportResult, String> {
    validate_account_id(&id)?;
    let path = PathBuf::from(path);
    let repository = Repository::new(state.db.pool.clone());
    let account = repository
        .get_auth_account(&id)
        .await
        .map_err(|_| storage_error())?;
    if account.provider != "codex" {
        return Err("Unsupported auth provider".to_owned());
    }
    // The raw credential JSON is decoded only in this native command and is
    // handed immediately to the provider-specific atomic exporter.
    let payload = serde_json::from_str(&account.payload_json)
        .map(ProviderPayload::new)
        .map_err(|_| safe_error(ProviderError::InvalidPayload))?;
    let result = CodexLogin::write_auth_json(&path, &payload).map_err(safe_error)?;
    Ok(AuthExportResult {
        path: result.path.to_string_lossy().into_owned(),
        backup_path: result
            .backup_path
            .map(|path| path.to_string_lossy().into_owned()),
    })
}

#[tauri::command]
pub async fn auth_toggle(
    id: String,
    disabled: bool,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthAccountDto, String> {
    validate_account_id(&id)?;
    let repository = Repository::new(state.db.pool.clone());
    repository
        .update_auth_account_disabled(&id, disabled)
        .await
        .map_err(|_| storage_error())?;
    dto_from_account(
        repository
            .get_auth_account(&id)
            .await
            .map_err(|_| storage_error())?,
    )
}

#[tauri::command]
pub async fn auth_quota_status(
    id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthQuotaStatus, String> {
    validate_account_id(&id)?;
    let repository = Repository::new(state.db.pool.clone());
    let account = repository
        .get_auth_account(&id)
        .await
        .map_err(|_| storage_error())?;
    let quota = account
        .quota_state()
        .map_err(|_| safe_error(ProviderError::InvalidPayload))?;
    Ok(AuthQuotaStatus {
        available: quota_is_available(quota.as_ref(), Utc::now()),
        quota,
    })
}

#[tauri::command]
pub async fn auth_update(
    input: AuthUpdateInput,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AuthAccountDto, String> {
    // Validate here, before creating any repository call, so invalid user
    // values cannot cause even a no-op database write.
    validate_update(&input)?;
    let model_mapping_json = input
        .model_mapping
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|| "{}".to_string());
    let repository = Repository::new(state.db.pool.clone());
    repository
        .update_auth_account(&input.id, input.label.trim(), input.priority, input.weight, &model_mapping_json)
        .await
        .map_err(|_| storage_error())?;
    dto_from_account(
        repository
            .get_auth_account(&input.id)
            .await
            .map_err(|_| storage_error())?,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::db::{models::AuthAccountUpsert, repository::Repository};

    const ACCESS: &str = "fixture-access-token";
    const REFRESH: &str = "fixture-refresh-token";
    const ID_TOKEN: &str = "fixture-id-token";

    fn account_fixture() -> crate::db::models::AuthAccount {
        crate::db::models::AuthAccount {
            id: "account-1".into(), provider: "codex".into(), label: "Codex".into(),
            account_id: "provider-account-1".into(), status: "active".into(), disabled: 0,
            priority: 0, weight: 1, quota_json: None,
            model_states_json: json!({"version":1,"models":[]}).to_string(),
            attributes_json: json!({"email":"person@example.test","plan_type":"plus","ignored":"secret"}).to_string(),
            model_mapping_json: "{}".to_string(),
            payload_json: json!({"access_token":ACCESS,"refresh_token":REFRESH,"id_token":ID_TOKEN,"expires_at":"2030-01-01T00:00:00Z"}).to_string(),
            last_refreshed_at: None, last_models_sync_at: None, next_refresh_after: None,
            next_retry_after: None, created_at: "2026-01-01T00:00:00Z".into(), updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn auth_update_validation_rejects_invalid_values_before_storage() {
        for input in [
            AuthUpdateInput {
                id: "account-1".into(),
                label: "   ".into(),
                priority: 0,
                weight: 1,
                model_mapping: None,
            },
            AuthUpdateInput {
                id: "account-1".into(),
                label: "Codex".into(),
                priority: -1,
                weight: 1,
                model_mapping: None,
            },
            AuthUpdateInput {
                id: "account-1".into(),
                label: "Codex".into(),
                priority: 0,
                weight: 0,
                model_mapping: None,
            },
        ] {
            assert!(validate_update(&input).is_err());
        }
    }

    #[test]
    fn account_and_mutation_dtos_never_serialize_credentials_or_payload_names() {
        let dto = dto_from_account(account_fixture()).unwrap();
        let list = serde_json::to_string(&vec![dto.clone()]).unwrap();
        let mutation = serde_json::to_string(&AuthMutationResult {
            account: dto,
            warning: None,
            notice: None,
        })
        .unwrap();
        let logout = serde_json::to_string(&AuthLogoutResult { deleted: true }).unwrap();
        let export = serde_json::to_string(&AuthExportResult {
            path: "/tmp/auth.json".into(),
            backup_path: Some("/tmp/auth.json.bak".into()),
        })
        .unwrap();
        for encoded in [list, mutation, logout, export] {
            for forbidden in [
                ACCESS,
                REFRESH,
                ID_TOKEN,
                "access_token",
                "refresh_token",
                "id_token",
                "payload_json",
            ] {
                assert!(
                    !encoded.contains(forbidden),
                    "serialized response leaked {forbidden}"
                );
            }
        }
    }

    async fn test_repository() -> Repository {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        Repository::new(pool)
    }

    #[tokio::test]
    async fn logout_deletes_local_account_only() {
        let repository = test_repository().await;
        let account = repository
            .upsert_by_provider_account_id(&AuthAccountUpsert {
                provider: "codex".into(),
                label: "Codex".into(),
                account_id: "provider-1".into(),
                attributes: json!({}),
                payload: json!({"version":1}),
                last_refreshed_at: None,
                next_refresh_after: None,
                next_retry_after: None,
            })
            .await
            .unwrap();
        let result = logout_local(&repository, &account.id).await.unwrap();
        assert!(result.deleted);
        // Local-only deletion: no provider call, no warning surfaced (ADR-38).
        assert!(repository.get_auth_account(&account.id).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_session_is_idempotent_and_never_enters_persistence() {
        let sessions = LoginSessions::new();
        let (id, _receiver) = sessions.start().await;
        let first = sessions.cancel(&id).await.unwrap();
        let second = sessions.cancel(&id).await.unwrap();
        assert_eq!(first.state, "cancelled");
        assert_eq!(second.state, "cancelled");
        // This is the persistence commit gate used after callback/token work.
        assert!(!sessions.begin_save(&id).await);
        let encoded = serde_json::to_string(&second).unwrap();
        for forbidden in [
            ACCESS,
            REFRESH,
            ID_TOKEN,
            "access_token",
            "refresh_token",
            "id_token",
        ] {
            assert!(!encoded.contains(forbidden), "status leaked {forbidden}");
        }
    }

    #[tokio::test]
    async fn terminal_error_status_is_queryable_and_categorized() {
        let sessions = LoginSessions::new();
        let (id, _receiver) = sessions.start().await;
        sessions.finish(&id, Err(ProviderError::LoginTimeout)).await;
        let status = sessions.status(&id).await.unwrap();
        assert_eq!(status.state, "failed");
        assert_eq!(status.error_code.as_deref(), Some("timeout"));
        assert!(status
            .error
            .as_deref()
            .is_some_and(|message| !message.is_empty()));
    }
}
