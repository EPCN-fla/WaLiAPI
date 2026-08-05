// ────────────────────────────────────────────────────────────────────────────
// 纯展示辅助函数与显示映射。
//
// 注意：这里【不】保存任何渠道/提供商/模型/URL 模板副本。URL、模型建议、
// 端点能力、地区分组的唯一可信源是后端 registry（`get_channel_presets`，
// T01），前端只消费它返回的数据。此处仅保留 UI 展示所需的字符串/图标映射。
// ────────────────────────────────────────────────────────────────────────────

import type { DraftEndpointTestFailureCategory } from "../types";

// 地区分组标签（产品分组，非部署地域判断）。
export const CHANNEL_CATEGORIES: Record<string, { label: string; icon: string }> = {
  international: { label: "国际", icon: "🌍" },
  domestic: { label: "国内", icon: "🇨🇳" },
  local: { label: "本地", icon: "💻" },
  custom: { label: "自定义", icon: "⚙️" },
};

// 协议显示名。
export const PROTOCOL_LABELS: Record<string, string> = {
  openai: "OpenAI",
  anthropic: "Anthropic",
  ollama: "Ollama",
};

// provider → 显示名。未知 provider 统一显示“自定义”（设计 3.5）。
export const CHANNEL_PROVIDER_LABELS: Record<string, string> = {
  openai: "OpenAI",
  google: "Google",
  deepseek: "DeepSeek",
  qwen: "通义千问",
  zhipu: "智谱 GLM",
  doubao: "字节豆包",
  doubao_coding_plan: "字节豆包（Coding Plan）",
  moonshot: "Moonshot AI",
  anthropic: "Anthropic",
  ollama: "Ollama（本地）",
  custom: "自定义",
};

// provider → 图标 key（来自 registry `icon_key`）。
export const CHANNEL_PROVIDER_ICONS: Record<string, string> = {
  openai: "🟢",
  google: "💎",
  deepseek: "🐋",
  qwen: "🔮",
  zhipu: "✨",
  doubao: "🫘",
  doubao_coding_plan: "🫘",
  moonshot: "🌙",
  anthropic: "🤖",
  ollama: "🦙",
  custom: "⚙️",
};

// 端点失败分类显示名（T07 failure category）。
export const ENDPOINT_TEST_CATEGORY_LABELS: Record<DraftEndpointTestFailureCategory, string> = {
  network: "网络不可达",
  timeout: "超时",
  authentication: "鉴权失败",
  endpoint_unsupported: "端点不支持",
  model: "模型错误",
  request: "请求被拒绝",
  protocol: "协议错误",
  unknown: "未知错误",
};

// 端点显示名（协议配置区使用）。
export const ENDPOINT_LABELS: Record<string, string> = {
  chat_completions: "Chat Completions",
  responses: "Responses",
  messages: "Messages",
  count_tokens: "Count Tokens",
  embeddings: "Embeddings",
  api_chat: "/api/chat",
};

export function getChannelProviderLabel(provider: string): string {
  return CHANNEL_PROVIDER_LABELS[provider] || "自定义";
}

export function getProtocolLabel(protocol: string): string {
  return PROTOCOL_LABELS[protocol] || protocol || "旧配置";
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

export function formatNumber(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

export function formatTime(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleString("zh-CN", { hour12: false });
}
