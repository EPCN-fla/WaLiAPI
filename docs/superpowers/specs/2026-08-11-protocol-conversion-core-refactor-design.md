# 协议转换核心模块重构：统一 9 项矩阵 + 全量直连 codec

> 日期: 2026-08-11
> 分支: v0.1.8-protocol-conversion-refactor
> 状态: 已批准设计

## 1. 背景与问题

当前协议转换能力**不是唯一核心**，知识散落多处，且部分转换**多跳经过 chat**，auth 账号转换路径独立。

### 1.1 转换知识散落 4+ 处

| 位置 | 职责 | 问题 |
|---|---|---|
| `protocol/codec/registry.rs` | 名义核心：`Downstream/Upstream` 枚举 + 静态 `Direction` 表 | 只注册 **5 个方向**，缺 `responses→chat`，原生同协议无概念 |
| `core/attempt.rs::codec_direction()` | 手写 `(EndpointKind, UpstreamProtocol) → (Downstream, Upstream, 字符串label)` | 两套类型系统靠手写映射粘连 |
| `endpoint_executor/driver.rs::sse_mode_for()` | 按 `codec_version` **字符串**匹配 → SseMode | stringly-typed，编译期无保障 |
| `endpoint_executor/mod.rs` 非流式分派 | 按**字符串**分派 + **内联重复实现**组合解码 | V4 在 executor 里又写了一遍 responses→chat→messages |
| `server/handlers.rs` legacy | 直接调 `protocol::responses_to_openai` / `openai_to_responses` | **绕过 registry** |
| `protocol/mod.rs` | 无版本 legacy 辅助函数与 codec 层并存 | 两套转换并存 |

### 1.2 多跳经过 chat

- **V5 `responses_to_messages`**：请求 `encode_responses_to_messages` = `responses_to_openai`(→chat) 再 `encode_chat_to_messages`；流式 `ResponsesMessagesStreamDecoder` = Responses→Chat→Messages 嵌套组合。
- **V4 `messages_to_responses`**：流式 `MessagesResponsesStreamDecoder` = Messages→Chat→Responses 嵌套组合。
- 代码注释明说 *"intentionally no second direct Responses ↔ Messages protocol machine"* —— 中间协议多跳引入双重损耗/双重归一化。

### 1.3 四套类型系统表达同一概念

`Downstream/Upstream`(codec) · `EndpointKind/UpstreamProtocol`(route_plan) · `NativeEndpoint`(presets) · `DownstreamProtocol`(gate)。

### 1.4 auth 账号

请求编码已走 registry（`classify_auth_account` → Conversion tier → `CodecRegistry::prepare`），但**执行路径独立**（`dispatch_auth_account_executor` / `dispatch_auth_account_stream_executor`），解码/流式仍是字符串分派，ADR-37 字段 allowlist 在适配器内自己做。

## 2. 设计决策（已拍板）

| 决策 | 结论 |
|---|---|
| 单跳策略 | **全量直连 codec**：6 个转换方向各自实现真正的直接 codec（请求编码 + 非流/流式解码），responses↔messages 新增独立协议机，彻底不经过 chat |
| 类型系统 | **核心内统一 + 薄适配**：codec 核心统一为单一 `Protocol { Chat, Messages, Responses }`；route_plan/gate/presets 类型不动，边界映射函数集中放核心模块 |
| auth 核心化 | **转换进核心 + 适配器保留传输**：账号转换与普通渠道同一路径走 registry；适配器只保留传输层（令牌头、强制流式、字段 allowlist 后处理） |
| 原生路径 | 3 个原生同协议（identity）也进 registry，统一 9 项矩阵入口，消灭 attempt.rs Native/Conversion 双分支 |
| 范围 | 只改转换相关；DB/UI/gate/presets/ClaudeAdaptor/知识库等不动 |

## 3. 目标架构（分层）

