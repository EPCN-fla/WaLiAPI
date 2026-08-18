//! Web 管理面板 SSE 事件桥：把 Tauri 桌面事件同时广播到 Web 前端。
//!
//! 桌面端通过 `app.emit(event, payload)` 把进度事件推给 Webview；
//! Web 面板没有 Webview，改为订阅 `/admin/api/events` SSE 流。
//! 这里提供与 `Emitter::emit` 等价的辅助函数，同时向两个通道投递。

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

/// broadcast channel 容量；超出后最旧的事件被丢弃（lagged 客户端跳过）。
pub const EVENT_CHANNEL_CAPACITY: usize = 100;

#[derive(Clone, Debug)]
pub struct AdminEvent {
    pub event: String,
    pub payload: serde_json::Value,
}

/// 与 `app.emit(event, payload)` 等价，额外把事件广播到 Web SSE 桥。
///
/// 仅用于 Web 前端也监听的事件（如 `kb-import-progress`、`kb-index-progress`、
/// `kb-document-progress`、`kb-document-error`、`wiki-source-progress`、
/// `theme-changed`）。发送失败（无订阅者）属正常情况，静默忽略。
pub fn emit_admin_event(app: &AppHandle, event: &str, payload: serde_json::Value) {
    let _ = app.emit(event, payload.clone());
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let _ = state.event_tx.send(AdminEvent {
            event: event.to_string(),
            payload,
        });
    }
}

/// 泛型版本：payload 先序列化为 JSON。
pub fn emit_admin<T: Serialize>(app: &AppHandle, event: &str, payload: T) {
    let value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    emit_admin_event(app, event, value);
}
