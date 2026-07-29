use crate::AppState;
use crate::db::repository::Repository;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

// ── 数据结构 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub label: String,
    pub icon: String,
    pub description: String,
    pub config_path: String,
    pub config_format: String,
    pub available: bool,
    pub applied: bool,
    pub download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigContent {
    pub exists: bool,
    pub content: String,
    pub error: Option<String>,
}

// ── 应用定义 ──

struct AppDef {
    name: &'static str,
    label: &'static str,
    icon: &'static str,
    description: &'static str,
    config_format: &'static str,
    download_url: &'static str,
    config_dir_fn: fn() -> PathBuf,
    config_file: &'static str,
    check_installed_fn: fn(&PathBuf) -> bool,
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

const APPS: &[AppDef] = &[
    AppDef {
        name: "claude-code",
        label: "Claude Code",
        icon: "terminal",
        description: "Anthropic 的命令行 AI 编程助手，读取 ~/.claude/settings.json 中的 env 配置",
        config_format: "JSON (~/.claude/settings.json)",
        download_url: "https://docs.anthropic.com/en/docs/claude-code/overview",
        config_dir_fn: || home_dir().join(".claude"),
        config_file: "settings.json",
        check_installed_fn: |dir| dir.exists() || home_dir().join(".claude.json").exists(),
    },
    AppDef {
        name: "codex",
        label: "Codex CLI",
        icon: "code",
        description: "OpenAI Codex 命令行工具，读取 ~/.codex/auth.json 和 config.toml",
        config_format: "JSON + TOML (~/.codex/)",
        download_url: "https://github.com/openai/codex",
        config_dir_fn: || home_dir().join(".codex"),
        config_file: "config.toml",
        check_installed_fn: |dir| dir.exists(),
    },
    AppDef {
        name: "gemini-cli",
        label: "Gemini CLI",
        icon: "boxes",
        description: "Google Gemini 命令行工具，读取 ~/.gemini/.env 和 settings.json",
        config_format: "ENV + JSON (~/.gemini/)",
        download_url: "https://github.com/google-gemini/gemini-cli",
        config_dir_fn: || home_dir().join(".gemini"),
        config_file: ".env",
        check_installed_fn: |dir| dir.exists(),
    },
    AppDef {
        name: "claude-desktop",
        label: "Claude Desktop",
        icon: "sparkles",
        description: "Anthropic 桌面应用，读取 claude_desktop_config.json",
        config_format: "JSON (claude_desktop_config.json)",
        download_url: "https://claude.ai/download",
        config_dir_fn: || {
            #[cfg(target_os = "macos")]
            {
                home_dir().join("Library/Application Support/Claude")
            }
            #[cfg(target_os = "windows")]
            {
                home_dir().join("AppData/Roaming/Claude")
            }
            #[cfg(target_os = "linux")]
            {
                home_dir().join(".config/Claude")
            }
        },
        config_file: "claude_desktop_config.json",
        check_installed_fn: |dir| dir.exists(),
    },
    AppDef {
        name: "opencode",
        label: "OpenCode",
        icon: "wrench",
        description: "开源 AI 编程工具，读取 opencode.json 中的 provider 配置",
        config_format: "JSON (~/.config/opencode/opencode.json)",
        download_url: "https://opencode.ai",
        config_dir_fn: || home_dir().join(".config/opencode"),
        config_file: "opencode.json",
        check_installed_fn: |dir| dir.exists(),
    },
    AppDef {
        name: "openclaw",
        label: "OpenClaw",
        icon: "bot",
        description: "开源 Agent 框架，读取配置文件中的 provider 段",
        config_format: "JSON (~/.qclaw/)",
        download_url: "https://openclaw.ai",
        config_dir_fn: || home_dir().join(".qclaw"),
        config_file: "config.json",
        check_installed_fn: |dir| dir.exists(),
    },
    AppDef {
        name: "hermes",
        label: "Hermes Agent",
        icon: "code",
        description: "Hermes Agent 框架，读取配置文件中的 custom_providers 段",
        config_format: "TOML/JSON (Hermes config)",
        download_url: "https://github.com/openai/hermes",
        config_dir_fn: || home_dir().join(".hermes"),
        config_file: "config.json",
        check_installed_fn: |dir| dir.exists(),
    },
    AppDef {
        name: "walicode",
        label: "WaLiCode",
        icon: "code",
        description: "AI Coding Assistant，写入 ai_settings.json 中的 provider 和 apiKey 配置",
        config_format: "JSON (~/Library/Application Support/WaLiCode/ai_settings.json)",
        download_url: "https://walicode.xiaofuge.cn/",
        #[cfg(target_os = "macos")]
        config_dir_fn: || home_dir().join("Library/Application Support/WaLiCode"),
        #[cfg(target_os = "windows")]
        config_dir_fn: || home_dir().join("AppData/Roaming/WaLiCode"),
        #[cfg(target_os = "linux")]
        config_dir_fn: || home_dir().join(".config/walicode"),
        config_file: "ai_settings.json",
        check_installed_fn: |dir| dir.exists(),
    },
];

// ── 原子写入 ──

fn atomic_write(path: &PathBuf, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::write(&tmp, data).map_err(|e| format!("写入临时文件失败: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("替换文件失败: {e}")
    })?;
    Ok(())
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<T, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("读取文件失败: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("解析 JSON 失败: {e}"))
}

fn write_json_file<T: Serialize>(path: &PathBuf, data: &T) -> Result<(), String> {
    let json = serde_json::to_string_pretty(data).map_err(|e| format!("序列化 JSON 失败: {e}"))?;
    // serde_json 默认将 non-ASCII 转义为 \uXXXX，解码回 UTF-8 保持中文可读
    let json = unescape_json_unicode(&json);
    atomic_write(path, json.as_bytes())
}

/// 将 JSON 字符串中的 \\uXXXX 转义序列解码回 UTF-8 字符（含代理对）
fn unescape_json_unicode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'u' {
            // 尝试解析 \uXXXX
            if i + 6 <= bytes.len() {
                if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 6]) {
                    if let Ok(code) = u32::from_str_radix(hex, 16) {
                        // 检查后续是否是代理对（\uXXXX\uXXXX）
                        if (0xD800..=0xDBFF).contains(&code) && i + 12 <= bytes.len()
                            && bytes[i + 6] == b'\\' && bytes[i + 7] == b'u' {
                            if let Ok(hex2) = std::str::from_utf8(&bytes[i + 8..i + 12]) {
                                if let Ok(code2) = u32::from_str_radix(hex2, 16) {
                                    if (0xDC00..=0xDFFF).contains(&code2) {
                                        let cp = 0x10000 + ((code - 0xD800) << 10) + (code2 - 0xDC00);
                                        if let Some(ch) = char::from_u32(cp) {
                                            out.push(ch);
                                            i += 12;
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                            i += 6;
                            continue;
                        }
                    }
                }
            }
        }
        // 安全地推送字节：如果是合法 UTF-8 起始字节就正常推，否则推 byte
        match std::str::from_utf8(&bytes[i..i+1]) {
            Ok(s) => out.push_str(s),
            Err(_) => out.push(bytes[i] as char),
        }
        i += 1;
    }
    out
}

// ── 备份与恢复 ──

fn backup_path(config_path: &PathBuf) -> PathBuf {
    let mut name = config_path.file_name().unwrap_or_default().to_string_lossy().to_string();
    name.push_str(".waliapi-backup");
    config_path.with_file_name(name)
}

fn backup_config(config_path: &PathBuf) -> Result<(), String> {
    if config_path.exists() {
        let content = fs::read(config_path).map_err(|e| format!("读取配置失败: {e}"))?;
        atomic_write(&backup_path(config_path), &content)?;
    }
    Ok(())
}

fn restore_config(config_path: &PathBuf) -> Result<(), String> {
    let backup = backup_path(config_path);
    if backup.exists() {
        let content = fs::read(&backup).map_err(|e| format!("读取备份失败: {e}"))?;
        atomic_write(config_path, &content)?;
        let _ = fs::remove_file(&backup);
        Ok(())
    } else {
        Err("没有找到备份文件".to_string())
    }
}

// ── 获取 WaLiAPI 网关信息 ──

async fn get_waliapi_url(state: &Arc<AppState>) -> String {
    let port = *state.server_port.read().await;
    format!("http://127.0.0.1:{}", port)
}

#[allow(dead_code)]
fn get_waliapi_key(state: &Arc<AppState>) -> Result<String, String> {
    let repo = Repository::new(state.db.pool.clone());
    let keys = tokio::task::block_in_place(|| {
        tauri::async_runtime::handle().block_on(async {
            repo.get_all_api_keys().await
        })
    }).map_err(|e| format!("获取 API Key 失败: {e}"))?;

    keys.into_iter()
        .find(|k| k.status == 1)
        .map(|k| k.key)
        .ok_or_else(|| "没有可用的 API Key，请先在「密钥」页创建".to_string())
}

// ── 各应用配置写入逻辑 ──

fn write_claude_code(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let settings_path = config_dir.join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        read_json_file(&settings_path).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = settings.as_object_mut() {
        obj.insert("env".to_string(), serde_json::json!({
            "ANTHROPIC_BASE_URL": waliapi_url,
            "ANTHROPIC_API_KEY": waliapi_key,
            "ANTHROPIC_MODEL": model
        }));
        obj.insert("_waliapi".to_string(), serde_json::json!(true));
    }

    write_json_file(&settings_path, &settings)
}

fn write_codex(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    use toml_edit::DocumentMut;

    // Codex 鉴权方式：experimental_bearer_token 作为 Bearer token 发给上游
    // 不写 auth.json 的 OPENAI_API_KEY，避免 Codex 拿它去 OpenAI 验证
    // （参照 cc-switch 的做法，只通过 experimental_bearer_token 传递 key）

    let config_path = config_dir.join("config.toml");
    let existing_text = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config.toml: {e}"))?
    } else {
        String::new()
    };

    let mut doc = existing_text
        .parse::<DocumentMut>()
        .map_err(|e| format!("Failed to parse config.toml: {e}"))?;

    // Set model_provider and model at top level
    doc["model_provider"] = toml_edit::value("waliapi");
    doc["model"] = toml_edit::value(model);

    // Ensure [model_providers] table exists
    if doc.get("model_providers").is_none() {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc["model_providers"] = toml_edit::Item::Table(table);
    }

    // Insert/update [model_providers.waliapi] preserving other providers
    if let Some(providers) = doc["model_providers"].as_table_mut() {
        let waliapi_entry = providers.entry("waliapi");
        let provider_table = waliapi_entry.or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        if let Some(table) = provider_table.as_table_mut() {
            table["name"] = toml_edit::value("WaLiAPI Gateway");
            table["base_url"] = toml_edit::value(format!("{}/v1", waliapi_url.trim_end_matches('/')));
            table["wire_api"] = toml_edit::value("responses");
            table["experimental_bearer_token"] = toml_edit::value(waliapi_key);
        }
    }

    atomic_write(&config_path, doc.to_string().as_bytes())?;
    Ok(())
}

fn write_gemini_cli(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let env_path = config_dir.join(".env");
    let env_content = format!(
        "# Generated by WaLiAPI\nGEMINI_API_KEY={}\nGEMINI_BASE_URL={}\nGEMINI_MODEL={}\n",
        waliapi_key, waliapi_url, model
    );
    atomic_write(&env_path, env_content.as_bytes())?;

    let settings_path = config_dir.join("settings.json");
    if !settings_path.exists() {
        write_json_file(&settings_path, &serde_json::json!({}))?;
    }
    Ok(())
}

fn write_claude_desktop(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let config_path = config_dir.join("claude_desktop_config.json");
    let mut config: serde_json::Value = if config_path.exists() {
        read_json_file(&config_path).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = config.as_object_mut() {
        obj.insert("apiKeyHelper".to_string(), serde_json::json!(format!("echo '{}'", waliapi_key)));
        obj.insert("apiBaseUrl".to_string(), serde_json::json!(waliapi_url));
        obj.insert("defaultModel".to_string(), serde_json::json!(model));
        obj.insert("_waliapi".to_string(), serde_json::json!(true));
    }

    write_json_file(&config_path, &config)
}

fn write_opencode(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let config_path = config_dir.join("opencode.json");
    let mut config: serde_json::Value = if config_path.exists() {
        read_json_file(&config_path).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({"$schema": "https://opencode.ai/config.json"})
    };

    if let Some(obj) = config.as_object_mut() {
        let provider = serde_json::json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": "WaLiAPI Gateway",
            "options": {
                "baseURL": format!("{}/v1", waliapi_url),
                "apiKey": waliapi_key
            },
            "models": {
                "waliapi-default": { "name": model }
            }
        });
        if let Some(providers) = obj.get_mut("provider").and_then(|v| v.as_object_mut()) {
            providers.insert("waliapi".to_string(), provider);
        } else {
            obj.insert("provider".to_string(), serde_json::json!({"waliapi": provider}));
        }
    }

