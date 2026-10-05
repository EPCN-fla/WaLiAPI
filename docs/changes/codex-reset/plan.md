# Codex 重置卡：实现计划

**日期**：2026-10-02
**设计基准**：[design.md](design.md)
**当前状态**：核心链路已落地；验证与未闭合项见 [execution.md](execution.md)。

## 1. 端到端链路

```text
AccountCard（账号 A）
  └─ authApi.listResetCredits(A)
       └─ auth_list_reset_credits
            └─ AuthService::list_reset_credits(A)
                 └─ CodexProvider::list_reset_credits
                      └─ GET /backend-api/wham/rate-limit-reset-credits

ResetCreditDialog 右侧选择卡 C + 底部确认
  └─ authApi.consumeResetCredit(A, C, operation)
       └─ auth_consume_reset_credit
            └─ AuthService::consume_reset_credit(A, C, operation)
                 ├─ 读取 A 最新账号与 payload
                 ├─ GET 卡列表并校验 C
                 ├─ 写入 auth_reset_operations.pending
                 ├─ CodexProvider::consume_reset_credit
                 │    └─ POST /backend-api/wham/rate-limit-reset-credits/consume
                 ├─ 持久化结果 code
                 ├─ reset/already_redeemed → refresh_quota(A)
                 └─ 返回安全结果 + quota 回读状态
```

## 2. 后端接口设计

### 2.1 Provider DTO

实现位于 `codex_backend.rs` 的 provider 私有解析函数，再转换成服务层安全 DTO：

```rust
CodexResetCredit {
    id: String,
    reset_type: String,
    status: String,
    granted_at: Option<String>,
    expires_at: Option<String>,
    title: Option<String>,
    description: Option<String>,
}

CodexResetCreditsSnapshot {
    available_count: Option<i64>,
    credits: Vec<CodexResetCredit>,
}

CodexResetCreditOutcome {
    code: ResetCreditCode,
    windows_reset: i64,
}
```

wire DTO 允许新增字段但不把未知字段当作成功；服务层只输出安全摘要。`expires_at` 兼容 Unix 秒和 RFC3339 字符串，禁止在 provider 层提前格式化为本地日期。

### 2.2 Provider 方法

`Provider` trait 已增加默认不支持实现：

```rust
async fn list_reset_credits(
    &self,
    account: &AuthAccount,
    payload: &ProviderPayload,
) -> Result<ResetCreditsSnapshot, ProviderError>;

async fn consume_reset_credit(
    &self,
    account: &AuthAccount,
    payload: &ProviderPayload,
    request_id: &str,
    credit_id: &str,
) -> Result<ResetCreditOutcome, ProviderError>;
```

`CodexProvider` 使用现有 `auth_headers`，保留 `Authorization`、`chatgpt-account-id`、`originator` 和 User-Agent；消费接口使用独立的非流式超时。调用方传入的 Authorization 和 actor header 不得被转发。

### 2.3 Service 方法

`AuthService` 已新增：

- `list_reset_credits(account_id)`：能力和账号状态检查后查询并转换安全 DTO；
- `consume_reset_credit(account_id, credit_id, operation_id)`：加账号锁，读取/创建 pending，重新查询卡列表，验证归属和有效期，消费并持久化结果；
- `resume_reset_operation(account_id, operation_id)`：只读取既有幂等键恢复，不自动选卡。

`AuthService::refresh_quota` 保持原签名和行为。消费成功后直接调用它，不复制 `fetch_quota`、`quota_from_usage_payload` 或 repository quota 写入逻辑。

## 3. 数据库设计

迁移 `src-tauri/migrations/045_auth_reset_operations.sql` 已落地：

```sql
CREATE TABLE auth_reset_operations (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  credit_id_hash TEXT NOT NULL,
  redeem_request_id TEXT NOT NULL,
  status TEXT NOT NULL,
  upstream_code TEXT,
  error_class TEXT,
  quota_refresh_status TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (account_id, redeem_request_id)
);
CREATE INDEX idx_auth_reset_operations_account
  ON auth_reset_operations(account_id, updated_at DESC);
```

状态由服务层写入：`pending`、`reset`、`already_redeemed`、`nothing_to_reset`、`no_credit`、`unknown`、`failed`。数据库不保存完整卡 ID；恢复命令只读取已有操作，不重放消费请求。

## 4. 命令与 Web 管理面

新增 Tauri commands：

| 命令 | 输入 | 返回 |
|---|---|---|
| `auth_list_reset_credits` | `{ id }` | 卡列表安全摘要、available count、fallback 状态 |
| `auth_consume_reset_credit` | `{ id, creditId, operationId? }` | 操作结果、quota refresh 状态、卡列表刷新提示 |
| `auth_resume_reset_operation` | `{ id, operationId }` | 原操作当前状态与可继续动作 |

