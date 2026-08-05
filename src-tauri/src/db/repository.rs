use super::models::*;
use sqlx::SqlitePool;

/// Parse the stored JSON endpoint list back into a Vec, or None when empty/absent.
fn parse_eps(raw: &Option<String>) -> Option<Vec<String>> {
    let s = raw.as_deref()?;
    let parsed: Vec<String> = serde_json::from_str(s).unwrap_or_default();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

pub struct Repository {
    pool: SqlitePool,
}

impl Repository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ==================== Channel ====================

    pub async fn get_all_channels(&self) -> Result<Vec<Channel>, sqlx::Error> {
        sqlx::query_as::<_, Channel>(
            "SELECT * FROM channels ORDER BY priority DESC, created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_channel(&self, id: &str) -> Result<Channel, sqlx::Error> {
        sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn get_enabled_channels(&self) -> Result<Vec<Channel>, sqlx::Error> {
        sqlx::query_as::<_, Channel>(
            "SELECT * FROM channels WHERE status = 1 ORDER BY priority DESC, weight DESC",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Resolve the full identity to persist for a create/update, and the
    /// legacy dual-write pair (type, base_url).
    ///
    /// * New fields all present (protocol/provider/native_base_url/native
    ///   endpoints) => identity written from them, dual-write via
    ///   `new_to_legacy`, revision = max(current, 1).
    /// * Otherwise => live-infer from legacy fields; identity revision stays 0.
    fn plan_channel_identity(
        protocol: &Option<String>,
        provider: &Option<String>,
        native_base_url: &Option<String>,
        native_endpoints: &Option<Vec<String>>,
        current_revision: i64,
        legacy_type: &str,
        legacy_base_url: &str,
        config_json: &str,
    ) -> (
        crate::core::channel_identity::ChannelIdentity,
        String,
        String,
        String,
    ) {
        use crate::core::channel_identity::{
            resolve_channel_identity, ChannelIdentity, ChannelIdentityRow,
        };

        let protocol_ok = protocol
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let provider_ok = provider
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let base_ok = native_base_url
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let eps_ok = native_endpoints
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        // Determine which legacy fields to infer from. If the caller supplied a
        // legacy type/base_url (e.g. old frontend payload), use those; else the
        // current row values.
        let (identity, legacy_type_out, legacy_base_out) =
            if protocol_ok && provider_ok && base_ok && eps_ok {
                let identity = ChannelIdentity {
                    protocol: protocol.clone().unwrap_or_default(),
                    provider: provider.clone().unwrap_or_default(),
                    native_base_url: native_base_url.clone().unwrap_or_default(),
                    native_endpoints: native_endpoints.clone().unwrap_or_default(),
                    identity_revision: current_revision.max(1),
                    legacy_executor_override: None,
                    executor_kind: crate::core::channel_identity::derive_executor_kind(
                        protocol.as_deref().unwrap_or(""),
                    )
                    .to_string(),
                    inferred: false,
                };
                let (lt, lb) = crate::core::channel_identity::new_to_legacy(&identity);
                (identity, lt, lb)
            } else {
                // Legacy infer from the legacy fields (old payload or current row).
                let row = ChannelIdentityRow {
                    channel_type: legacy_type.to_string(),
                    base_url: legacy_base_url.to_string(),
                    config: serde_json::from_str(config_json)
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                    protocol: protocol.clone(),
                    provider: provider.clone(),
                    native_base_url: native_base_url.clone(),
                    native_endpoints: native_endpoints
                        .as_ref()
                        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())),
                    preset_revision: None,
                    identity_revision: 0,
                    legacy_executor_override: None,
                };
                let identity = resolve_channel_identity(&row);
                let lt = legacy_type.to_string();
                let lb = legacy_base_url.to_string();
                (identity, lt, lb)
            };

        let endpoints_json =
            serde_json::to_string(&identity.native_endpoints).unwrap_or_else(|_| "[]".to_string());
        (identity, legacy_type_out, legacy_base_out, endpoints_json)
    }

    pub async fn create_channel(&self, input: &CreateChannelInput) -> Result<Channel, sqlx::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_iso();
        let models = serde_json::to_string(&input.models).unwrap_or_else(|_| "[]".to_string());
        let config = input
            .config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());
        let model_mapping = input
            .model_mapping
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let (identity, legacy_type, legacy_base, endpoints_json) = Self::plan_channel_identity(
            &input.protocol,
            &input.provider,
            &input.native_base_url,
            &input.native_endpoints,
            0,
            &input.channel_type,
            &input.base_url,
            &config,
        );

        sqlx::query(
            "INSERT INTO channels (
                id, name, type, base_url, api_key, models, status, priority, weight,
                config, model_mapping, timeout_secs,
                protocol, provider, native_base_url, native_endpoints,
                preset_revision, identity_revision, legacy_executor_override,
                created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&legacy_type)
        .bind(&legacy_base)
        .bind(&input.api_key)
        .bind(&models)
        .bind(input.priority.unwrap_or(0))
        .bind(input.weight.unwrap_or(1))
        .bind(&config)
        .bind(&model_mapping)
        .bind(input.timeout_secs.unwrap_or(60))
        .bind(&identity.protocol)
        .bind(&identity.provider)
        .bind(&identity.native_base_url)
        .bind(&endpoints_json)
        .bind(&input.preset_revision)
        .bind(identity.identity_revision)
        .bind(&identity.legacy_executor_override)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        self.get_channel(&id).await
    }

    /// Import a channel with FULL business-field fidelity (T09).
    ///
    /// Deliberately NOT `create_channel`: that writer hard-codes `status = 1`
    /// and a default `timeout_secs`, which would silently corrupt round-trips
    /// of disabled/slow channels.  This narrow import-write API persists every
    /// business field verbatim (status/priority/weight/timeout_secs/config
    /// unknown keys/URL/key/models/array model_mapping) and copies the identity
    /// columns exactly as resolved by `commands::import_export`.
    pub async fn import_channel(&self, input: &ImportChannelInput) -> Result<Channel, sqlx::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_iso();
        let models = serde_json::to_string(&input.models).unwrap_or_else(|_| "[]".to_string());
        let config = serde_json::to_string(&input.config).unwrap_or_else(|_| "{}".to_string());
        let model_mapping =
            serde_json::to_string(&input.model_mapping).unwrap_or_else(|_| "{}".to_string());
        let endpoints_json = input
            .native_endpoints
            .as_ref()
            .map(|eps| serde_json::to_string(eps).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| "[]".to_string());

        sqlx::query(
            "INSERT INTO channels (
                id, name, type, base_url, api_key, models, status, priority, weight,
                config, model_mapping, timeout_secs,
                protocol, provider, native_base_url, native_endpoints,
                preset_revision, identity_revision, legacy_executor_override,
                created_at, updated_at, last_test_at, last_test_ok)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&input.channel_type)
        .bind(&input.base_url)
        .bind(&input.api_key)
        .bind(&models)
        .bind(input.status)
        .bind(input.priority)
        .bind(input.weight)
        .bind(&config)
        .bind(&model_mapping)
        .bind(input.timeout_secs)
        .bind(&input.protocol)
        .bind(&input.provider)
        .bind(&input.native_base_url)
        .bind(&endpoints_json)
        .bind(&input.preset_revision)
        .bind(input.identity_revision)
        .bind(&input.legacy_executor_override)
        .bind(&now)
        .bind(&now)
        .bind(&input.last_test_at)
        .bind(input.last_test_ok)
        .execute(&self.pool)
        .await?;

        self.get_channel(&id).await
    }

    pub async fn update_channel(&self, input: &UpdateChannelInput) -> Result<Channel, sqlx::Error> {
        // Explicit empty native endpoints are rejected (T02 DTO contract):
        // None = keep, empty Vec = invalid configuration.
        if let Some(eps) = &input.native_endpoints {
            if eps.is_empty() {
                return Err(sqlx::Error::Protocol(
                    "native_endpoints must not be explicitly empty; omit it to keep current value"
                        .to_string(),
                ));
            }
        }

        let now = now_iso();
        let mut tx = self.pool.begin().await?;

        // STEP 1: write the legacy/business fields exactly as the payload
        // provides them (old frontend payloads). Naming type/base_url/config in
        // this UPDATE fires the invalidation trigger (revision 0), which is what
        // makes an old binary's legacy edit re-infer identity on next read.
        let mut q = sqlx::QueryBuilder::new("UPDATE channels SET updated_at = ");

        q.push_bind(&now);

        if let Some(name) = &input.name {
            q.push(", name = ").push_bind(name);
        }
        if let Some(ct) = &input.channel_type {
            q.push(", type = ").push_bind(ct);
        }
        if let Some(base_url) = &input.base_url {
            q.push(", base_url = ").push_bind(base_url);
        }
        if let Some(api_key) = &input.api_key {
            q.push(", api_key = ").push_bind(api_key);
        }
        if input.clear_api_key == Some(true) {
            q.push(", api_key = ").push_bind("");
        }
        if let Some(models) = &input.models {
            let m = serde_json::to_string(models).unwrap_or_else(|_| "[]".to_string());
            q.push(", models = ").push_bind(m);
        }
        if let Some(status) = input.status {
            q.push(", status = ").push_bind(status);
        }
        if let Some(priority) = input.priority {
            q.push(", priority = ").push_bind(priority);
        }
        if let Some(weight) = input.weight {
            q.push(", weight = ").push_bind(weight);
        }
        if let Some(config) = &input.config {
            let c = serde_json::to_string(config).unwrap_or_else(|_| "{}".to_string());
            q.push(", config = ").push_bind(c);
        }
        if let Some(mapping) = &input.model_mapping {
            let m = serde_json::to_string(mapping).unwrap_or_else(|_| "{}".to_string());
            q.push(", model_mapping = ").push_bind(m);
        }
        if let Some(timeout_secs) = input.timeout_secs {
            q.push(", timeout_secs = ").push_bind(timeout_secs);
        }

        q.push(" WHERE id = ").push_bind(&input.id);
        q.build().execute(&mut *tx).await?;

        // Read the row as it now stands (post-legacy-write, post-trigger) so the
        // identity plan starts from the persisted state: if the trigger fired,
        // identity fields are NULL/revision 0 and we re-infer.
        let row: Channel = sqlx::query_as("SELECT * FROM channels WHERE id = ?")
            .bind(&input.id)
            .fetch_one(&mut *tx)
            .await?;

        // Effective fields: what was written this UPDATE, else the row's value.
        let eff_type = input
            .channel_type
            .clone()
            .unwrap_or_else(|| row.channel_type.clone());
        let eff_base = input
            .base_url
            .clone()
            .unwrap_or_else(|| row.base_url.clone());
        let eff_config = input
            .config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| row.config.clone());
        let eff_protocol = input.protocol.clone().or_else(|| row.protocol.clone());
        let eff_provider = input.provider.clone().or_else(|| row.provider.clone());
        let eff_native_base = input
            .native_base_url
            .clone()
            .or_else(|| row.native_base_url.clone());
        let eff_eps = input
            .native_endpoints
            .clone()
            .or_else(|| parse_eps(&row.native_endpoints));
        let eff_preset_revision = input
            .preset_revision
            .clone()
            .or_else(|| row.preset_revision.clone());

        // Compute the full identity plan. On a full identity write this yields
        // the DERIVED legacy dual-write pair (type/base_url from new_to_legacy).
        let (identity, legacy_type, legacy_base, endpoints_json) = Self::plan_channel_identity(
            &eff_protocol,
            &eff_provider,
            &eff_native_base,
            &eff_eps,
            row.identity_revision,
            &eff_type,
            &eff_base,
            &eff_config,
        );

        // STEP 1b: write the DERIVED legacy dual-write pair in a separate
        // statement (never merged with the identity write — 不得单条 UPDATE
        // 同时写新旧). On a full-identity write this repairs a raw/empty
        // base_url to the old-code compat root (F1); on a legacy write it is a
        // no-op equal to the effective fields.
        sqlx::query("UPDATE channels SET type = ?, base_url = ? WHERE id = ?")
            .bind(&legacy_type)
            .bind(&legacy_base)
            .bind(&input.id)
            .execute(&mut *tx)
            .await?;

        // STEP 2: final UPDATE writes the complete new identity + current
        // revision. If the identity plan fell back to legacy inference, the
        // revision stays 0 and the resolver live-infers on read.
        sqlx::query(
            "UPDATE channels SET
                protocol = ?, provider = ?, native_base_url = ?, native_endpoints = ?,
                preset_revision = ?, identity_revision = ?, legacy_executor_override = ?
             WHERE id = ?",
        )
        .bind(&identity.protocol)
        .bind(&identity.provider)
        .bind(&identity.native_base_url)
        .bind(&endpoints_json)
        .bind(&eff_preset_revision)
        .bind(identity.identity_revision)
        .bind(&identity.legacy_executor_override)
        .bind(&input.id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        self.get_channel(&input.id).await
    }

    pub async fn update_channel_status(&self, id: &str, status: i64) -> Result<(), sqlx::Error> {
        let now = now_iso();
        sqlx::query("UPDATE channels SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_channel(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM channels WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_channel_test_result(&self, id: &str, ok: bool) -> Result<(), sqlx::Error> {
        let now = now_iso();
        sqlx::query("UPDATE channels SET last_test_at = ?, last_test_ok = ? WHERE id = ?")
            .bind(&now)
            .bind(if ok { 1 } else { 0 })
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn reorder_channels(&self, ordered_ids: &[String]) -> Result<(), sqlx::Error> {
        let now = now_iso();
        let mut tx = self.pool.begin().await?;
        for (i, id) in ordered_ids.iter().enumerate() {
            let priority = (ordered_ids.len() - i) as i64;
            sqlx::query("UPDATE channels SET priority = ?, updated_at = ? WHERE id = ?")
                .bind(priority)
                .bind(&now)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // ==================== API Key ====================

    pub async fn get_all_api_keys(&self) -> Result<Vec<ApiKey>, sqlx::Error> {
        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await
    }

    pub async fn get_api_key_by_key(&self, key: &str) -> Result<ApiKey, sqlx::Error> {
        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE key = ? AND status = 1")
            .bind(key)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn create_api_key(&self, input: &CreateApiKeyInput) -> Result<ApiKey, sqlx::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_iso();
        let key = format!("sk-waliapi-{}", uuid::Uuid::new_v4().simple());
        let allowed_models =
            serde_json::to_string(&input.allowed_models.clone().unwrap_or_default())
                .unwrap_or_else(|_| "[]".to_string());
        let allowed_channels =
            serde_json::to_string(&input.allowed_channels.clone().unwrap_or_default())
                .unwrap_or_else(|_| "[]".to_string());

        sqlx::query(
            "INSERT INTO api_keys (id, name, key, status, allowed_models, allowed_channels, quota_limit, quota_used, created_at, updated_at)
             VALUES (?, ?, ?, 1, ?, ?, ?, 0, ?, ?)"
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&key)
        .bind(&allowed_models)
        .bind(&allowed_channels)
        .bind(input.quota_limit.unwrap_or(-1))
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn update_api_key_status(&self, id: &str, status: i64) -> Result<(), sqlx::Error> {
        let now = now_iso();
        sqlx::query("UPDATE api_keys SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_api_key(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn increment_quota(&self, id: &str, tokens: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE api_keys SET quota_used = quota_used + ? WHERE id = ?")
            .bind(tokens)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ==================== Request Log ====================

    pub async fn create_log(&self, log: &RequestLog) -> Result<(), sqlx::Error> {
        // Insert with seq auto-incremented via subquery (atomic, avoids race condition).
        // The 11 T09 observability columns (migration 016) are bound as Option<> so
        // legacy callers using `..Default::default()` persist NULLs for them.
        sqlx::query(
            "INSERT INTO request_logs (id, seq, api_key_id, api_key_name, channel_id, channel_name, model, upstream_model, mode, status_code, prompt_tokens, completion_tokens, total_tokens, duration_ms, error_message, is_stream, is_retry, created_at, request_body, response_choices, risk_level, risk_score, risk_summary, security_action, sanitized, blocked_reason, trace_id, downstream_protocol, downstream_endpoint, route_group, upstream_protocol, upstream_endpoint, provider, codec_version, failure_class, identity_revision, client_cancelled, stream_committed)
             VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM request_logs), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&log.id)
        .bind(&log.api_key_id)
        .bind(&log.api_key_name)
        .bind(&log.channel_id)
        .bind(&log.channel_name)
        .bind(&log.model)
        .bind(&log.upstream_model)
        .bind(&log.mode)
        .bind(log.status_code)
        .bind(log.prompt_tokens)
        .bind(log.completion_tokens)
        .bind(log.total_tokens)
        .bind(log.duration_ms)
        .bind(&log.error_message)
        .bind(log.is_stream)
        .bind(log.is_retry)
        .bind(&log.created_at)
        .bind(&log.request_body)
        .bind(&log.response_choices)
        .bind(&log.risk_level)
        .bind(log.risk_score)
        .bind(&log.risk_summary)
        .bind(&log.security_action)
        .bind(log.sanitized)
        .bind(&log.blocked_reason)
        .bind(&log.trace_id)
        .bind(&log.downstream_protocol)
        .bind(&log.downstream_endpoint)
        .bind(&log.route_group)
        .bind(&log.upstream_protocol)
        .bind(&log.upstream_endpoint)
        .bind(&log.provider)
        .bind(&log.codec_version)
        .bind(&log.failure_class)
        .bind(log.identity_revision)
        .bind(log.client_cancelled)
        .bind(log.stream_committed)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_security_findings(
        &self,
        log_id: &str,
        findings: &[crate::security::SecurityFinding],
        action: &str,
    ) -> Result<(), sqlx::Error> {
        for finding in findings {
            sqlx::query(
                "INSERT INTO request_security_findings (id, log_id, phase, category, rule_id, severity, title, description, location, evidence_masked, evidence_hash, action, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(crate::utils::id::new_id())
            .bind(log_id)
            .bind(&finding.phase)
            .bind(&finding.category)
            .bind(&finding.rule_id)
            .bind(finding.severity.as_str())
            .bind(&finding.title)
            .bind(&finding.description)
            .bind(&finding.location)
            .bind(&finding.evidence_masked)
            .bind(Option::<String>::None)
            .bind(action)
            .bind(crate::utils::time::now_iso())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn get_security_findings(
        &self,
        log_id: &str,
    ) -> Result<Vec<RequestSecurityFinding>, sqlx::Error> {
        sqlx::query_as::<_, RequestSecurityFinding>(
            "SELECT * FROM request_security_findings WHERE log_id = ? ORDER BY created_at ASC",
        )
        .bind(log_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_log(&self, id: &str) -> Result<RequestLog, sqlx::Error> {
        sqlx::query_as::<_, RequestLog>("SELECT * FROM request_logs WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn delete_logs_before(&self, before_date: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM request_logs WHERE created_at < ?")
            .bind(before_date)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_all_logs(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM request_logs")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_log(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM request_logs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_logs(&self, limit: i64, offset: i64) -> Result<Vec<RequestLog>, sqlx::Error> {
        sqlx::query_as::<_, RequestLog>(
            "SELECT * FROM request_logs ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn search_logs(
        &self,
        keyword: Option<&str>,
        api_key_name: Option<&str>,
        channel_name: Option<&str>,
        model: Option<&str>,
        date_from: Option<&str>,
        date_to: Option<&str>,
        trace_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RequestLog>, sqlx::Error> {
        let mut q = sqlx::QueryBuilder::new("SELECT * FROM request_logs WHERE 1=1");

        if let Some(kw) = keyword {
            let pattern = format!("%{}%", kw);
            q.push(" AND (api_key_name LIKE ")
                .push_bind(pattern.clone());
            q.push(" OR channel_name LIKE ").push_bind(pattern.clone());
            q.push(" OR model LIKE ").push_bind(pattern.clone());
            q.push(" OR upstream_model LIKE ")
                .push_bind(pattern.clone());
            q.push(" OR api_key_id LIKE ").push_bind(pattern.clone());
            q.push(" OR id LIKE ").push_bind(pattern);
            q.push(")");
        }

        if let Some(name) = api_key_name {
            let pattern = format!("%{}%", name);
            q.push(" AND api_key_name LIKE ").push_bind(pattern);
        }

        if let Some(name) = channel_name {
            let pattern = format!("%{}%", name);
            q.push(" AND channel_name LIKE ").push_bind(pattern);
        }

        if let Some(m) = model {
            let pattern = format!("%{}%", m);
            q.push(" AND (model LIKE ").push_bind(pattern.clone());
            q.push(" OR upstream_model LIKE ").push_bind(pattern);
            q.push(")");
        }

        if let Some(from) = date_from {
            q.push(" AND created_at >= ").push_bind(from);
        }

        if let Some(to) = date_to {
            q.push(" AND created_at <= ").push_bind(to);
        }

        if let Some(tid) = trace_id {
            let pattern = format!("%{}%", tid);
            q.push(" AND trace_id LIKE ").push_bind(pattern);
        }

        q.push(" ORDER BY created_at DESC LIMIT ").push_bind(limit);
        q.push(" OFFSET ").push_bind(offset);

        q.build_query_as::<RequestLog>().fetch_all(&self.pool).await
    }

    pub async fn get_dashboard_stats(&self) -> Result<DashboardStats, sqlx::Error> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let today_prefix = format!("{}%", today);

        let today_requests: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE created_at LIKE ?")
                .bind(&today_prefix)
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);

        let today_total_tokens: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(total_tokens), 0) FROM request_logs WHERE created_at LIKE ?",
        )
        .bind(&today_prefix)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        let active_channels: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM channels WHERE status = 1")
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);

        let total_channels: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channels")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        let total_api_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        let total_requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        let total_tokens: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(total_tokens), 0) FROM request_logs")
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);

        let avg_latency: f64 = sqlx::query_scalar(
            "SELECT COALESCE(AVG(duration_ms), 0) FROM request_logs WHERE created_at LIKE ?",
        )
        .bind(&today_prefix)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0.0);

        let total_knowledge_bases: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM kb_knowledge_bases")
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);

        let total_kb_documents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_documents")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        let total_kb_chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_chunks")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        Ok(DashboardStats {
            today_requests,
            today_total_tokens,
            active_channels,
            avg_latency_ms: avg_latency,
            total_channels,
            total_api_keys,
            total_requests,
            total_tokens,
            total_knowledge_bases,
            total_kb_documents,
            total_kb_chunks,
        })
    }

    pub async fn get_channel_stats(&self) -> Result<Vec<ChannelStats>, sqlx::Error> {
        sqlx::query_as::<_, ChannelStats>(
            "SELECT\n                r.channel_id as channel_id,\n                COUNT(*) as total_calls,\n                SUM(CASE WHEN r.status_code >= 200 AND r.status_code < 300 THEN 1 ELSE 0 END) as success_calls,\n                SUM(CASE WHEN r.status_code >= 200 AND r.status_code < 300 THEN 0 ELSE 1 END) as failed_calls,\n                COALESCE(SUM(r.total_tokens), 0) as total_tokens,\n                COALESCE(SUM(r.prompt_tokens), 0) as prompt_tokens,\n                COALESCE(SUM(r.completion_tokens), 0) as completion_tokens,\n                COALESCE(AVG(r.duration_ms), 0) as avg_latency_ms,\n                MAX(r.created_at) as last_call_at\n            FROM request_logs r\n            WHERE r.channel_id IS NOT NULL\n            GROUP BY r.channel_id"
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_api_key_stats(&self) -> Result<Vec<ApiKeyStats>, sqlx::Error> {
        sqlx::query_as::<_, ApiKeyStats>(
            "SELECT\n                r.api_key_id as api_key_id,\n                COUNT(*) as total_calls,\n                SUM(CASE WHEN r.status_code >= 200 AND r.status_code < 300 THEN 1 ELSE 0 END) as success_calls,\n                SUM(CASE WHEN r.status_code >= 200 AND r.status_code < 300 THEN 0 ELSE 1 END) as failed_calls,\n                COALESCE(SUM(r.total_tokens), 0) as total_tokens,\n                COALESCE(SUM(r.prompt_tokens), 0) as prompt_tokens,\n                COALESCE(SUM(r.completion_tokens), 0) as completion_tokens,\n                COALESCE(AVG(r.duration_ms), 0) as avg_latency_ms,\n                MAX(r.created_at) as last_call_at\n            FROM request_logs r\n            WHERE r.api_key_id IS NOT NULL\n            GROUP BY r.api_key_id"
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_log_stats(&self, days: i64) -> Result<Vec<LogStats>, sqlx::Error> {
        let since = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::days(days))
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();

        sqlx::query_as::<_, LogStats>(
            "SELECT substr(created_at, 1, 10) as date, COUNT(*) as count, COALESCE(SUM(total_tokens), 0) as total_tokens
             FROM request_logs
             WHERE created_at >= ?
             GROUP BY date
             ORDER BY date DESC"
        )
        .bind(&since)
        .fetch_all(&self.pool)
        .await
    }
}