    write_json_file(&config_path, &config)
}

fn write_openclaw(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let config_path = config_dir.join("config.json");
    let mut config: serde_json::Value = if config_path.exists() {
        read_json_file(&config_path).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = config.as_object_mut() {
        obj.insert("baseUrl".to_string(), serde_json::json!(format!("{}/v1", waliapi_url)));
        obj.insert("apiKey".to_string(), serde_json::json!(waliapi_key));
        obj.insert("model".to_string(), serde_json::json!(model));
        obj.insert("_waliapi".to_string(), serde_json::json!(true));
    }

    write_json_file(&config_path, &config)
}

fn write_hermes(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let config_path = config_dir.join("config.json");
    let mut config: serde_json::Value = if config_path.exists() {
        read_json_file(&config_path).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = config.as_object_mut() {
        if let Some(providers) = obj.get_mut("custom_providers").and_then(|v| v.as_array_mut()) {
            providers.retain(|p| p.get("id").and_then(|v| v.as_str()) != Some("waliapi"));
            let mut entry = serde_json::Map::new();
            entry.insert("id".to_string(), serde_json::json!("waliapi"));
            entry.insert("name".to_string(), serde_json::json!("WaLiAPI Gateway"));
            entry.insert("base_url".to_string(), serde_json::json!(format!("{}/v1", waliapi_url)));
            entry.insert("api_key".to_string(), serde_json::json!(waliapi_key));
            entry.insert("default_model".to_string(), serde_json::json!(model));
            providers.push(serde_json::Value::Object(entry));
        } else {
            let mut entry = serde_json::Map::new();
            entry.insert("id".to_string(), serde_json::json!("waliapi"));
            entry.insert("name".to_string(), serde_json::json!("WaLiAPI Gateway"));
            entry.insert("base_url".to_string(), serde_json::json!(format!("{}/v1", waliapi_url)));
            entry.insert("api_key".to_string(), serde_json::json!(waliapi_key));
            entry.insert("default_model".to_string(), serde_json::json!(model));
            obj.insert("custom_providers".to_string(), serde_json::Value::Array(vec![serde_json::Value::Object(entry)]));
        }
    }

    write_json_file(&config_path, &config)
}

fn write_walicode(config_dir: &PathBuf, waliapi_url: &str, waliapi_key: &str, model: &str) -> Result<(), String> {
    let config_path = config_dir.join("ai_settings.json");
    let mut config: serde_json::Value = if config_path.exists() {
        read_json_file(&config_path).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = config.as_object_mut() {
        // 写入 WaLiAPI 网关作为 provider 配置
        obj.insert("provider".to_string(), serde_json::json!("openai"));
        obj.insert("providerType".to_string(), serde_json::json!("custom"));
        obj.insert("apiKey".to_string(), serde_json::json!(waliapi_key));
        obj.insert("baseUrl".to_string(), serde_json::json!(format!("{}/v1", waliapi_url.trim_end_matches('/'))));
        obj.insert("model".to_string(), serde_json::json!(model));
        obj.insert("_waliapi".to_string(), serde_json::json!(true));
    }

    write_json_file(&config_path, &config)
}

// ── 检测是否已由 WaLiAPI 配置 ──

fn detect_applied(config_path: &PathBuf, app_name: &str) -> bool {
    if !config_path.exists() {
        return false;
    }
    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    match app_name {
        "claude-code" | "claude-desktop" | "openclaw" => {
            let v: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("_waliapi").and_then(|v| v.as_bool()).unwrap_or(false)
        }
        "codex" => content.contains("WaLiAPI") || content.contains("waliapi"),
        "gemini-cli" => content.contains("WaLiAPI"),
        "opencode" => {
            let v: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.pointer("/provider/waliapi").is_some()
        }
        "hermes" => {
            let v: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("custom_providers")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.iter().find(|p| p.get("id").and_then(|v| v.as_str()) == Some("waliapi")))
                .is_some()
        }
        "walicode" => {
            let v: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("_waliapi").and_then(|v| v.as_bool()).unwrap_or(false)
        }
        _ => false,
    }
}

// ── Tauri Commands ──

#[tauri::command]
pub async fn get_app_configs(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<AppInfo>, String> {
    let _ = &state;
    let apps: Vec<AppInfo> = APPS.iter().map(|app| {
        let config_dir = (app.config_dir_fn)();
        let config_path = config_dir.join(app.config_file);
        let available = (app.check_installed_fn)(&config_dir);
        let applied = detect_applied(&config_path, app.name);

        AppInfo {
            name: app.name.to_string(),
            label: app.label.to_string(),
            icon: app.icon.to_string(),
            description: app.description.to_string(),
            config_path: config_path.to_string_lossy().to_string(),
            config_format: app.config_format.to_string(),
            available,
            applied,
            download_url: app.download_url.to_string(),
        }
    }).collect();

    Ok(apps)
}

#[tauri::command]
pub async fn apply_app_config(
    app_name: String,
    api_key: String,
    model: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<ApplyResult, String> {
    let waliapi_url = get_waliapi_url(&state).await;

    let app_def = APPS.iter().find(|a| a.name == app_name)
        .ok_or_else(|| format!("不支持的应用: {app_name}"))?;

    let config_dir = (app_def.config_dir_fn)();
    let config_path = config_dir.join(app_def.config_file);

    let _ = backup_config(&config_path);

    let result = match app_name.as_str() {
        "claude-code" => write_claude_code(&config_dir, &waliapi_url, &api_key, &model),
        "codex" => write_codex(&config_dir, &waliapi_url, &api_key, &model),
        "gemini-cli" => write_gemini_cli(&config_dir, &waliapi_url, &api_key, &model),
        "claude-desktop" => write_claude_desktop(&config_dir, &waliapi_url, &api_key, &model),
        "opencode" => write_opencode(&config_dir, &waliapi_url, &api_key, &model),
        "openclaw" => write_openclaw(&config_dir, &waliapi_url, &api_key, &model),
        "hermes" => write_hermes(&config_dir, &waliapi_url, &api_key, &model),
        "walicode" => write_walicode(&config_dir, &waliapi_url, &api_key, &model),
        _ => return Err(format!("不支持的应用: {app_name}")),
    };

    match result {
        Ok(()) => Ok(ApplyResult {
            success: true,
            message: format!("配置已写入 {}", config_path.display()),
        }),
        Err(e) => {
            let _ = restore_config(&config_path);
            Ok(ApplyResult { success: false, message: e })
        }
    }
}

#[tauri::command]
pub async fn clear_app_config(
    app_name: String,
) -> Result<ApplyResult, String> {
    let app_def = APPS.iter().find(|a| a.name == app_name)
        .ok_or_else(|| format!("不支持的应用: {app_name}"))?;

    let config_dir = (app_def.config_dir_fn)();
    let config_path = config_dir.join(app_def.config_file);

    match restore_config(&config_path) {
        Ok(()) => Ok(ApplyResult {
            success: true,
            message: format!("已恢复 {} 的原始配置", app_def.label),
        }),
        Err(e) => Ok(ApplyResult { success: false, message: format!("恢复失败: {e}") }),
    }
}

#[tauri::command]
pub async fn get_app_config_content(app_name: String) -> Result<ConfigContent, String> {
    let app_def = APPS.iter().find(|a| a.name == app_name)
        .ok_or_else(|| format!("不支持的应用: {app_name}"))?;

    let config_dir = (app_def.config_dir_fn)();
    let config_path = config_dir.join(app_def.config_file);

    if !config_path.exists() {
        return Ok(ConfigContent { exists: false, content: String::new(), error: None });
    }

    match fs::read_to_string(&config_path) {
        Ok(content) => Ok(ConfigContent { exists: true, content, error: None }),
        Err(e) => Ok(ConfigContent { exists: true, content: String::new(), error: Some(format!("读取失败: {e}")) }),
    }
}

#[tauri::command]
pub async fn open_config_folder(app_name: String) -> Result<(), String> {
    let app_def = APPS.iter().find(|a| a.name == app_name)
        .ok_or_else(|| format!("不支持的应用: {app_name}"))?;

    let config_dir = (app_def.config_dir_fn)();

    // 如果目录不存在，尝试创建
    if !config_dir.exists() {
        fs::create_dir_all(&config_dir).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    // 如果配置文件不存在，先创建一个空文件
    let config_path = config_dir.join(app_def.config_file);
    if !config_path.exists() {
        atomic_write(&config_path, b"{}")?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&config_dir)
            .spawn()
            .map_err(|e| format!("打开文件夹失败: {e}"))?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&config_dir)
            .spawn()
            .map_err(|e| format!("打开文件夹失败: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&config_dir)
            .spawn()
            .map_err(|e| format!("打开文件夹失败: {e}"))?;
    }

    Ok(())
}
