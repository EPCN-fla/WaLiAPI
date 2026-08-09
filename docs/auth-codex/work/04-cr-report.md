# CR 报告（Round 1）

## 结论：FAIL

## 问题清单

- **主要** / `src-tauri/src/core/attempt.rs:231`、`src-tauri/src/core/attempt.rs:308` / 转换 codec 按 `RouteGroup` 第一候选的 `upstream_protocol` 选择，而不是按当前 `RouteGroupCandidate` 选择。当前 `build_route_plan` 只按 Native/Conversion tier 分组，因此 Chat 的同一 Conversion 组可以同时包含 Anthropic Channel 与 Responses Auth Account，Messages 组也可以同时包含 Chat Channel 与 Responses Auth Account。只要首候选失败并降级到另一协议候选，第二次 attempt 仍沿用首候选方向编码：例如把 Chat 请求编码成 Responses body 后发给 Anthropic `/messages`，混合候选降级主流程会稳定失败。/ 将 `codec_direction` 改为接收当前 candidate（或其 `upstream_protocol`），所有请求编码、`codec_version` 与 SSE mode 均以当前候选为准；补 Chat/Message 各一条“不同上游协议同组、首候选失败、第二候选成功”的流式与非流式测试。

- **主要** / `src-tauri/src/auth_provider/service.rs:249` / 出站前懒刷新使用 `self.refresh_account(account_id).await?` 直接返回错误；刷新失败时没有把账号置为 `invalid`。这与 ADR-10“刷新失败则账号置失效，路由跳过”冲突，失效 refresh token 的账号会继续保持 `active` 并被每个请求反复选中。401 后的强制刷新分支会标失效，但请求前到期/临期刷新失败不会。/ 捕获懒刷新错误；对凭据拒绝/无效 refresh token 标记 `invalid`（并按既定规则写 `next_retry_after`），再返回脱敏失败。补“临期 token + refresh 失败”测试，断言零 `/responses` 请求、账号状态为 `invalid`、后续候选加载会过滤该账号。

- **主要** / `src-tauri/src/protocol/codec/responses_codec.rs:818`、`src-tauri/src/protocol/codec/responses_codec.rs:848` / Responses→Chat 流状态机只处理 `response.output_item.added` 与 `response.function_call_arguments.delta`，完全忽略 `response.function_call_arguments.done`/`response.output_item.done` 中的最终 arguments。若上游只在 done 事件给出完整参数（或 delta 缺失/不完整），下游只收到 `finish_reason=tool_calls`，却没有可执行的 tool call；同时也未在 done 时校验最终 arguments JSON。/ 在 done 事件用 `output_index`/`item_id` 合并并补齐 call id、name、最终 arguments，校验 arguments 是合法 JSON，确保每个完成的 function call 至少输出一次完整可执行的 Chat tool call；补“只有 done 无 delta”“delta+done”“done 参数非法”以及 Messages 组合路径测试。

- **主要** / `src-tauri/src/server/handlers.rs:119`、`src-tauri/src/server/handlers.rs:129` / Auth rollout 开关只判断数据库中是否存在任意非空模型快照账号，没有判断该账号是否匹配当前请求的 model/endpoint。全局 `new_routeplan=false` 时，只要另一个模型有 Auth 账号，就会把当前请求强制送入 RoutePlan；随后普通 Channel 仍受 `native_responses/cross_protocol_codec` 关闭约束，导致原本应走 legacy 成功的 Responses/转换请求被错误拒绝。/ 在决定强制 rollout 前按当前 model、账号状态/quota/模型快照以及 endpoint 能力计算 request-scoped Auth 候选；只有当前请求确有可用 Auth Account 才强制 RoutePlan，否则保持 legacy。补“账号仅支持 model-A，请求 model-B，flags 全关”对 Responses 和 Messages 的回归测试。

- **主要** / `src-tauri/src/core/plan_executor.rs:189`、`src-tauri/src/endpoint_executor/driver.rs:328`、`src-tauri/src/endpoint_executor/driver.rs:690` / Auth Account 所有候选失败时，`FlowStep::Halt` 丢弃最后一次 attempt 的候选元数据，流式和非流式失败日志随后走通用 pre-commit writer，并硬编码 `upstream_type="channel"`。因此典型的 401→刷新→401 或账号 5xx 全部耗尽会被记成 API Channel，违反 ADR-30，账号 id/name 也丢失。/ 在 `AttemptFlow`/`PlanExecution` 保留最后尝试的 candidate meta，失败日志写实际 `upstream_type/id/name/provider/codec`；没有发生任何 attempt 的规划前拒绝才使用无候选值。补单账号最终失败的 stream/non-stream 日志断言。

- **次要** / `src-tauri/src/auth_provider/codex_login.rs:371`（首个新增代码 lint；完整命令涉及更多位置）/ 任务卡 T12 要求 `cargo clippy --all-targets -- -D warnings` 通过，当前命令失败（本次执行报告 174 个错误，包含新增 Auth/Codec 代码的 `type_complexity`、`nonminimal_bool`、dead code/unused 等，也包含既有模块告警）。这不直接改变运行时结果，但验收命令未达标。/ 清理本次新增告警；若仓库既有告警不在本轮修复范围，应先建立明确的 clippy baseline/allow 策略，再保证本次 diff 不新增告警并让任务卡中的实际门禁命令可重复通过。

## 通过项摘要

- 数据迁移包含 `auth_accounts`、`UNIQUE(provider, account_id)`、路由索引及 `request_logs.upstream_type DEFAULT 'channel'`；upsert 会保留 id/label/P/W/disabled 并恢复 `active`。
- OAuth PKCE S256、随机 localhost 回调、state/一次性交换、5 分钟 timeout、系统浏览器 opener 均已接线；auth.json 按真实嵌套 `tokens` 形状解析，opaque refresh token 未被解码。
- auth.json 写回具备前端覆盖确认、备份、同目录临时文件、`fsync`、0600 与原子 rename；不会修改 `config.toml`。
- Provider trait/registry/AuthService 已落地；401 的一次刷新重试位于账号适配器内部，未改 AttemptFlow 的逻辑 attempt 计数。
- backend-api 固定 `/responses`、`/models`，Bearer 注入、调用方鉴权头剥离、`stream:true`、顶层 allowlist、无 zstd 已实现。
- QuotaState 支持动态 limit id、primary/secondary、429 Retry-After/退避、最晚恢复点；账号 disabled/invalid/quota/空模型过滤与 `allowed_channels` 豁免已实现。
- Responses/Chat/Messages 三协议的流式和非流式基本链路、Native Responses usage 完整 record 扫描、rate_limits side-band 透传均已接线。
- 10 条 Tauri Auth 命令已注册，renderer DTO 不包含 access/refresh/id token 或 `payload_json`；RoutePlan `debug_json` 只含安全账号标识。
- 前端 `/channels/auth`、Sidebar 精确 active、风险文案、账号三态、模型/限额展示、编辑/启停/删除/刷新/同步/写回及覆盖确认已实现。
- 本次验证：`cargo fmt --check` 通过；`cargo test` 通过（426 个库测试 + integration tests）；`npm run build` 通过。

## 未核对项（如有，说明原因）

- 未使用真实 ChatGPT/Codex 订阅令牌访问生产 `chatgpt.com`，因此 OAuth/模型列表/backend-api 的生产端兼容性仍属于需求书 §2.3 的真实令牌待验证项；本轮仅核对本地 mock 与静态实现。