```
┌─ 消费层 ─────────────────────────────────────────────┐
│  core/attempt.rs · endpoint_executor/driver.rs       │
│  endpoint_executor/mod.rs · server/handlers.rs       │
│  auth 执行路径（dispatch_auth_account_*）              │
└───────────────┬──────────────────────────────────────┘
                │ 只调核心 API，不碰协议细节、不碰字符串
┌───────────────▼──────────────────────────────────────┐
│  边界适配层（核心模块内）                              │
│  EndpointKind → Protocol      UpstreamProtocol→Protocol│
│  取代 attempt.rs::codec_direction 手写映射            │
└───────────────┬──────────────────────────────────────┘
┌───────────────▼──────────────────────────────────────┐
│  转换核心 protocol/codec                              │
│  Protocol { Chat, Messages, Responses }              │
│  Registry.prepare(downstream, upstream, model, req)  │
│  → PreparedConversion { encoded_request, context,    │
│      report, non_stream, streaming }                 │
│  完整 9 项矩阵（3 identity + 6 直连）                  │
└──────────────────────────────────────────────────────┘
```

**核心洞察**：`PreparedConversion` 现在就已携带 boxed `non_stream`/`streaming` 解码器，但 executor 没在用（driver 按字符串重推 SseMode、executor 内联重复实现组合解码）。重构最大收益点：**让执行层真正消费 `PreparedConversion` 的解码器盒**，一处改动同时消灭字符串分派 + 内联重复。

## 4. 核心矩阵（9 项）

| 下游 ╲ 上游 | Chat | Messages | Responses |
|---|---|---|---|
| **Chat** | identity 直通 | chat→messages 直连 (V1) | chat→responses 直连 (V3) |
| **Messages** | messages→chat 直连 (V2) | identity 直通 | **messages→responses 新增直连** |
| **Responses** | **responses→chat 新注册 (V6)** | **responses→messages 改直连** | identity 直通 |

- **identity**（3 个原生）：model 替换 + 原样转发，走 `registry.prepare()` 统一入口，返回 `codec_version="native"` 的 `PreparedConversion`。消灭 attempt.rs Native/Conversion 双分支。
- **responses→chat 新注册 (V6)**：把现有 `ResponsesToChat` SseMode + `protocol::responses_to_openai` 封装为直接 codec 注册，收敛 handlers.rs legacy `responses_via_chat` 路径与 attempt.rs else 分支。
- **responses↔messages 改直连**：删除两处"经 chat"组合，各写独立协议机（见 §6）。
- `codec_version` 从字符串改为 registry 返回的类型化值（`CodecVersion`），driver/executor 不再字符串匹配。

## 5. 消费方改造

| 现在 | 改造后 |
|---|---|
| attempt.rs：Native/Conversion 两个分支 + `codec_direction()` | 统一 `registry.prepare(...)`；映射函数移入边界适配层 |
| driver.rs：`sse_mode_for()` 字符串匹配 | 删除，直接用 `prepared.streaming` |
| executor/mod.rs：非流式 decode 按字符串分派 + 内联重复组合 | 删除，直接用 `prepared.non_stream.decode(body)` |
| handlers.rs：legacy `responses_via_chat` 直接调 `protocol::responses_to_openai` | 收敛进 registry；`protocol/mod.rs` 无版本辅助函数标记 deprecated |

## 6. 直连协议机（新增工作量核心）

### 6.1 responses→messages 直连

- **请求编码**：Responses `input[]` / `tools` / `instructions` / `reasoning.effort` / `max_output_tokens` 直接映射到 Messages body。现状两跳丢字段（`parallel_tool_calls`/`store`/`include`/`prompt_cache_key`/`client_metadata`）→ 直连按 fail-open 规则保留可表达字段并写入 ConversionReport。
- **非流解码**：Responses response object → Messages response（`output[]` → `content[]`，usage 映射）。
- **流式解码**：Responses 事件链（`output_item.added → content_part.added → output_text.delta/done → function_call → response.completed`）→ Messages 事件链（`message_start → content_block_start → text_delta → tool_use → message_stop`），独立状态机。
- **特殊事件**：`codex.rate_limits` 透传（现在组合解码器的 `event.contains("codex.rate_limits")` 特判要由直连状态机原生处理）。

