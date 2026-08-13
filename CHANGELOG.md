# Changelog

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
