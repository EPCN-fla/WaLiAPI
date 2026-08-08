mod adaptor;
mod channel_presets;
pub mod commands;
pub mod core;
pub mod db;
mod endpoint_executor;
mod protocol;
#[cfg(test)]
mod rollout_integration_tests;
pub mod security;
mod server;
pub mod services;
pub mod utils;

use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, RunEvent,
};
use tauri_plugin_store::StoreExt;
use tokio::sync::RwLock;

pub struct AppState {
    pub db: Arc<db::Database>,
    pub server_port: Arc<RwLock<u16>>,
    pub server_running: Arc<std::sync::atomic::AtomicBool>,
    pub server_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// T07: short-lived, in-process test-run receipt store used to validate
    /// `test_run_id + draft_fingerprint + force_save` at channel save time.
    /// Process restart clears it → every receipt expires → re-test required.
    pub test_receipts: Arc<crate::services::channel_test::TestReceiptStore>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(
            tauri_plugin_sql::Builder::default()
                .add_migrations(
                    "sqlite:waliapi.db",
                    vec![tauri_plugin_sql::Migration {
                        version: 1,
                        description: "init database",
                        sql: include_str!("../migrations/001_init.sql"),
                        kind: tauri_plugin_sql::MigrationKind::Up,
                    }],
                )
                .build(),
        )
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None::<Vec<&str>>,
        ))
        .setup(|app| {
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let show_item = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip("WaLiAPI - Local LLM API Gateway")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let _ = restore_main_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "show" => {
                        let _ = restore_main_window(app);
                    }
                    _ => {}
                })
                .build(app)?;

            if let Some(window) = app.get_webview_window("main") {
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| match event {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        if should_close_to_tray(&app_handle) {
                            api.prevent_close();
                            if let Some(main_window) = app_handle.get_webview_window("main") {
                                let _ = main_window.hide();
                            }
                        }
                    }
                    _ => {}
                });
            }

            let app_handle = app.handle().clone();
            tauri::async_runtime::block_on(async move {
                let db = db::Database::new(&app_handle).await;
                let state = Arc::new(AppState {
                    db: Arc::new(db),
                    server_port: Arc::new(RwLock::new(0)),
                    server_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    server_handle: Arc::new(RwLock::new(None)),
                    test_receipts: Arc::new(crate::services::channel_test::TestReceiptStore::new(
                        std::time::Duration::from_secs(30 * 60),
                    )),
                });
                app_handle.manage(state.clone());

                let handle = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = server::start_server(handle, state).await;
                });
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::channel::get_channels,
            commands::channel::get_channel_presets,
            commands::channel::get_channel,
            commands::channel::get_channel_api_key,
            commands::channel::create_channel,
            commands::channel::update_channel,
            commands::channel::toggle_channel,
            commands::channel::delete_channel,
            commands::channel::test_channel,
            commands::channel::test_channel_draft,
            commands::channel::sync_upstream_models,
            commands::channel::get_channel_stats,
            commands::channel::reorder_channels,
            commands::api_key::get_api_keys,
            commands::api_key::create_api_key,
            commands::api_key::update_api_key,
            commands::api_key::delete_api_key,
            commands::api_key::get_api_key_stats,
            commands::log::get_logs,
            commands::log::get_log,
            commands::log::get_log_security_findings,
            commands::log::delete_log,
            commands::log::delete_logs_before,
            commands::log::delete_all_logs,
            commands::log::get_log_stats,
            commands::stats::get_dashboard_stats,
            commands::settings::get_settings,
            commands::settings::get_feature_flags,
            commands::settings::save_settings,
            commands::settings::apply_theme,
            commands::settings::set_auto_start,
            commands::server::get_server_status,
            commands::server::restart_server,
            commands::security::get_builtin_security_rules,
            commands::security::update_builtin_security_rule,
            commands::security::delete_builtin_security_rule,
            commands::security::reset_builtin_security_rules,
            commands::security::get_custom_security_rules,
            commands::security::create_custom_security_rule,
            commands::security::toggle_custom_security_rule,
            commands::security::delete_custom_security_rule,
            commands::import_export::export_channels,
            commands::import_export::import_walicode_backup,
            commands::import_export::import_waliapi_export,
            commands::import_export::scan_local_ai_configs,
            commands::import_export::import_scanned_sources,
            commands::import_export::pick_import_file,
            commands::import_export::save_export_file,
            // Knowledge Base
            commands::knowledge_base::get_knowledge_bases,
            commands::knowledge_base::create_knowledge_base,
            commands::knowledge_base::update_knowledge_base,
            commands::knowledge_base::delete_knowledge_base,
            commands::knowledge_base::get_kb_documents,
            commands::knowledge_base::delete_kb_document,
            commands::knowledge_base::reindex_kb_document,
            commands::knowledge_base::search_knowledge_base,
            commands::knowledge_base::ask_knowledge_base,
            commands::knowledge_base::get_kb_stats,
            commands::knowledge_base::upload_kb_document,
            commands::knowledge_base::get_kb_conversations,
            commands::knowledge_base::clear_kb_conversations,
            commands::knowledge_base::get_kb_sources,
            commands::knowledge_base::delete_kb_source,
            commands::knowledge_base::import_kb_source,
            commands::knowledge_base::get_kb_index_status,
            commands::knowledge_base::build_kb_index,
            commands::knowledge_base::drop_kb_index,
            commands::knowledge_base::get_kb_tags,
            commands::services::get_service_statuses,
            // Wiki
            commands::wiki::get_wiki_projects,
            commands::wiki::create_wiki_project,
            commands::wiki::get_wiki_project,
            commands::wiki::update_wiki_project,
            commands::wiki::delete_wiki_project,
            commands::wiki::get_wiki_pages,
            commands::wiki::get_wiki_page,
            commands::wiki::save_wiki_page,
            commands::wiki::get_wiki_sources,
            commands::wiki::add_wiki_source,
            commands::wiki::delete_wiki_source,
            commands::wiki::search_wiki,
            commands::wiki::get_wiki_graph,
            commands::wiki::get_wiki_stats,
            commands::wiki::ingest_wiki_source,
            commands::wiki::rescan_wiki_sources,
            commands::wiki::get_wiki_tags,
            // App Config (应用配置)
            commands::app_config::get_app_configs,
            commands::app_config::apply_app_config,
            commands::app_config::clear_app_config,
            commands::app_config::get_app_config_content,
            commands::app_config::open_config_folder,
        ])
        .build(tauri::generate_context!())
        .expect("error while building WaLiAPI")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            {
                if let RunEvent::Reopen { .. } = event {
                    let _ = restore_main_window(app);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app, &event);
            }
        });
}

fn restore_main_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        {
            let _ = app.show();
        }
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    Ok(())
}

fn should_close_to_tray(app: &tauri::AppHandle) -> bool {
    app.store("settings.json")
        .ok()
        .and_then(|store| store.get("general.close_to_tray").and_then(|v| v.as_bool()))
        .unwrap_or(true)
}
