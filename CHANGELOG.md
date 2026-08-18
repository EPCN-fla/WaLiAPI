# Changelog

## v0.2.1

### 协议转换层结构化重构

- 🔧 **protocol 模块目录化**：将 protocol 根转换逻辑拆分为独立子模块——codec/chat、codec/messages、codec/responses_codec、directions（messages_to_responses / responses_to_messages），每个方向独立 encode/decode/stream/test，消除 1500 行巨型文件
- 🔧 **死代码清理与 API 收敛**：清理 protocol 模块遗留 API 和死代码，clippy 告警归零，完成模块结构与 re-export 审计
- 🔧 **codec 加固**：移植 tool-call 回放保留空 reasoning_content 兼容性优化，修复测试编译问题，全仓 cargo fmt 格式化
- 📝 **重构方案文档**：新增 protocol 模块结构化重构实现方案文档

### Kimi Code Auth 账号接入

- ✨ **Kimi 设备 OAuth 登录**：实现 Kimi 设备授权流程（device code → 授权 → token），支持 token 自动刷新
- ✨ **Provider 中立认证框架**：新增 provider metadata + model protocol snapshot，支持多登录方式扩展
- ✨ **认证路由集成**：model-level auth profiles 传入 prepared attempts，executor 注册 Kimi 认证尝试
- ✨ **登录会话管理**：provider-neutral login sessions and commands，通用 login context 与 locked replacement 持久化
- ✨ **协议感知模型发现**：Kimi 后端协议感知的模型发现与注册
- ✨ **前端 Auth 面板**：Kimi auth login UI + provider-aware accounts 页面
- 🐛 **402 订阅无效终态处理**：402 订阅无效分为终态，不再 12h 死循环重试
- 🐛 **令牌失效原因记录**：invalidation_reason 记录并透出到 DTO，失效账号卡片显示具体失效原因
- 🐛 **渠道页账号过滤修复**：渠道页按 provider 过滤账号卡片，不再混显
- ✅ **测试覆盖**：Kimi routing replacement refresh 与协议流程测试，clippy lint 修复

### 审计日志流式响应修复

- 🐛 **流式响应内容记录修复**：流式请求的审计日志中 `response_choices` 字段此前始终为空，现已正确记录响应内容（content / reasoning_content / tool_calls），与非流式路径行为一致
- 🔧 **多协议流式累积**：新增 SSE 事件解析器，支持三种流式协议的响应内容累积：
  - OpenAI Chat Completions（`choices[].delta.content` / `reasoning_content` / `tool_calls`）
  - Anthropic Messages（`content_block_delta` 的 `text_delta` / `thinking_delta` / `input_json_delta`）
  - OpenAI Responses API（`response.output_text.delta` / `response.completed`）
- 🔧 **StreamPumpCore 扩展**：新增 `accumulated_reasoning`、`response_role`、`finish_reason`、`tool_calls_map` 字段，`build_response_choices()` 方法从累积内容构建标准 JSON

### 其他

- 版本号统一升级至 0.2.1（package.json / Cargo.toml / tauri.conf.json）
- 121 个文件变更，+22,616 / -14,462 行代码

## v0.1.9

- 渠道多 Key 负载均衡：单个渠道配置多个 API Key，按权重随机选择，分散并发压力
- 渠道复制快捷配置：一键复制现有渠道配置，快速创建相似渠道
- 审计日志自动刷新：页面可见时每 5 秒静默轮询，新日志自动出现，无需手动刷新
- 自动更新 Release Notes 动态化：从 CHANGELOG.md 自动提取版本说明，不再显示固定文案
- 版本号统一升级至 0.1.9（package.json / Cargo.toml / tauri.conf.json）

## v0.1.8

- API 密钥黑白名单
- Auth 账号模型映射
- 路由优先级修复
- Usage 密钥过滤
- API Key 编辑功能

## v0.1.5

- 模型映射一对多
- 渠道超时配置
- proxy.rs P0 修复
- IME composing 修复
- 拖拽排序修复

## v0.1.3

- 符号感知分块（AST）
- FTS5 混合检索
- MCP server instructions
- 知识库标签

## v0.1.1

- 多协议网关
- 仪表盘优化
- 渠道统计
- 接入示例

## v0.1.0

- 首发版本
- 多渠道 + 密钥 + 日志 + 安全审计 + SSE 流式
