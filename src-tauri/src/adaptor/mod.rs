pub mod claude;
pub mod custom;
pub mod deepseek;
pub mod gemini;
pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
    pub model_mapping: serde_json::Value,
    pub extra: serde_json::Value,
    pub timeout_secs: u64,
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
