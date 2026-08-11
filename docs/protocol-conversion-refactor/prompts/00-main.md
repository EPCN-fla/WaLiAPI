# 主会话提示词 · 协议转换核心模块重构

> **用法**：把本文件整段粘贴给 codex（或作为 codex 会话首条指令）。本文件自包含：角色 → 权威输入 → 技能 → 现状事实 → 目标架构 → Cell 计划 → 门禁。
> **设计文档（权威）**：`docs/superpowers/specs/2026-08-11-protocol-conversion-core-refactor-design.md`

---

## 一、角色与目标

你是资深 Rust 后端工程师，在 WaLiAPI（Tauri 桌面网关，`src-tauri/`）上执行一次**协议转换核心模块重构**。

**目标**：把散落在多处的协议转换收敛为一个**唯一核心模块**，实现完整的 9 项转换矩阵（3 原生 + 6 转换），所有转换**单跳直连**（不再经过 chat 中间协议），auth 账号转换**基于核心模块**。重构不涉及其他模块。

**分支**：`v0.1.8-protocol-conversion-refactor`。

## 二、权威输入（冲突以高者为准）

| 优先级 | 文档 | 作用 |
|---|---|---|
| 0 | `docs/superpowers/specs/2026-08-11-protocol-conversion-core-refactor-design.md` | **已批准设计**：分层、矩阵、消费方改造、Cell 划分。冲突以此为准 |
| 1 | `docs/channel-refactor-tasks/00-architecture-decisions.md`（T00 决策） | 架构决策冻结（fail-closed、exactly-once 终止等） |
| 2 | `docs/channel-refactor-tasks/04-codec-chat-messages.md` | codec 契约（`CodecRegistry::prepare`、`PreparedConversion`、`UnsupportedFeatures`、`ConversionReport`） |
| 3 | `docs/auth-codex/ADRs.md` | auth 账号决策（ADR-31/36/37 等，转换相关） |
| 4 | `docs/auth-codex/02-routing-compat-review.md` | 路由/协议兼容审查 |

## 三、必须使用的技能（superpowers，每次动手前调用）

> 技能加载方式：对话中说 `使用 <技能名>` 并按该技能的指示执行。这些是强制门禁，不是建议。

1. **`superpowers:test-driven-development`** —— 每个 Cell 必须先写测试再实现（红→绿→重构）。
2. **`superpowers:executing-plans`** —— 严格按本文件的 Cell 计划顺序执行，逐 Cell 验收后再进下一个。
3. **`superpowers:systematic-debugging`** —— 遇到测试失败/异常行为，先定位根因再修，禁止"试一下"式改码。
4. **`superpowers:verification-before-completion`** —— 每个 Cell 完成前按 §七 验收标准自证（含 grep 校验），不宣布未验证的完成。
5. **`superpowers:requesting-code-review`** / **`superpowers:receiving-code-review`** —— Cell 6 完成后发起代码评审，接受并合入评审意见。
6. **`superpowers:finishing-a-development-branch`** —— 全部完成后收尾分支（如需）。
7. **`superpowers:brainstorming`** —— **禁止**：设计已批准，不得重新 brainstorm 或擅自变更已拍板决策。遇到设计未覆盖的疑点，记录并报告用户拍板，不自行决策。

## 四、现状事实（先读这些，不要凭记忆）

**转换知识散落 4+ 处**：
- `protocol/codec/registry.rs` —— 名义核心：`Downstream/Upstream` 枚举 + 静态 `Direction` 表，已注册 5 方向（V1-V5），缺 `responses→chat`、无原生 identity。
- `core/attempt.rs::codec_direction()`（L312-344）—— 手写 `(EndpointKind, UpstreamProtocol) → (Downstream, Upstream, 字符串label)`；`build_prepared_attempt`（L182-310）有 Native/Conversion 双分支 + legacy else 分支（`responses_via_chat_v1` 直接调 `protocol::responses_to_openai`）。
- `endpoint_executor/driver.rs::sse_mode_for()`（L50-63）—— 按 `codec_version` **字符串**匹配 → `SseMode`。
- `endpoint_executor/mod.rs`（L760-860）—— 非流式 decode 按**字符串**分派 + **内联重复实现**组合解码。
- `server/handlers.rs`（L2440 起）—— legacy `responses_via_chat` 直接调 `protocol::responses_to_openai` / `openai_to_responses`，绕过 registry。
- `protocol/mod.rs`（2029 行）—— 无版本 legacy 辅助函数（`responses_to_openai`、`openai_to_responses`、`responses_tool_choice_to_chat`）。

