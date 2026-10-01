//! VLM 调用：复用网关渠道调度（与 embedder.rs 同一模式）。
//! 按渠道协议适配请求格式：claude 渠道走 Anthropic Messages 视觉格式（/messages），
//! 其余渠道走 OpenAI Chat Completions 视觉格式（/chat/completions，主流国产视觉模型均兼容）。
//!
//! 与 embedding 的差异：无渠道命中时**不 fallback 全部渠道**——向不支持视觉的
//! 文本模型发图片必失败且浪费 token，直接返回 OCR_NO_VISION_CHANNEL 引导配置。

use base64::Engine;

use crate::core::dispatcher::Dispatcher;
use crate::db::models::Channel;
use crate::db::repository::Repository;

use super::prompt::OCR_PROMPT;
use super::OcrError;

/// 单页识别结果。
#[derive(Debug)]
pub struct OcrPageResult {
    pub markdown: String,
    /// 上游返回的 total_tokens（尽力解析，未返回时为 0）
    pub total_tokens: u64,
}

pub struct VlmOcrClient<'a> {
    repo: &'a Repository,
    model: &'a str, // 如 "qwen-vl-max" / "gpt-4o" / "glm-4v-flash"
    timeout_secs: u64,
}

impl<'a> VlmOcrClient<'a> {
    pub fn new(repo: &'a Repository, model: &'a str) -> Self {
        Self {
            repo,
            model,
            timeout_secs: 120, // 单页默认 120s
        }
    }

    /// 识别单页。内部：选渠道 → 按优先级/权重排序 → 逐渠道尝试（failover）。
    pub async fn ocr_page(&self, jpeg: &[u8], page_no: usize) -> Result<OcrPageResult, OcrError> {
        // 与 embedder.rs 相同：取启用渠道后复用 Dispatcher 的模型匹配逻辑
        let channels =
            self.repo
                .get_enabled_channels()
                .await
                .map_err(|e| OcrError::PageFailed {
                    page: page_no,
                    reason: format!("读取渠道失败: {}", e),
                })?;

        let candidates = Dispatcher::select_channels(&channels, self.model);
        if candidates.is_empty() {
            return Err(OcrError::NoVisionChannel(self.model.to_string()));
        }

        let mut last_err = String::new();
        for channel in &candidates {
            match try_ocr_with_channel(jpeg, self.model, channel, self.timeout_secs).await {
                Ok(result) => {
                    tracing::info!(
                        "OCR 识别成功: channel={}, model={}, page={}, tokens={}",
                        channel.name,
                        self.model,
                        page_no,
                        result.total_tokens
                    );
                    return Ok(result);
                }
                Err(e) => {
                    tracing::warn!(
                        "OCR 渠道 {} 识别第 {} 页失败（model={}）: {} — 尝试下一渠道",
                        channel.name,
                        page_no,
                        self.model,
                        e
                    );
                    last_err = e;
                    continue;
                }
            }
        }

        Err(OcrError::PageFailed {
            page: page_no,
            reason: last_err,
        })
    }
}

async fn try_ocr_with_channel(
    jpeg: &[u8],
    model: &str,
    channel: &Channel,
    timeout_secs: u64,
) -> Result<OcrPageResult, String> {
    let base_url = channel.base_url.trim_end_matches('/');
    let image_data = base64::engine::general_purpose::STANDARD.encode(jpeg);

    // 应用模型映射（与 embedder.rs 同一语义）
    let actual_model = super::super::resolve_channel_model(channel, model);

    // 按渠道协议构造请求：claude 渠道走 Anthropic Messages 视觉格式（/messages + x-api-key），
    // 其余渠道走 OpenAI Chat Completions 视觉格式（/chat/completions + Bearer）
    let is_claude = channel.channel_type == "claude";
    let (url, body) = if is_claude {
        (
            format!("{}/messages", base_url),
            serde_json::json!({
                "model": actual_model,
                "max_tokens": 4096,
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": image_data } },
                            { "type": "text", "text": OCR_PROMPT }
                        ]
                    }
                ]
            }),
        )
    } else {
        (
            format!("{}/chat/completions", base_url),
            serde_json::json!({
                "model": actual_model,
                "stream": false,
                "max_tokens": 4096,
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            { "type": "text", "text": OCR_PROMPT },
                            { "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{}", image_data) } }
                        ]
                    }
                ]
            }),
        )
    };

    let client = reqwest::Client::new();
    let mut req = client.post(&url).header("Content-Type", "application/json");
    if is_claude {
        req = req
            .header("x-api-key", &channel.api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("Authorization", format!("Bearer {}", channel.api_key));
    }
    let resp = req
        .json(&body)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "HTTP {}: {}",
            status,
            text.chars().take(300).collect::<String>()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Parse response failed: {}", e))?;

    // 响应解析：claude 为 content[] 文本块数组；OpenAI 为 choices[0].message.content（兼容多段数组）
    let content = if is_claude {
        json.get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        match json.pointer("/choices/0/message/content") {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
                .trim()
                .to_string(),
            _ => String::new(),
        }
    };

    // 空内容或过短视为识别失败（触发重试/failover）
    if content.chars().count() < 10 {
        return Err(format!(
            "识别结果为空或过短（{} 字符）",
            content.chars().count()
        ));
    }

    // token 用量：claude 为 usage.input_tokens + output_tokens；OpenAI 为 usage.total_tokens
    let total_tokens = if is_claude {
        let input = json
            .pointer("/usage/input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = json
            .pointer("/usage/output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        input + output
    } else {
        json.pointer("/usage/total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };

    Ok(OcrPageResult {
        markdown: content,
        total_tokens,
    })
}
