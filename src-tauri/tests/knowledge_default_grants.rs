//! 新建密钥和知识库的双向默认授权、显式撤权及事务原子性回归。
use serde_json::json;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use waliapi_lib::{
    db::{models::CreateApiKeyInput, repository::Repository},
    server::knowledge_access::{get_grants, set_grants},
    services::knowledge::{
        models::{CreateKbInput, UpdateKbInput},
        repository::KbRepository,
    },
};

async fn memory_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    pool
}

fn key_input(name: &str) -> CreateApiKeyInput {
    serde_json::from_value(json!({"name": name})).unwrap()
}

fn kb_input(name: &str) -> CreateKbInput {
    serde_json::from_value(json!({"name": name})).unwrap()
}

async fn create_key(pool: &SqlitePool, name: &str) -> String {
    Repository::new(pool.clone())
        .create_api_key(&key_input(name))
        .await
        .unwrap()
        .id
}

async fn create_kb(pool: &SqlitePool, name: &str) -> String {
    KbRepository::new(pool.clone())
        .create_kb(&kb_input(name))
        .await
        .unwrap()
        .id
}

async fn assert_grants(pool: &SqlitePool, key: &str, expected: &[&str]) {
    let mut expected: Vec<String> = expected.iter().map(|id| (*id).to_owned()).collect();
    expected.sort();
    assert_eq!(get_grants(pool, key).await.unwrap(), expected);
}

#[tokio::test]
async fn creating_api_key_grants_all_existing_knowledge_bases() {
    let pool = memory_pool().await;
    let kb_a = create_kb(&pool, "已有知识库 A").await;
    let kb_b = create_kb(&pool, "已有知识库 B").await;
    let key = create_key(&pool, "新密钥").await;

    assert_grants(&pool, &key, &[&kb_a, &kb_b]).await;
}

#[tokio::test]
async fn keys_created_without_knowledge_bases_receive_new_knowledge_base() {
    let pool = memory_pool().await;
    let key_a = create_key(&pool, "先创建密钥 A").await;
    let key_b = create_key(&pool, "先创建密钥 B").await;
    assert_grants(&pool, &key_a, &[]).await;
    assert_grants(&pool, &key_b, &[]).await;

    let kb = create_kb(&pool, "后创建知识库").await;
    assert_grants(&pool, &key_a, &[&kb]).await;
    assert_grants(&pool, &key_b, &[&kb]).await;
}

#[tokio::test]
async fn default_grants_include_disabled_knowledge_bases_and_keys() {
    let pool = memory_pool().await;
    let kb_enabled = create_kb(&pool, "启用知识库").await;
    let kb_disabled = create_kb(&pool, "停用知识库").await;
    KbRepository::new(pool.clone())
        .update_kb(
            &kb_disabled,
            &serde_json::from_value::<UpdateKbInput>(json!({"status": 0})).unwrap(),
        )
        .await
        .unwrap();

    let key_enabled = create_key(&pool, "启用密钥").await;
    let key_disabled = create_key(&pool, "停用密钥").await;
    let repo = Repository::new(pool.clone());
    repo.update_api_key_status(&key_disabled, 0).await.unwrap();
    assert_grants(&pool, &key_enabled, &[&kb_enabled, &kb_disabled]).await;

    let kb_new = create_kb(&pool, "新知识库").await;
    for key in [&key_enabled, &key_disabled] {
        assert_grants(&pool, key, &[&kb_enabled, &kb_disabled, &kb_new]).await;
    }
    let key_new = create_key(&pool, "另一个新密钥").await;
    assert_grants(&pool, &key_new, &[&kb_enabled, &kb_disabled, &kb_new]).await;

    assert_eq!(
        repo.get_api_key_by_id(&key_disabled).await.unwrap().status,
        0
    );
    assert_eq!(
        KbRepository::new(pool.clone())
            .get_kb(&kb_disabled)
            .await
            .unwrap()
            .status,
        0
    );
}

