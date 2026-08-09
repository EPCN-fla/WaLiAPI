use serde_json::json;
use waliapi_lib::db::{
    models::{AuthAccountUpsert, ModelState, ModelStates, QuotaState},
    repository::Repository,
};

async fn fresh_db() -> sqlx::SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory db");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate fresh db");
    pool
}

fn upsert(
    provider: &str,
    account_id: &str,
    label: &str,
    payload: serde_json::Value,
) -> AuthAccountUpsert {
    AuthAccountUpsert {
        provider: provider.into(),
        label: label.into(),
        account_id: account_id.into(),
        attributes: json!({"email": "person@example.test"}),
        payload,
        last_refreshed_at: Some("2026-08-09T00:00:00.000Z".into()),
        next_refresh_after: None,
        next_retry_after: None,
    }
}

#[tokio::test]
async fn auth_repository_upsert_preserves_route_configuration_and_provider_scope() {
    let repo = Repository::new(fresh_db().await);
    let first = repo
        .upsert_by_provider_account_id(&upsert(
            "codex",
            "shared-id",
            "Original",
            json!({"token": "old"}),
        ))
        .await
        .expect("first upsert");
    repo.update_auth_account(&first.id, "Custom label", 7, 4)
        .await
        .expect("update route settings");
    repo.update_auth_account_disabled(&first.id, true)
        .await
        .expect("disable account");

    let replaced = repo
        .upsert_by_provider_account_id(&upsert(
            "codex",
            "shared-id",
            "Login label",
            json!({"token": "new"}),
        ))
        .await
        .expect("replacement upsert");
    assert_eq!(replaced.id, first.id);
    assert_eq!(replaced.label, "Custom label");
    assert_eq!(replaced.priority, 7);
    assert_eq!(replaced.weight, 4);
    assert_eq!(replaced.disabled, 1);
    assert_eq!(replaced.payload_json, json!({"token": "new"}).to_string());

    let other_provider = repo
        .upsert_by_provider_account_id(&upsert(
            "other",
            "shared-id",
            "Other",
            json!({"token": "other"}),
        ))
        .await
        .expect("other provider account");
    assert_ne!(other_provider.id, first.id);
    assert_eq!(repo.list_auth_accounts().await.unwrap().len(), 2);
}

#[tokio::test]
async fn auth_repository_model_failure_preserves_snapshot_and_expired_quota_routes_again() {
    let repo = Repository::new(fresh_db().await);
    let account = repo
        .upsert_by_provider_account_id(&upsert("codex", "acct", "Codex", json!({"token": "x"})))
        .await
        .unwrap();
    let snapshot = ModelStates {
        version: 1,
        models: vec![ModelState {
            id: "gpt-test".into(),
            status: "available".into(),
            unavailable: false,
            next_retry_after: None,
            last_error: None,
        }],
    };
    repo.update_models_if_success(&account.id, &snapshot, "2026-08-09T01:00:00.000Z")
        .await
        .unwrap();
    let before = repo.get_auth_account(&account.id).await.unwrap();

    // No write occurs on a failed sync, so both values remain exactly stable.
    let after = repo.get_auth_account(&account.id).await.unwrap();
    assert_eq!(after.model_states_json, before.model_states_json);
    assert_eq!(after.last_models_sync_at, before.last_models_sync_at);

    let quota = QuotaState {
        exceeded: true,
        next_recover_at: Some("2026-08-09T01:30:00.000Z".into()),
        ..Default::default()
    };
    repo.update_quota(&account.id, Some(&quota)).await.unwrap();
    let routeable = repo
        .list_route_accounts("2026-08-09T02:00:00.000Z")
        .await
        .unwrap();
    assert_eq!(routeable.len(), 1);
    assert_eq!(routeable[0].id, account.id);
    assert!(!routeable[0].quota_state().unwrap().unwrap().exceeded);
}
