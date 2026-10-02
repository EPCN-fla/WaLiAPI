pub mod claude;
pub mod custom;
pub mod deepseek;
pub mod gemini;
pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Connect-timeout (10 s) shared by all clients.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Blocking-client 分桶上限：超时值来自渠道配置，实际取值集合很小；
/// 上限只防病态配置撑爆缓存。
const BLOCKING_CLIENT_MAX_BUCKETS: usize = 32;

/// FIX-21：流式客户端进程内单例——每个连接池/线程池只建一次，
/// 高流量下不再每请求重建（reqwest::Client clone 是廉价的 Arc 复制）。
/// 出站代理扩展后按「已解析代理 URL」分桶（空串 = 直连）。
static STREAMING_CLIENTS: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();

/// FIX-21：阻塞客户端按 (总超时, 代理) 分桶复用（同组合共享连接池）。
static BLOCKING_CLIENTS: OnceLock<Mutex<HashMap<(u64, String), reqwest::Client>>> = OnceLock::new();

/// 全局出站代理 URL（设置 `network.proxy.enabled` + `network.proxy.url`）。
/// 启动时与 save_settings 时同步；渠道级「跟随全局」/缺省都读这里。
static GLOBAL_PROXY_URL: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// 更新全局出站代理（None = 关闭，全部直连）。
pub fn set_global_proxy(url: Option<String>) {
    let trimmed = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
    *GLOBAL_PROXY_URL.write().unwrap_or_else(|e| e.into_inner()) = trimmed;
}

/// 读取全局出站代理 URL。
pub fn global_proxy_url() -> Option<String> {
    GLOBAL_PROXY_URL
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn streaming_client_map() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    STREAMING_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn blocking_client_map() -> &'static Mutex<HashMap<(u64, String), reqwest::Client>> {
    BLOCKING_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build a reqwest client for **non-streaming** requests: the total request
/// duration (connect + send + receive) is capped at `timeout_secs`.
///
/// FIX-21：按 `(timeout_secs, proxy)` 分桶缓存复用；构建失败降级为无总超时的
/// 兜底客户端并打日志（与旧行为一致，但只告警一次桶）。
///
/// 出站代理语义：`Some(url)` → 请求经该代理转发；`None` → 显式直连
///（`.no_proxy()`，忽略进程环境变量代理），保证「直连」可预期。
pub fn blocking_client(timeout_secs: u64, proxy_url: Option<&str>) -> reqwest::Client {
    let key = (timeout_secs.max(1), proxy_url.unwrap_or("").to_string());
    let mut buckets = blocking_client_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(client) = buckets.get(&key) {
        return client.clone();
    }
    match build_client(
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(std::time::Duration::from_secs(key.0)),
        proxy_url,
    ) {
        Some(client) => {
            if buckets.len() >= BLOCKING_CLIENT_MAX_BUCKETS {
                // 病态配置兜底：不缓存，直接返回（每次构建，但不会内存膨胀）。
                return client;
            }
            buckets.insert(key, client.clone());
            client
        }
        None => {
            tracing::warn!(
                "blocking client build failed (timeout={}s), falling back",
                key.0
            );
            reqwest::Client::new()
        }
    }
}

/// Build a reqwest client for **streaming** (SSE) requests: only the TCP
/// connection establishment is capped at [`CONNECT_TIMEOUT_SECS`]; the
/// response body is allowed to stream indefinitely so long LLM generations
/// are not cut off by a premature total-timeout.
///
/// FIX-21：按已解析代理分桶缓存复用；构建失败降级默认客户端并打日志。
pub fn streaming_client(proxy_url: Option<&str>) -> reqwest::Client {
    let key = proxy_url.unwrap_or("").to_string();
    let mut buckets = streaming_client_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(client) = buckets.get(&key) {
        return client.clone();
    }
    let client = build_client(
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)),
        proxy_url,
    )
    .unwrap_or_else(|| {
        tracing::warn!("streaming client build failed, falling back");
        reqwest::Client::new()
    });
    buckets.insert(key, client.clone());
    client
}

/// 统一给 builder 注入代理：有 URL 用 `Proxy::all`，否则 `.no_proxy()` 显式直连。
pub fn with_proxy(
    builder: reqwest::ClientBuilder,
    proxy_url: Option<&str>,
) -> reqwest::ClientBuilder {
    match proxy_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => match reqwest::Proxy::all(url) {
            Ok(proxy) => builder.proxy(proxy),
            Err(e) => {
                tracing::warn!("invalid proxy url {url}: {e}");
                builder
            }
        },
        None => builder.no_proxy(),
    }
}

/// 构建客户端（with_proxy + build）；None 表示构建失败，调用方应降级。
fn build_client(
    builder: reqwest::ClientBuilder,
    proxy_url: Option<&str>,
) -> Option<reqwest::Client> {
    with_proxy(builder, proxy_url).build().ok()
}

#[cfg(test)]
mod client_reuse_tests {
    use super::*;

