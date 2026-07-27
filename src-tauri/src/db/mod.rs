pub mod models;
pub mod repository;

use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tauri::{AppHandle, Manager};

pub struct Database {
    pub pool: SqlitePool,
}

/// 修复旧版迁移记录的 checksum，使 v0.1.1 用户升级到 v0.1.3 时不会 VersionMismatch
///
/// v0.1.1 → v0.1.3 迁移文件变更：
/// - 005_add_response_choices.sql → 005_add_response_choices_and_seq.sql（内容变更）
/// - 007 文件可能被本地修改过（description 不匹配）
///
/// 此函数在 sqlx::migrate 之前运行，将旧 checksum 更新为当前文件的 checksum。
/// 仅更新已有记录，不会跳过任何迁移。
async fn fix_legacy_migration_checksums(pool: &SqlitePool) {
    use sha2::Digest;

    // 计算当前迁移文件的 SHA-384 checksum（与 sqlx 算法一致：对文件内容原始字节做 SHA-384）
    let migration_005 = include_str!("../../migrations/005_add_response_choices_and_seq.sql");
    let checksum_005: Vec<u8> = sha2::Sha384::digest(migration_005.as_bytes()).to_vec();

    let migration_007 = include_str!("../../migrations/007_fix_log_seq.sql");
    let checksum_007: Vec<u8> = sha2::Sha384::digest(migration_007.as_bytes()).to_vec();

    // 更新 version=5 和 version=7 的 checksum（BLOB 类型），使其匹配当前文件
    for (version, new_checksum) in [(5i64, checksum_005), (7i64, checksum_007)] {
        let result = sqlx::query(
            "UPDATE _sqlx_migrations SET checksum = ? WHERE version = ? AND checksum != ?",
        )
        .bind(&new_checksum)
        .bind(version)
        .bind(&new_checksum)
        .execute(pool)
        .await;

        if let Ok(res) = result {
            if res.rows_affected() > 0 {
                log::warn!("已修复迁移版本 {} 的 checksum 以兼容 v0.1.3", version);
            }
        }
    }
}

impl Database {
    pub async fn new(app: &AppHandle) -> Self {
        let app_data_dir = app
            .path()
            .app_data_dir()
            .expect("failed to get app data dir");

        std::fs::create_dir_all(&app_data_dir).expect("failed to create app data dir");

        let db_path = app_data_dir.join("waliapi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&db_url)
            .await
            .expect("failed to connect to database");

        // 修复旧版迁移 checksum（v0.1.1 → v0.1.3 兼容）
        fix_legacy_migration_checksums(&pool).await;

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("failed to run database migrations");

        // Seed built-in security rules if table exists and is empty
        let _ = crate::security::rules::seed_builtin_rules(&pool).await;

        Self { pool }
    }
}