#[tokio::test]
async fn explicit_revocation_survives_updates_and_creation_of_other_records() {
    let pool = memory_pool().await;
    let kb_revoked = create_kb(&pool, "将被撤权的知识库").await;
    let kb_retained = create_kb(&pool, "保留授权的知识库").await;
    let key = create_key(&pool, "已有密钥").await;
    set_grants(&pool, &key, std::slice::from_ref(&kb_retained))
        .await
        .unwrap();

    let repo = Repository::new(pool.clone());
    repo.update_api_key_name(&key, "重命名密钥").await.unwrap();
    repo.update_api_key_quota(&key, 100).await.unwrap();
    repo.update_api_key_allowed_models(&key, &["test-model".to_owned()])
        .await
        .unwrap();
    repo.update_api_key_status(&key, 0).await.unwrap();
    repo.update_api_key_status(&key, 1).await.unwrap();
    let kb_repo = KbRepository::new(pool.clone());
    for status in [0, 1] {
        kb_repo
            .update_kb(
                &kb_revoked,
                &serde_json::from_value::<UpdateKbInput>(
                    json!({"name": "更新已撤权知识库", "status": status}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
    }
    assert_grants(&pool, &key, &[&kb_retained]).await;

    let kb_new = create_kb(&pool, "另一个新知识库").await;
    assert_grants(&pool, &key, &[&kb_retained, &kb_new]).await;
    let key_new = create_key(&pool, "另一个新密钥").await;
    assert_grants(&pool, &key_new, &[&kb_revoked, &kb_retained, &kb_new]).await;
    assert_grants(&pool, &key, &[&kb_retained, &kb_new]).await;

    // 全部撤权同样保持为空；更新密钥不重新灌入默认授权。
    set_grants(&pool, &key, &[]).await.unwrap();
    repo.update_api_key_name(&key, "全部撤权后更新")
        .await
        .unwrap();
    assert_grants(&pool, &key, &[]).await;
}

#[tokio::test]
async fn deleting_either_parent_cascades_only_its_grants() {
    let pool = memory_pool().await;
    let kb_a = create_kb(&pool, "知识库 A").await;
    let kb_b = create_kb(&pool, "知识库 B").await;
    let key_a = create_key(&pool, "密钥 A").await;
    let key_b = create_key(&pool, "密钥 B").await;
    assert_grants(&pool, &key_a, &[&kb_a, &kb_b]).await;

    Repository::new(pool.clone())
        .delete_api_key(&key_a)
        .await
        .unwrap();
    assert_grants(&pool, &key_a, &[]).await;
    assert_grants(&pool, &key_b, &[&kb_a, &kb_b]).await;

    KbRepository::new(pool.clone())
        .delete_kb(&kb_a)
        .await
        .unwrap();
    assert_grants(&pool, &key_b, &[&kb_b]).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM api_key_knowledge_access WHERE kb_id = ?"
        )
        .bind(&kb_a)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}

async fn reject_second_grant(pool: &SqlitePool) {
    // 第一条授权允许写入，第二条故障，证明父行和先前授权均随事务回滚。
    sqlx::query(
        "CREATE TRIGGER reject_second_default_grant BEFORE INSERT ON api_key_knowledge_access
         WHEN (SELECT COUNT(*) FROM api_key_knowledge_access) >= 1
         BEGIN SELECT RAISE(ABORT, 'injected default grant failure'); END",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn assert_no_grants(pool: &SqlitePool) {
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_key_knowledge_access")
            .fetch_one(pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn default_grant_failure_rolls_back_api_key_and_partial_grants() {
    let pool = memory_pool().await;
    let kb_a = create_kb(&pool, "知识库 A").await;
    let kb_b = create_kb(&pool, "知识库 B").await;
    reject_second_grant(&pool).await;

    let error = Repository::new(pool.clone())
        .create_api_key(&key_input("应回滚的密钥"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected default grant failure"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_no_grants(&pool).await;
    assert_eq!(
        KbRepository::new(pool.clone())
            .get_all_kbs()
            .await
            .unwrap()
            .len(),
        2
    );

    sqlx::query("DROP TRIGGER reject_second_default_grant")
        .execute(&pool)
        .await
        .unwrap();
    let key = create_key(&pool, "故障解除后创建密钥").await;
    assert_grants(&pool, &key, &[&kb_a, &kb_b]).await;
}

#[tokio::test]
async fn default_grant_failure_rolls_back_knowledge_base_and_partial_grants() {
    let pool = memory_pool().await;
    let key_a = create_key(&pool, "密钥 A").await;
    let key_b = create_key(&pool, "密钥 B").await;
    reject_second_grant(&pool).await;

    let error = KbRepository::new(pool.clone())
        .create_kb(&kb_input("应回滚的知识库"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected default grant failure"));
    assert!(KbRepository::new(pool.clone())
        .get_all_kbs()
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_no_grants(&pool).await;

    sqlx::query("DROP TRIGGER reject_second_default_grant")
        .execute(&pool)
        .await
        .unwrap();
    let kb = create_kb(&pool, "故障解除后创建知识库").await;
    assert_grants(&pool, &key_a, &[&kb]).await;
    assert_grants(&pool, &key_b, &[&kb]).await;
}

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_file_database_creations_do_not_miss_default_pairs() {
    let directory = TestDirectory(
        std::env::temp_dir().join(format!("waliapi-default-grants-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&directory.0).unwrap();
    let options = SqliteConnectOptions::new()
        .filename(directory.0.join("grants.sqlite"))
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    // 两个独立连接池访问同一个临时文件，覆盖跨连接写锁竞争。
    let key_pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options.clone())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&key_pool).await.unwrap();
    let kb_pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();

    let mut keys = Vec::new();
    let mut kbs = Vec::new();
    for round in 0..4 {
        let barrier = Arc::new(tokio::sync::Barrier::new(9));
        let mut tasks = Vec::new();
        for index in 0..8 {
            let is_key = index % 2 == 0;
            let pool = if is_key { &key_pool } else { &kb_pool }.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                tokio::task::yield_now().await;
                let name = format!("concurrent-{round}-{index}");
                let id = if is_key {
                    create_key(&pool, &name).await
                } else {
                    create_kb(&pool, &name).await
                };
                (is_key, id)
            }));
        }
        barrier.wait().await;
        for task in tasks {
            let (is_key, id) = task.await.unwrap();
            if is_key {
                keys.push(id);
            } else {
                kbs.push(id);
            }
        }
        let expected: Vec<&str> = kbs.iter().map(String::as_str).collect();
        for key in &keys {
            assert_grants(&key_pool, key, &expected).await;
        }
    }
    assert_eq!((keys.len(), kbs.len()), (16, 16));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_key_knowledge_access")
            .fetch_one(&key_pool)
            .await
            .unwrap(),
        256
    );
    let foreign_key_errors = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&key_pool)
        .await
        .unwrap();
    assert!(foreign_key_errors.is_empty());
    key_pool.close().await;
    kb_pool.close().await;
}