    /// 同 (超时, 代理) 组合只建一个客户端；不同组合分桶。全局映射与其他并行
    /// 测试共享，断言只做「键必然落桶」与「总量至少增长」的单向判定（其他
    /// 测试只会增桶不会删桶），不做精确等值。
    #[test]
    fn blocking_clients_are_bucketed_by_timeout() {
        let before = {
            let map = blocking_client_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                !map.contains_key(&(12345, String::new()))
                    && !map.contains_key(&(12346, String::new()))
            );
            map.len()
        };
        let _a = blocking_client(12345, None);
        let _b = blocking_client(12345, None);
        {
            let map = blocking_client_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                map.contains_key(&(12345, String::new())),
                "bucket must be cached"
            );
            assert!(
                map.len() >= before + 1,
                "same timeout must share one bucket"
            );
        }
        let _c = blocking_client(12346, None);
        {
            let map = blocking_client_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                map.contains_key(&(12346, String::new())),
                "second timeout must get its own bucket"
            );
            assert!(map.len() >= before + 2);
        }
    }

    /// 零/越界超时归一到 1s 桶（超时语义与旧实现一致）。
    #[test]
    fn blocking_client_normalizes_timeout_key() {
        let _a = blocking_client(0, None);
        let map = blocking_client_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            map.contains_key(&(1, String::new())),
            "timeout 0 must normalize to the 1s bucket"
        );
    }

    /// 代理 URL 参与桶键：同一超时下，直连与代理、不同代理互不共享客户端。
    #[test]
    fn clients_are_bucketed_by_proxy() {
        let direct_key = (777, String::new());
        let proxied_key = (777, "http://127.0.0.1:7890".to_string());
        let _direct = blocking_client(777, None);
        let _proxied = blocking_client(777, Some("http://127.0.0.1:7890"));
        let map = blocking_client_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(map.contains_key(&direct_key));
        assert!(map.contains_key(&proxied_key));
    }

    /// ProxySetting.resolve 语义：direct → None；custom → URL；global/缺省 → 全局。
    #[test]
    fn proxy_setting_resolution_semantics() {
        set_global_proxy(Some("http://127.0.0.1:7890".to_string()));
        assert_eq!(
            ProxySetting {
                mode: "global".into(),
                url: None
            }
            .resolve(),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            ProxySetting {
                mode: "direct".into(),
                url: None
            }
            .resolve(),
            None
        );
        assert_eq!(
            ProxySetting {
                mode: "custom".into(),
                url: Some(" http://127.0.0.1:7897 ".into())
            }
            .resolve(),
            Some("http://127.0.0.1:7897".to_string())
        );
        // custom 但 URL 为空 → 视为直连（不静默回退全局，避免误走代理）。
        assert_eq!(
            ProxySetting {
                mode: "custom".into(),
                url: Some("  ".into())
            }
            .resolve(),
            None
        );
        set_global_proxy(None);
        let extra = serde_json::json!({"proxy": {"mode": "global"}});
        assert_eq!(
            ProxySetting::from_extra(&extra)
                .expect("proxy key must parse")
                .resolve(),
            None
        );
        // 缺省 proxy 键 → None（跟随全局，此时全局已清空）。
        assert!(ProxySetting::from_extra(&serde_json::json!({})).is_none());
    }
}

/// 渠道级出站代理设置（存于渠道 `config` JSON 的 `proxy` 键）。
///
/// * `global`（或缺省）→ 跟随全局设置（`network.proxy.*`）；
/// * `direct` → 该渠道强制直连（忽略全局代理）；
/// * `custom` → 使用 `url` 指定的代理（如 `http://127.0.0.1:7890`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxySetting {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl ProxySetting {
    /// 从渠道 config JSON（extra）解析 `proxy` 键；缺省/非法返回 None（视为 global）。
    pub fn from_extra(extra: &serde_json::Value) -> Option<ProxySetting> {
        extra
            .get("proxy")
            .and_then(|v| serde_json::from_value::<ProxySetting>(v.clone()).ok())
    }

    /// 解析为实际代理 URL：None = 直连，Some(url) = 经该代理转发。
    pub fn resolve(&self) -> Option<String> {
        match self.mode.as_str() {
            "direct" => None,
            "custom" => self
                .url
                .clone()
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty()),
            // "global" / 未知值 / 缺省 → 跟随全局
            _ => global_proxy_url(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
    pub model_mapping: serde_json::Value,
    pub extra: serde_json::Value,
    pub timeout_secs: u64,
    /// 渠道级出站代理（来自 config.proxy）；None = 跟随全局。
    #[serde(default)]
    pub proxy: Option<ProxySetting>,
}

impl ChannelConfig {
    /// 该渠道实际使用的出站代理 URL（None = 直连）。
    pub fn proxy_url(&self) -> Option<String> {
        self.proxy
            .as_ref()
            .map(|p| p.resolve())
            .unwrap_or_else(global_proxy_url)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRequest {
    pub model: String,
    pub body: serde_json::Value,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub success: bool,
    pub message: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Prompt tokens served from upstream cache.
    pub cached_tokens: u64,
}

#[async_trait]
pub trait Adaptor: Send + Sync {
    #[allow(dead_code)]
    fn channel_type(&self) -> &'static str;
    #[allow(dead_code)]
    fn default_models(&self) -> Vec<&'static str>;
    #[allow(dead_code)]
    fn default_base_url(&self) -> &str;

    async fn test(&self, config: &ChannelConfig) -> Result<TestResult, anyhow::Error>;

    async fn forward(
        &self,
        request: &ProxyRequest,
        config: &ChannelConfig,
    ) -> Result<(u16, serde_json::Value, Option<TokenUsage>), anyhow::Error>;

    async fn forward_stream(
        &self,
        request: &ProxyRequest,
        config: &ChannelConfig,
    ) -> Result<reqwest::Response, anyhow::Error>;
}

pub fn get_adaptor(channel_type: &str) -> Box<dyn Adaptor> {
    match channel_type {
        "openai" => Box::new(openai::OpenAIAdaptor),
        "deepseek" => Box::new(deepseek::DeepSeekAdaptor),
        "claude" => Box::new(claude::ClaudeAdaptor),
        "gemini" => Box::new(gemini::GeminiAdaptor),
        "custom" => Box::new(custom::CustomAdaptor),
        _ => Box::new(custom::CustomAdaptor),
    }
}