**多跳经过 chat**：
- V5 `encode_responses_to_messages`（`responses_codec.rs:662`）= `responses_to_openai` → `encode_chat_to_messages`（两跳）。
- V4/V5 流式是嵌套组合：`ResponsesMessagesStreamDecoder`（`responses_codec.rs:1322`）= `ResponsesStreamDecoder` → `ChatStreamDecoder`；`MessagesResponsesStreamDecoder`（L1556）= `MessagesSseState` → `ChatToResponsesStreamDecoder`。组合解码器有 `event.contains("codex.rate_limits")` 特判。
- 注释明说 *"intentionally no second direct Responses ↔ Messages protocol machine"*。

**类型系统**：`Downstream/Upstream`(codec) · `EndpointKind/UpstreamProtocol`(route_plan) · `NativeEndpoint`(presets) · `DownstreamProtocol`(gate)。**本次只统一 codec 核心内，其余不动**。

**auth 账号**：`classify_auth_account`（`route_plan.rs:824`）→ Conversion tier → registry 编码；执行路径独立（`endpoint_executor/mod.rs` 的 `dispatch_auth_account_executor` L72 / `dispatch_auth_account_stream_executor` L121）；`auth_provider/codex_backend.rs` 有字段 allowlist（ADR-37）与强制流式（ADR-36）。

## 五、目标架构

```
消费层(attempt/driver/executor/handlers/auth执行路径)  →  只调核心 API，不碰字符串
边界适配层(EndpointKind→Protocol, UpstreamProtocol→Protocol)  →  取代 codec_direction
转换核心 protocol/codec:  Protocol{Chat,Messages,Responses} + Registry.prepare + PreparedConversion
```

**9 项矩阵**：

| 下游 ╲ 上游 | Chat | Messages | Responses |
|---|---|---|---|
| **Chat** | identity 直通 | chat→messages 直连 | chat→responses 直连 |
| **Messages** | messages→chat 直连 | identity 直通 | **messages→responses 新增直连** |
| **Responses** | **responses→chat 新注册 (V6)** | **responses→messages 改直连** | identity 直通 |

**关键原则**：
- `PreparedConversion` 携带的 `non_stream`/`streaming` boxed 解码器**就是执行层要消费的东西**——driver 直接用 `prepared.streaming`，executor 直接用 `prepared.non_stream.decode(body)`，**消灭全部字符串分派与内联重复**。
- `codec_version` 用类型化值（`CodecVersion`），不再字符串匹配。
- 转换 **fail-closed**：无 codec 的方向返回错误，绝不透传原 payload。

## 六、Cell 计划（严格按序，逐 Cell 验收）

> 每个 Cell：先 `superpowers:test-driven-development`，实现后按 §七 自证。

### Cell C1 —— 核心类型与矩阵
- 在 codec 核心内定义单一 `Protocol { Chat, Messages, Responses }`，替代 `Downstream`/`Upstream`（保留兼容或直接迁移，按设计文档）。
- 重构 `CodecRegistry`：完整 **9 项矩阵**，含 3 个 identity（返回 `codec_version="native"` 的 `PreparedConversion`：model 替换 + 原样转发）。
- 新增边界适配层：`EndpointKind → Option<Protocol>`（只认 3 个可转换端点）、`UpstreamProtocol → Option<Protocol>`（Ollama 排除）。
- 迁移 `attempt.rs::codec_direction()` 逻辑进边界适配层。
- **验收**：`registry.prepare` 覆盖 9 项；identity 返回 native PreparedConversion；attempt.rs 不再有手写映射。