命令在 `src-tauri/src/lib.rs` 注册，并在 `admin_routes.rs` 的 invoke 分发中使用相同命令名。所有 Web 请求继续使用现有管理会话和 CSRF 保护。

## 5. 前端设计

### 5.1 类型与 API

在 `src/types/index.ts` 增加 `AuthResetCredit`、`AuthResetCreditsSnapshot`、`AuthResetOperationResult`、`AuthResetCapability`。在 `src/lib/api.ts` 增加：

```ts
listResetCredits(id: string)
consumeResetCredit(id: string, creditId: string, operationId?: string)
resumeResetOperation(id: string, operationId: string)
```

只能调用 `runtime.ts` 的 `invoke`，不得直接访问 ChatGPT URL。

### 5.2 账号卡与弹窗

`AccountCard` 只负责展示入口和回调；新增 `ResetCreditDialog` 负责：

1. 加载并显示目标账号标签；
2. 展示标题、描述、获得时间、过期时间、状态和可用数量；
3. 用原始时间值过滤可提交卡；
4. 二次确认并显示不可逆提示；
5. 消费期间禁用按钮；
6. 显示四类结果、待确认状态和 fallback；
7. 成功后刷新账号卡片、额度和重置卡列表。

前端按已确认的产品视觉稿和项目现有 Tailwind、lucide-react、样式令牌实现右侧选卡弹窗、背景列表、卡片单选、底部提示和确认按钮；后端接口、消费前二次查询、幂等和额度回读保持不变。

其他 provider、API Key、无效/停用账号不渲染入口。账号 A 的弹窗状态不能由账号 B 的刷新结果覆盖。

## 6. 分阶段开发

### 阶段 0：契约冻结

- 新增 provider DTO、结果枚举和 capability。
- 固定请求路径、认证头 allowlist、超时和未知 code 处理。
- 完成 provider mock contract 后再进入数据库实现。

### 阶段 1：数据与服务

- 新增 045 迁移、models 和 Repository 操作。
- 实现账号校验、消费锁、幂等 pending、结果持久化。
- 接通消费后的现有 quota 回读。

### 阶段 2：命令和管理面

- 注册 Tauri commands。
- 增加 Web admin invoke 分发。
- 完成安全 DTO 和错误分类。

### 阶段 3：前端闭环

- 增加 TypeScript 类型和 API 封装。
- 完成 AccountCard 入口、ResetCreditDialog 和结果状态。
- 接入官方 Usage fallback 与手动刷新额度。

### 阶段 4：必要测试和回归

- 完成 Rust provider/service/repository/command 测试和前端构建。
- 执行必要的 provider/service/command 和前端回归，确认原功能不受影响。
- 指定账号先完成卡列表核对，再只做一次明确卡消费 smoke test。
- 测试通过后直接进入开发版本验收，不设计灰度发布流程。

## 7. 回滚

1. 停止新增消费命令或恢复上一版本代码。
2. 保留 `auth_reset_operations` 记录，禁止重复消费。
3. 入口显示官方 Usage 链接。
4. 保持 `auth_refresh_quota`、模型同步、令牌刷新和路由不变。
5. 消费已确认但额度未回读的操作不回滚远端结果，只允许稍后使用原账号刷新。


## 实施任务与当前状态

## 阶段 0：契约和能力

- [x] T00.1 在 `codex_backend.rs` 增加重置卡列表、消费请求和响应 wire DTO；验证 mock 请求命中两个精确路径。
- [x] T00.2 实现认证头 allowlist、独立超时和四类结果 code 映射；验证 Bearer、`chatgpt-account-id`、`originator` 正确且调用方 Authorization 不被转发。
- [x] T00.3 在 `Provider` trait 增加默认 UnsupportedFeatures 能力，在 `ProviderSpec` 增加 `supports_reset_credit`；验证只有 Codex 为 true。
- [x] T00.4 增加接口不可用时的安全 fallback；验证不发送重复消费请求且返回固定 Usage URL。

## 阶段 1：数据和 Repository

- [x] T01.1 新增 `src-tauri/migrations/045_auth_reset_operations.sql`；验证旧数据库升级、内存 SQLite 建表和唯一索引。
- [x] T01.2 在 `db/models.rs` 定义 reset operation 状态、错误分类和安全摘要；验证序列化不含 payload、完整卡 ID 或认证字段。
- [x] T01.3 在 `db/repository.rs` 增加 pending 创建、幂等读取、状态更新和未完成操作查询；验证账号隔离、重复创建和应用重启恢复。
- [x] T01.4 用单向哈希保存卡标识并补齐索引；验证日志和数据库字段无法还原或输出完整卡 ID。

## 阶段 2：AuthService 编排