### 6.2 messages→responses 直连

对称方向：Messages body → Responses body；Messages SSE → Responses SSE（`message_start/content_block_start/content_block_delta/tool_use/message_stop` → `output_item.added/content_part.added/output_text.delta/function_call/response.completed`）。

## 7. auth 账号（基于核心模块）

```
账号请求 → registry.prepare(Protocol, Protocol)   ← 与普通渠道完全同路径
        → CodecProvider 适配器（仅传输层）
             · Bearer 令牌头 / actor 头 / 会话头
             · 强制 stream:true（ADR-36）
             · 字段 allowlist 后处理（ADR-37，核心提供可选 post-process hook）
        → backend-api
```

- `dispatch_auth_account_executor` 与普通 executor 共享同一解码消费路径（直接用 `prepared.non_stream`/`prepared.streaming`）。
- 核心模块提供可选 `post_process` hook（`Option<Box<dyn Fn(Value) -> Value>>`），auth 用它做 allowlist 过滤，codec 本身不感知账号。

## 8. 范围边界

**改动**：`protocol/codec/*`、`core/attempt.rs`、`endpoint_executor/{driver,mod,sse}.rs`、`server/handlers.rs`（仅 legacy 收敛）、`auth_provider/*`（仅执行路径核心化）。
**不动**：DB/迁移、前端 UI、`security/gate`、`channel_presets`、`adaptor/ClaudeAdaptor`（遗留转换职责不扩）、knowledge/wiki/stats/commands、`protocol/mod.rs` 除 legacy 辅助函数标记外。

## 9. 测试策略

- 每方向：请求编码器属性测试 + 非流/流式解码器测试。
- **直连 vs 经 chat 旧实现对照 fixture**：证明直连不丢语义（`codex.rate_limits` 透传、usage、tool id/顺序、reasoning）。
- 全矩阵往返测试（A→B→A 幂等）。
- 现有 integration/e2e 保持绿（`cargo test` + 现有 Cell e2e）。
- 消费方改造后：driver/executor 不再出现 codec_version 字符串匹配（grep 校验）。

## 10. 实施 Cell 划分（供 codex 执行）

| Cell | 内容 | 验收 |
|---|---|---|
| C1 | 核心类型与矩阵：`Protocol` 枚举、registry 重构（9 项含 identity + 边界适配层） | `registry.prepare` 覆盖 9 项；identity 返回 native PreparedConversion |
| C2 | responses→messages 直连协议机（encode + non-stream + streaming） | 直连事件链单测通过；对照 fixture 不丢语义 |
| C3 | messages→responses 直连协议机 | 同上（对称） |
| C4 | responses→chat 注册 (V6) + legacy 收敛（handlers/attempt/protocol/mod.rs） | handlers legacy 路径改走 registry；`protocol/mod.rs` 直接调用点清零 |
| C5 | 消费方改造：attempt 统一、driver `sse_mode_for` 删除、executor 直接用解码器盒、auth 执行路径核心化 | grep 无 codec_version 字符串匹配；auth 与普通渠道共用解码路径 |
| C6 | 测试收口：全矩阵往返、对照 fixture、现有测试保持绿 | `cargo test` 全绿 |

## 11. 风险与注意

- V5 直连重写是行为变更最大点：以现有 e2e（Cell 5 Responses→Chat、V5 Responses→Anthropic）为回归基线，直连前先固话旧组合行为的 fixture。
- `codex.rate_limits` 事件在直连状态机中必须原生透传，不得依赖组合解码器的字符串特判。
- auth 账号的 `dispatch_auth_account_*` 是两个独立入口，消费方改造需同步覆盖两处（非流 + 流式）。
