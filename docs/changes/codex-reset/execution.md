# Codex 重置卡：执行记录

**复核日期**：2026-10-02
**分支**：`add-reset`
**状态**：IMPLEMENTED

## 架构结论

当前实现符合 WaLiAPI 的分层边界：

```text
React Auth 页面
  → src/lib/api.ts / runtime.ts
  → Tauri command 或 /admin/api/invoke
  → AuthService
  → Provider trait
  → CodexProvider
  → ChatGPT backend-api
```

- 重置能力挂在 `Provider` 可选能力上，非 Codex provider 默认返回 `UnsupportedFeatures`。
- 账号状态、provider 能力、卡归属、状态、类型和过期时间均在 `AuthService` 发网前再次校验。
- 一次性消费写入 `auth_reset_operations`，卡标识只保存 SHA-256，`quota_json` 不承担操作状态。
- 消费后的额度回读调用既有 `AuthService::refresh_quota`，没有复制额度解析和持久化逻辑。
- 桌面端与 headless Web 端使用同一组命令和 DTO，前端请求继续经过 `runtime.ts`。

## 代理与客户端复用

重置卡列表、消费和额度回读均在 `CodexProvider` 中调用：

- `crate::adaptor::global_proxy_url()` 读取设置页同步的全局代理；
- `crate::adaptor::blocking_client(30, proxy)` 复用现有非流式连接池；
- Codex Responses 流式请求继续使用 `streaming_client(proxy)`。

因此没有新增重置专用代理、固定端口或第二套 HTTP transport。模型同步和 endpoint executor 同时修正为：渠道未填写 `config.proxy` 时回退全局代理，显式 `direct` 仍保持直连。

## 结果与回归

已验证：

- `cd src-tauri && cargo test reset_credit --no-fail-fast`：3 个 provider contract/parser 测试通过；
- `cd src-tauri && cargo test --no-fail-fast`：1149 个库测试及全部集成测试通过；
- `pnpm build`：TypeScript 检查和 Vite 构建通过；
- `git diff --check origin/main...HEAD`：通过；
- `cargo fmt --check`：仅被未改动的基线文件 `src-tauri/src/endpoint_executor/grok_arguments.rs` 格式差异阻塞。

未发现重置改动直接破坏登录、刷新令牌、模型同步、额度刷新或 `/v1/*` 路由的调用边界。

## 尚未闭合项

前端现在在确认时生成 UUID 格式的 `operationId`，将其传入消费命令，并在当前弹窗中展示任务状态。消费请求异常时锁定当前任务，不生成新幂等键、不自动重试；后端仍负责持久化 `unknown` 并保证同一操作不会重复消费。按本次范围不增加历史操作查询或历史记录页面。

## 启动故障修复

重置卡迁移统一使用 `045_auth_reset_operations.sql`，不在运行时代码中保留旧 043 兼容分支。本机数据库已完成一次性迁移整理，当前记录为知识库 043、渠道 044、重置卡 045；之后启动只执行标准 SQLx 迁移链。

当前 GitHub `latest` 的 v0.3.8 macOS 包仍来自未包含重置功能的旧构建，运行在已执行 045 的数据库上会报 `VersionMissing(45)`。本地新包已验证正常；远程下载要恢复可用，需使用包含 045 的提交重新发布带签名的版本包。


## 验证证据

- `cd src-tauri && cargo test --no-fail-fast`：1149 个库测试及全部集成测试通过。
- `cargo test reset_credit --no-fail-fast`：provider contract/parser 聚焦测试通过。
- `pnpm build`：TypeScript 检查和 Vite 构建通过。
- `git diff --check origin/main...HEAD`：通过。
- `cargo fmt --check`：仅被未修改的基线文件 `src-tauri/src/endpoint_executor/grok_arguments.rs` 格式差异阻塞。
- 指定账号的只读列表与唯一一次真实消费已完成；上游返回 `reset`，额度回读为 `refreshed`，未重试、未换卡、未再次消费。

## 保留限制

- 前端 command/admin 与 React 自动化 smoke test 尚未建立，浏览器手动链路已实现但未形成独立自动化门禁。
- 登录、刷新令牌、模型同步、额度刷新和 `/v1/*` 的完整回归矩阵仍待补跑。
- 迁移已统一为 `045_auth_reset_operations.sql`；远程旧版本包不包含该迁移，使用旧包打开已执行 045 的数据库会报 `VersionMissing(45)`，需要用包含 045 的提交重新发布。