### Cell C2 —— responses→messages 直连协议机
- 请求编码器：Responses `input[]/tools/instructions/reasoning.effort/max_output_tokens` 直接映射 Messages body（设计 §6.1）；fail-open 字段写 ConversionReport。
- 非流解码器：Responses response object → Messages response。
- 流式状态机：Responses 事件链 → Messages 事件链，独立实现（**不**复用 Chat 中间件）；`codex.rate_limits` 原生透传。
- **验收**：直连事件链单测通过；对照"经 chat"旧组合 fixture 证明不丢语义（usage、tool id/顺序、reasoning、rate_limits）。

### Cell C3 —— messages→responses 直连协议机
- 对称方向（设计 §6.2）：Messages body → Responses body；Messages SSE → Responses SSE。
- **验收**：同上（对称）。

### Cell C4 —— responses→chat 注册 (V6) + legacy 收敛
- 把现有 `ResponsesToChat` SseMode + `protocol::responses_to_openai` 封装为直接 codec 注册为 V6。
- 收敛 handlers.rs legacy `responses_via_chat` 路径与 attempt.rs else 分支，改走 registry。
- `protocol/mod.rs` 被 registry 取代的辅助函数标记 deprecated（不删，避免破坏其它调用点；只清理本次收敛的直接调用）。
- **验收**：`grep -rn "responses_to_openai\|openai_to_responses" src-tauri/src/server src-tauri/src/core src-tauri/src/endpoint_executor` 只剩 registry 内部调用。

### Cell C5 —— 消费方改造 + auth 核心化
- attempt.rs：Native/Conversion 统一走 `registry.prepare(...)`。
- driver.rs：删除 `sse_mode_for()`，直接用 `prepared.streaming`。
- executor/mod.rs：删除非流式字符串分派与内联重复，直接用 `prepared.non_stream.decode(body)`。
- auth：`dispatch_auth_account_executor` / `dispatch_auth_account_stream_executor` 与普通 executor 共享解码路径；核心提供可选 `post_process` hook，auth 适配器只保留传输层（令牌头、强制流式、allowlist 过滤）。
- **验收**：`grep -rn "codec_version.as_deref\|Some(\".*_v1\")" src-tauri/src/endpoint_executor` 无命中；auth 与普通渠道共用解码路径。

### Cell C6 —— 测试收口
- 全矩阵往返测试（A→B→A 幂等）。
- 直连 vs 旧组合对照 fixture（C2/C3 期间固化的）。
- 现有 integration/e2e 全绿（含 Cell 5 Responses→Chat e2e、V5 Responses→Anthropic e2e）。
- **验收**：`cargo test --manifest-path src-tauri/Cargo.toml` 全绿。

## 七、每 Cell 验收门禁（自证清单）

1. 该 Cell 的测试先红后绿（TDD）。
2. `cargo test --manifest-path src-tauri/Cargo.toml` 通过（至少该 Cell 相关用例 + 既有用例不回归）。
3. 涉及字符串分派的 Cell：grep 校验无 `codec_version` 字符串匹配残留。
4. `cargo fmt --check`、`cargo clippy` 不新增告警（既有债务不算）。
5. 不触碰 §八 范围外文件（`git status` 核对改动文件清单）。

## 八、硬约束（违反即打回）

- **不涉及其他模块**：DB/迁移、前端 UI、`security/gate`、`channel_presets`、`adaptor/ClaudeAdaptor`、knowledge/wiki/stats/commands 一律不改。
- **不写面条代码**：高内聚低耦合；每个方向独立文件/模块，可独立测试；禁止在消费层散落协议细节。
- **fail-closed**：无 codec 的方向报错，不静默透传。
- **转换单跳**：responses↔messages 不得再依赖 chat 中间件；发现重新引入组合就重写。
- **不擅自改设计**：遇到设计未覆盖的疑点，停下列表报告用户，等拍板，不自行决策。
- **exactly-once 终止**：流式终止事件（`[DONE]`/`message_stop`/`response.completed`）保持 once 语义（T00 决策 6）。

## 九、完成判定

- Cell C1-C6 全部通过 §七 门禁。
- 设计文档 §4 矩阵、§5 消费方改造、§7 auth 全部落地。
- 发起并合入一次代码评审（`superpowers:requesting-code-review`）。
- 向用户提交：改动文件清单、每 Cell 验收证据、`cargo test` 汇总、遗留风险（如有）。