- [x] T02.1 实现 `list_reset_credits(account_id)`；验证 API Key、其他 provider、失效、停用和不存在账号在发网前失败。
- [x] T02.2 实现服务端二次卡列表校验；验证跨账号卡、过期卡、非 `available` 卡和非 `codex_rate_limits` 卡不会发送 POST。
- [x] T02.3 实现每账号消费锁、逻辑操作 ID 和 UUID `redeem_request_id`；验证同账号重复点击只产生一个上游请求，不同账号互不阻塞。
- [x] T02.4 实现 `consume_reset_credit` 的四类结果持久化和安全提示；验证未知 code 不被当作成功。
- [x] T02.5 实现超时/断连的 pending 或 unknown 恢复；验证恢复只复用原幂等键，不换卡、不生成新键。
- [x] T02.6 在 reset/already_redeemed 后调用现有 `refresh_quota`；验证只更新目标账号，回读失败时旧 `quota_json` 保留。
- [x] T02.7 增加日志脱敏和审计字段；验证 token、Cookie、Authorization、上游正文和完整卡 ID 不出现在日志、错误或事件中。

## 阶段 3：Tauri 与 Web 管理面

- [x] T03.1 在 `commands/auth.rs` 增加 `auth_list_reset_credits`、`auth_consume_reset_credit`、`auth_resume_reset_operation`；验证只返回安全 DTO。
- [x] T03.2 在 `lib.rs` 注册新命令；验证桌面 MockRuntime 可调用，既有 `auth_refresh_quota` 未改变。
- [x] T03.3 在 `server/admin_routes.rs` 增加 invoke 分发；验证 Web 管理会话、CSRF、参数和返回结构与 Tauri 一致。
- [x] T03.4 增加固定官方 Usage fallback；验证 URL 无 query、fragment、邮箱、token 或内部账号 ID。

## 阶段 4：前端闭环

- [x] T04.1 在 `src/types/index.ts` 增加 capability、卡摘要、快照、操作结果和错误类型；验证与 Rust DTO 的字段一致。
- [x] T04.2 在 `src/lib/api.ts` 增加三个 auth API 方法并通过 `runtime.ts` invoke；静态检索验证无裸 fetch/直接 Tauri invoke。
- [x] T04.3 在 `AccountCard.tsx` 为有效 Codex OAuth 账号增加入口；验证 Kimi、Gemini、Grok、API Key、停用和失效账号不显示消费按钮。
- [x] T04.4 按已确认的产品视觉稿复原右侧选卡弹窗；验证默认选中首张可用卡、按原始时间值过滤卡、显示本地时区、取消不发 POST、重复点击被禁用。后端消费链路不改动。
- [x] T04.5 在 `AuthChannelsPage.tsx` 串接查询、确认、结果、额度刷新和卡列表刷新；验证账号 A 的操作不会改变账号 B。
- [x] T04.6 前端确认时生成并复用当前任务的 `operationId`，未知结果锁定任务状态；后端只读恢复命令保留，不增加历史结果查询。

## 阶段 5：测试和回归

- [x] T05.1 provider mock contract 覆盖 GET/POST 路径、请求体、认证头、四类结果和未知 code；运行 Codex provider 聚焦测试。
- [x] T05.2 service + Repository 聚焦测试覆盖账号隔离、状态校验、消费锁、幂等恢复、045 迁移和 quota 回读失败；运行对应 Rust 测试。
- [~] T05.3 provider command/admin 已接入，前端三态、取消、重复点击、任务状态和 operationId 传递已实现；尚未建立独立的 command/admin 与 React 自动化 smoke test。
- [~] T05.4 `cargo test --no-fail-fast`（1149 个库测试及全部集成测试）、必要 Rust 聚焦测试和 `pnpm build` 已通过；`cargo fmt --check` 仍被基线文件 `src-tauri/src/endpoint_executor/grok_arguments.rs` 的既有格式差异阻塞。
- [x] T05.5 在指定验收账号执行只读列表 smoke test；存在 3 张可用卡，唯一目标为 UTC 2026-10-04 的 `codex_rate_limits` 卡。
- [x] T05.6 从桌面端选择 UTC 2026-10-04 到期卡并执行唯一一次真实消费；上游返回 `reset`，额度回读 `refreshed`，验收结束。

## 阶段 6：必要回归和交付

- [~] T06.1 已核对登录、刷新令牌、模型同步、额度刷新和 `/v1/*` 的调用边界未被重置命令直接改写；完整回归矩阵仍待补跑。
- [x] T06.2 完成官方 Usage fallback 和 pending 恢复演练；验证不会重复消费或清空 quota。
- [x] T06.3 已按当前代码、验证结果和未完成项更新本目录文档。

## 每个任务的交付记录

任务完成时在变更记录中写明：修改文件、接口变化、测试命令与结果、兼容性影响、未决风险。未经 Xerina 明确要求，不提交、推送或发布。
