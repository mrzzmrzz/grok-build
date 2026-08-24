# Fable Review：codex-subscription-port-spec 评审与修订迁移计划

评审对象：`docs/codex-subscription-port-spec.md`（Draft 0.1）
评审基线：分支 `codex/sync-open-grok-codex` @ `4b12cf6f`，工作区含未提交的 `Cargo.toml`、`crates/codegen/xai-grok-shell/src/lib.rs` 修改
评审方式：SPEC 逐条论断与代码库逐行核对（三路并行静态审查 + 直接读取），无工具链环境下的纯静态结论
日期：2026-08-23

---

## 1. 总评

SPEC 的方向、硬约束和风险识别**基本正确**，尤其以下判断已逐行核实为真：

- custom tool ID 的 `split_once(':')` 截断风险真实存在（`conversation.rs:521-532`），且**无任何测试覆盖**；
- custom tool output 的 `content`/`images` 分离字段确实丢失混合顺序（`conversation.rs:319-330`）；
- durable output 只依赖 `ResponseCompleted`/`ResponseIncomplete`（`stream/responses.rs:301,528-536,759-778`），`OutputItemDone` 不产出持久项；
- Welcome fallback 推广泄漏真实存在（`app_view.rs:4655-4667` 的 `.or(self.announcement.as_ref())`），且公告随机选择无 severity 过滤（`event_loop.rs:1513-1524`、`acp_handler/settings.rs:488-494`）；
- Cargo.lock 缺 `v8`/`deno_core_icudata`/`xai-grok-code-mode*` 条目，是构建阻塞项；
- 命名约束已满足：全仓库 `opengrok|open-grok` 零残留。

但 SPEC 有三类需要修订的问题：

1. **基线过时、低估已完成度**：SPEC 说只有 3 个 Codex 提交、pager 改动还是"未提交草稿"，实际已有 5 个提交（`9caea035`、`4b12cf6f` 已落地）；`codex_auth.rs` 的实现完整度远超 SPEC 描述的"骨架"。
2. **若干论断与代码不符**（见 §2.3），例如"catalog 支持 ETag"实际只存不用。
3. **计划结构是水平分层，缺少竖切收敛点和性能维度**；部分工作会与仓库已有的 seam 重复造轮子（见 §3、§5）。

**结论：SPEC 可作为约束清单（§2 硬约束全部采纳），但实施计划应按本文 §6 的修订版执行。**

---

## 2. 事实核查

### 2.1 基线修正

- 分支上实际有 **5** 个 Codex 提交：`048a63f8`（code-mode crates）、`9ab1810a`（codex_auth/codex_models 文件）、`fe7eea64`（sampler custom tool wire）、`9caea035`（pager promo 门控）、`4b12cf6f`（CLI + model picker label）。SPEC §1 只列了前 3 个，并把后两者描述为未提交草稿。
- 当前未提交修改仅剩：`Cargo.toml`（workspace member + v8/icudata 依赖 + path 声明）、`shell/src/lib.rs`（注册 `codex_auth`/`codex_models` 模块，**没有这两行，`9ab1810a` 的两个文件根本不参与编译**）、未跟踪的 `docs/`。这两处修改是让已落地提交成立的前提，应尽快入库（见阶段 0）。
- 环境确认无 `cargo`/`rustc`/`rustup`，SPEC 关于"只能静态审查"的判断成立。

### 2.2 SPEC 低估的已有资产（直接改变实施策略）

1. **provider 类型层已完成**：`ModelProvider { Xai, Codex }`、`ResponsesDialect`、`CodeModeTransport`、`ProviderProfile`（含 `XAI`/`CODEX` 常量、`sends_x_grok_headers`、`prompt_cache_headers`、`supports_backend()`）已在 `sampling-types/src/types.rs:1064-1140` 落地——但**全仓库零调用点，是死类型**。SPEC §2.3 的"至少需要明确区分"应改写为"把已有类型接线"，并增加约束：任何阶段结束时不允许留下无调用点的 provider 脚手架。
2. **`codex_auth.rs`（1688 行）是完整硬化实现，不是骨架**：PKCE S256、loopback callback、device auth（含 PKCE 强制校验 `:873-875`）、refresh（含 permanent-failure 记忆 `:75-113`）、best-effort revoke、`<path>.lock` 文件锁 + `fs2` 排他锁、pid 后缀临时文件原子写 + `sync_all`、owner-only 权限（含 Windows ACL 路径）、proactive 后台 refresh、identity anchor（`set_oauth_identity_anchor:1219`）。存储路径正确：`grok_home()/codex-auth.json`。**SPEC §6.2 的清单几乎全部已满足。**
3. **`CodexBearerResolver` 已实现 sampler 的 `BearerResolver` trait**（`codex_auth.rs:1280,1306-1310`），而 xAI 侧的挂载点就是 `SamplerConfig.bearer_resolver`（`sampler_turn.rs:520`、`agent/subagent/mod.rs:682` 附加，`config.rs:5254` 默认 `None`）。**credential 接线的 seam 已经对上了，SPEC §6.3/§7 的实施成本远低于其行文预期。**
4. **`prompt_cache_key` 已存在且有测试**：`conversation.rs:703-704`（body 字段，非 header）、`ApiBackend::forwards_prompt_cache_key()`（仅 Responses 上 wire）、fallback 到 `x_grok_conv_id`（`conversation/responses.rs:164-168`）、测试 `conversation.rs:2495`。Workstream D 的这部分是"复用 + 补 Codex 派生规则"，不是从零实现。
5. **仓库已有用户级 provider 体系**：`[model_providers.<id>]` TOML（`agent/model_providers.rs:9-24`：`base_url`/`api_key`/`env_key`/`api_backend`/`extra_headers`/`auth_provider`/`AuthProviderConfig` 命令式认证助手），以及 `ResolvedCredentials`（`agent/config.rs:4812-4817`）→ `sampling_config_for_model`（`config.rs:5211-5261`）的现成解析链。xAI auth 模块自带 `device_code.rs`、`oidc/`、`single_flight.rs`、`storage.rs`（原子写、owner-only）。**Codex 应作为内建 provider 挂进这条链，而不是另起一条平行解析路径。**
6. **CLI 已按 SPEC §6.1 语义接好**（`4b12cf6f`）：`login --codex`（conflicts_with `--oauth`）、`--device-auth`（xAI/Codex 共享 flag，`login --codex --device-auth` 可解析且有测试 `cli.rs:1448`）、`logout --codex`（conflicts_with `--all`）、`logout --all`（先 Codex 再 xAI）、裸 `login`/`logout` 语义未变、`--codex` 成功后 `finalize_and_exit(0)` 不落入 xAI 流程（`main.rs:2187-2232`）。缺口只剩 TUI slash 入口。
7. **功能性 upsell 与广告门控结构独立**：credit-limit 402/403 modal、free-usage 429 paywall、restricted-command upsell 全部走 `dispatch/billing.rs` 的硬编码 `UPSELL_URL_UPGRADE`，不经过 `promo_cta` 门（`billing.rs:33,74-84,187-190,247-309`）。SPEC §2.4 担心的"误删功能性提示"在当前门控方案下结构上不会发生——这是保留单点门控作为过渡态的一个理由。

### 2.3 SPEC 论断中不成立或遗漏的部分

1. **"catalog 支持 ETag"（§6.4）目前不成立**：`codex_models.rs` 只捕获并持久化 ETag（`:368-372,435`），**从不发送 `If-None-Match`**，不存在 304 条件请求路径。要么补条件 GET，要么从验收标准中移除该声明。
2. **`codex_models.rs` 是彻底的死代码**：`pub(crate)` 且零调用者。TTL/fingerprint/401-retry-once/原子写都已实现（`:25,252-279,289-295,570-597`），但没有任何入口。SPEC §14"Codex catalog 是否真正进入 model manager"的答案是明确的**否**。
3. **SPEC 遗漏：`logout --codex` 不清理 `codex_models_cache.json`**（`logout_at:1020-1075` 只删 `codex-auth.json`）。这是账户状态残留，违反 SPEC 自己的"account identity 变更时清理旧账户关联状态"原则。§15.2 的 auth 测试清单应补一条。
4. **SPEC 遗漏：custom tool ID 的跨 backend 泄漏**。编码后的 `custom_tool_call:<call_id>:<item_id>` 就是持久化的 `ToolCall.id`，会原样进入 Chat Completions wire（`conversation/chat_completions.rs:126`），并在 Messages backend 被 `sanitize_tool_call_id` 破坏性改写（冒号→`_`，`messages.rs:85-95,200,243`——call/result 两侧同函数处理所以自洽，但 ID 已不可逆）。修 envelope 时必须一并定义"Codex 专用编码不得泄漏到其他 backend 的 history 重放"的边界，§15.5 测试要加跨 backend 用例。
5. **§9.3 typed custom tool 的答案可以提前给出**：async-openai 是 fork（`our-forks/async-openai` rev `95b52ebd`，解析为 0.33.1），本地无源码无法直接确认；但间接证据明确——fork 已有 `rs::CustomToolCall`/`rs::CustomToolCallOutput`/`ResponseCustomToolCallInput{Delta,Done}` 等 item/event 类型，而 tools 数组定义仍手工 `json!({"type":"custom",...})` 走 `extra_tool_entries` raw JSON 通道，且 doc comment 明说 x_search "has no `rs::Tool` variant"（`conversation/responses.rs:382-448`）。**裁决：`rs::Tool::Custom` 大概率不存在；将 raw JSON 通道确认为 custom tool 定义的正式机制，加序列化 snapshot 测试锁定 wire 形状，工具链可用后再核实 fork 源码决定是否迁 typed。不作为阻塞项。**
6. **`ApiBackend` 有三个变体**（`ChatCompletions`/`Responses`/`Messages`），SPEC §2.3 只写了两个。Provider×Backend 的合法组合矩阵必须把 Messages 算进去（`ProviderProfile::supports_backend` 已正确处理：Codex 仅允许 Responses）。
7. **`ModelId.0` 直接访问确认存在**：`slash/commands/model.rs:238` 及测试 `:513`。`ModelId` 是 `Arc<str>` newtype，顺手改 accessor 即可，非结构性问题。
8. **sampler header 现状确认**：`GrokRequestHeaders::apply()`（`client.rs:48-79`）在三个 backend 的全部 6 个调用点无条件注入 `x-grok-*`，无任何 provider 分支；`x-codex-turn-state`/originator/account header 在 sampler 中零命中。SPEC §8 的缺口描述准确，且 `ProviderProfile.sends_x_grok_headers` 正是为此准备的开关——接线点就在 `apply()` 的调用侧。

---

## 3. 对 SPEC §18 十二个决策点的裁决

1. **Provider seam**：采用最小 Xai/Codex 双态，**不引入 provider registry**。实现方式：把已有的 `ProviderProfile` 挂到 `SamplerConfig`（新增 `provider_profile` 字段，默认 `XAI` 保证旧配置零变化），header/endpoint/工具映射按 profile 分支；用户侧声明复用 `model_family`（`ModelInfo` 已有该字段，`config.rs:3874`）+ `default_models.json` 的 `"model_family": "codex"`。不要在 shell/agent 层再造一套 provider 枚举。
2. **resolve_credentials snapshot**：**不需要破坏现有 `BearerResolver`**。`CodexBearerResolver` 已实现该 trait；缺的是 account_id/FedRAMP 必须与 bearer 同源。方案：给 `BearerResolver` 增加 `fn resolve_auth(&self) -> ResolvedBearerAuth`（含 bearer、account_id、fedramp、expires_at、provider），提供从现有 bearer 方法合成的 default impl——xAI 实现零改动，Codex 覆写返回同一次锁内读取的 snapshot。
3. **async-openai typed custom tool**：见 §2.3 第 5 条——raw JSON 通道转正 + snapshot 测试，不阻塞。
4. **Code Mode 是否第一阶段接通**：**否**。fallback metadata 不得声明 `code_mode_only`；若 live catalog 声明而 session/tool registry 未接入，startup 校验必须拒绝或降级为普通 tool mode 并警告（fail closed，SPEC §5.3 已有此要求，采纳）。
5. **Codex usage**：纳入范围但放后期（阶段 8）。`fetch_usage` 已在 `codex_auth.rs:1076` 实现，成本主要在 TUI 链路；不属于竖切必需。
6. **V8 构建条件**：`v8 = "=149.2.0"` 是极重依赖。**新增决定：`xai-grok-code-mode` 挂 workspace cargo feature（如 `code-mode`）门控**，默认构建与 CI 不编 V8，发布构建打开。Cargo.lock 无论 feature 开关都必须完整（lock 记录全集），所以 P0 不受影响。
7. **Command::Login/Logout**：已落地且设计合理（clap conflicts 齐全、裸命令语义未变、`--device-auth` 共享 flag 是合理复用而非污染）。无公共 API 破坏。剩余：TUI slash `/login codex` 等价入口。
8. **ModelId.0**：改用稳定 accessor，顺手修，不单列阶段。
9. **广告边界**：SPEC 划界正确。已核实功能性 upsell 结构独立（§2.2 第 7 条），不会误伤。**剩余两处真漏洞必修**：公告随机选择无过滤（`event_loop.rs:1513`、`settings.rs:488`）+ Welcome fallback（`app_view.rs:4655`）。终态要求物理删除死代码（`render_promo_row` 已零调用）与 15 处 `#[cfg(any())]` 禁用测试（`announcements.rs:1021-1586`）。
10. **$GROK_HOME**：接受。已核实零 opengrok 残留；护栏改为 CI 自动化（见 §7）。
11. **Cargo.lock**：**是，P0，阶段 0 完成**，不留到最后。
12. **ever_used_codex**：采纳，单调标记，随 session 持久化并传递到 resume/fork/subagent；与 auxiliary model provenance 同放阶段 8。

---

## 4. 对已落地代码的必修缺陷清单（新增，SPEC 未成清单化）

按优先级，这些是"先修已落地的，再加新的"原则下的第一批工作：

| # | 缺陷 | 位置 | 修法 |
|---|------|------|------|
| 1 | workspace 注册未提交，`codex_auth`/`codex_models` 实际不参与编译 | `Cargo.toml`、`shell/src/lib.rs`（未提交） | 阶段 0 提交 + 更新 Cargo.lock |
| 2 | custom tool ID `split_once` 首冒号截断 | `conversation.rs:521-532` | 改无歧义 envelope（见下），双向 round-trip 测试：普通/含冒号/Unicode/空/长 ID/旧格式兼容 |
| 3 | custom tool ID 跨 backend 泄漏 | `chat_completions.rs:126`、`messages.rs:85-95,200,243` | 定义边界：Codex 编码 ID 进入非 Responses backend 时的行为显式化并测试 |
| 4 | `content`/`images` 丢失混合顺序 | `conversation.rs:319-330`，序列化 `conversation/responses.rs:289-330`、`messages.rs:209-241` | `ToolResultItem` 增加 ordered `parts: Vec<ContentPart>`（保留旧字段 serde 兼容），三个 backend 序列化改从 parts 生成 |
| 5 | durable output 只信 `final_response` | `stream/responses.rs:301,528-536,759-778` | `OutputItemDone` 作为 durable carrier，terminal response 去重（doom-loop 恢复通道 `record_output_item`/`record_terminal_output` 的既有去重语义可参考，`doom_loop_recovery.rs:350-383`） |
| 6 | `logout --codex` 不清 models cache | `codex_auth.rs:1020-1075` | logout 时调用 `CodexModelsClient::invalidate_cache`（`codex_models.rs:299` 已有现成方法） |
| 7 | ETag 只存不用 | `codex_models.rs:368-372` | 补 `If-None-Match` + 304 处理，或从验收标准移除 ETag 声明 |
| 8 | 公告随机选择无 severity 过滤 | `event_loop.rs:1513-1524`、`acp_handler/settings.rs:488-494` | 选择阶段过滤非 critical（或白名单 severity），而非渲染层兜底 |
| 9 | Welcome fallback 渲染推广正文 | `app_view.rs:4655-4667` → `hero_box.rs:456`、`welcome/mod.rs:1654` | fallback 链只接受 critical 公告 |
| 10 | 15 处 `#[cfg(any())]` 禁用测试 + 死 promo 机器 | `announcements.rs` 全域，`render_promo_row:524`（零调用）等 | 删死代码、删过时测试、保留并扩充 `passive_promo_cta_is_not_selected_or_drawn:815` 这类负向回归 |

Envelope 建议（#2）：保持前缀嗅探兼容，新格式 `custom_tool_call.v2:` + JSON 或 length-prefix 载荷；`decode` 先试 v2，失败回退旧 `split_once`（旧数据只读兼容），`encode` 只产 v2。`item_id` 在尾部所以旧格式可用 `rsplit_once` 立即降低（但不消除）风险——不作为最终方案，仅在 v2 落地前的过渡提交中可接受。

---

## 5. SPEC 之外的增量建议

### 5.1 架构：复用已有 seam（最重要的修正）

- **credential**：走 `BearerResolver` 挂载点（§3 第 2 条），不新建平行解析路径；Codex 的存储/锁/原子写已自足，但 refresh 并发防抖应确认与 `auth/single_flight.rs` 语义一致。
- **provider 声明**：`model_family` 字段 + `default_models.json` 承载，`ProviderProfile::for_provider` 在 model 解析处一次求值、随 `SamplerConfig` 下发；sampler 内部只读 profile，不做任何猜测——这同时满足 SPEC §2.3 与 §7.2 的"adapter 不读 auth 文件"分工。
- **消灭死脚手架**：每个阶段的完成定义包含"本阶段引入/接线的类型无零调用点残留"。当前三个死区：`ProviderProfile` 家族、`codex_models.rs` 全模块、`codex_auth.rs` 的大部分 pub API（`run_tui_login`/`fetch_usage`/`start_proactive_refresh`/`CodexBearerResolver` 均无调用者）。

### 5.2 性能（SPEC 缺失的维度）

1. **V8 feature 门控**（§3 第 6 条）：省下日常迭代与 CI 的几十分钟级 V8 编译，是最大单项工程效率收益。
2. **prompt cache 命中率 = 推理性能**：`prompt_cache_key` 稳定性、turn-state 复用、session affinity header 直接决定 Codex 端 KV cache 命中与延迟/成本。把"同一 session 内 key 稳定、retry/continuation 不换 key"升格为性能验收项（mock server 断言）。
3. **credential 内存 snapshot**：`Arc` 不可变快照 + 过期检查，请求热路径零文件 IO；仅过期/401 走磁盘 + single-flight refresh，防止并发 refresh 风暴。
4. **catalog stale-while-revalidate**：启动直接用缓存渲染，live 刷新在后台完成后热更新；冷启动路径零网络等待（5 秒超时只约束后台刷新）。`load_fresh_or_fetch` 现有逻辑接近此语义，接线时保持。
5. **V8 平台惰性初始化 + 会话状态上限**：上游设计是每个 exec cell 用 fresh isolate 并重注入序列化的 stored_values（persistent 的是会话状态而非 isolate），isolate 常驻与 snapshot 预热不适用；应做的是首次进入 Code Mode 才初始化 V8 平台（建议默认 jitless），并给 stored_values 加大小上限/淘汰策略防止长会话无界增长。
6. **流式路径零克隆**：durable output 改造时历史项用 `Arc`/`Bytes` 引用；retry 重放不整段 re-serialize conversation（`ToolResultItem.content` 已是 `Arc<str>`，保持这个纪律）。

### 5.3 工程护栏自动化

- 禁止字符串（`open-grok`/`OPENGROK_HOME`/`.opengrok`）写成测试或 CI grep，而非人工终检；
- "Codex 请求无 `x-grok-*`、xAI 请求无 Codex header"写成 mock HTTP server 断言测试；
- auth/catalog/sampler 三处共享一个 mock server 测试 harness（临时 `GROK_HOME` + wiremock 风格），避免三套各写一遍；
- 序列化 snapshot 测试锁定 raw JSON custom tool 的 wire 形状。

---

## 6. 修订后的实施计划

原 SPEC 阶段 1-9 是水平分层，风险是做完多层才发现接不通。修订原则：**P0 先行、先修已落地代码、竖切收敛、独立工作并行**。

**阶段 0 —— 构建解锁（P0，工具链可用后第一件事）**
提交 `Cargo.toml` + `shell/lib.rs` 模块注册；`xai-grok-code-mode`/`-protocol` 挂 `code-mode` feature；更新 Cargo.lock（全集）；`cargo check --locked` 全 workspace 通过（含 `--features code-mode` 一次验证 V8 平台条件）；旧 xAI 测试全绿。

**阶段 1 —— 已落地代码的正确性修复**
§4 清单 #2–#7（ID envelope、跨 backend 边界、ordered content、durable output、logout 清 cache、ETag）。全部有既定测试要求，无新架构。

**阶段 2 —— 广告清理收尾（独立，可与阶段 1 并行）**
§4 清单 #8–#10：选择过滤、Welcome fallback、删死代码与禁用测试、负向回归 + critical 公告显示/隐藏/恢复/过期测试。SPEC §13/§15.7 的清单全部采纳。

**阶段 3 —— 竖切：一个 Codex 模型端到端（本计划的收敛点）**
`default_models.json` 加一个 `model_family: "codex"` 条目（能力保守：无 code_mode、无 compaction 声明）→ ModelInfo/`sampling_config_for_model` 携带 `ProviderProfile` → `BearerResolver::resolve_auth` snapshot 扩展 → session 构造按 profile 绑定 endpoint/resolver → `GrokRequestHeaders::apply` 按 profile 分支（Codex：Bearer + ChatGPT-Account-ID + originator + FedRAMP，无 `x-grok-*`）→ mock server 端到端冒烟：`login --codex`（mock）→ 启动 Codex session（无 xAI 登录）→ 一次 Responses 往返。**此阶段完成 = SPEC §19 链路的骨架打通。**
同阶段覆盖 §15.3 provider routing 测试 1-10 条。

**阶段 4 —— catalog 与 model picker 接线**
`CodexModelsClient` 接入 model manager（live 覆盖 fallback、用户配置最后生效、同名模型按 provider 区分、fail closed）；TUI picker 列出 Codex 模型并真正驱动 model switch（重建 sampler/session，非仅 label）；headless 解析同路径；`ModelId.0` 改 accessor；TUI slash `/login codex`、`/logout codex`；stale-while-revalidate 启动路径。

**阶段 5 —— turn-state 与 prompt cache affinity**
SPEC §8.2/§8.3 全部采纳：turn-state 九条生命周期规则、`prompt_cache_key` 派生与稳定性、401 forced-refresh-once 复用 turn-state、`x-codex-beta-features` 按能力发送、不发送不支持的 `previous_response_id`。测试按 §15.4 清单。

**阶段 6 —— Responses stream 完整性收尾**
reasoning item/encrypted_content/summary index 归属、malformed 报错 vs future event 忽略的 provider 策略、stream retry/replay 不破坏历史、§15.5 全清单（阶段 1 已覆盖的 ID/order/durable 部分不重做）。

**阶段 7 —— Code Mode 接入（feature 门控下进行）**
SPEC §11 全部采纳：session/tool registry、native exec/wait、nested tools、permission/approval/plan gate、persistent V8（惰性创建 + 空闲回收）、cancellation/timeout/stale generation、capability 随 model switch 重算；完成后才允许 catalog/fallback 声明 code_mode 能力。测试按 §15.6。

**阶段 8 —— 订阅完备性与隐私边界**
remote compaction v2（未实现前 metadata 不得声明）、hosted search 分道（Codex 不带 `x_search`）、image preparation、`ever_used_codex` 单调边界、auxiliary model provenance、subagent 边界、xAI/Codex usage 双链路独立失败。SPEC §10/§12/§14 采纳。

**阶段 9 —— 最终验证**
SPEC §15.1 命令全跑 + §5.3 护栏 CI 化 + §19 验收链路逐条打勾。

### 提交边界（修订 SPEC §17）

```text
0. build: register codex modules, gate code-mode behind feature, refresh Cargo.lock
1. fix(responses): unambiguous custom tool ID envelope with round-trip tests
2. fix(responses): ordered tool-result content and durable output items
3. fix(auth): codex logout invalidates catalog cache; conditional catalog GET
4. fix(pager): filter passive promos at selection and welcome fallback
5. chore(pager): delete dead promo machinery and rewrite announcement tests
6. feat(models): provider-aware ModelInfo and Codex fallback entry
7. feat(session): provider-bound credentials via BearerResolver snapshot
8. feat(sampler): profile-driven Codex headers, turn-state, prompt cache
9. feat(models): live Codex catalog into model manager and picker
10. feat(code-mode): integrate runtime with session/tool registry (feature-gated)
11. feat(session): compaction, search, usage, ever_used_codex boundaries
12. test: cross-provider isolation and advertisement regression suites
```

---

## 7. 验收标准修订（相对 SPEC §15/§19 的增删）

新增：

- `cargo check --locked` 默认 feature 集不编译 V8；`--features code-mode` 在目标平台可编译；
- `logout --codex` 后 `codex_models_cache.json` 失效；
- custom tool ID 含冒号跨 Chat/Messages backend 的行为测试；
- prompt cache key 稳定性作为性能验收项（mock 断言）；
- 冷启动无网络等待（catalog 缓存优先）；
- 每阶段无零调用点的新增 pub API 残留；
- 禁止字符串与 header 隔离由 CI 测试保证而非人工 diff review。

修改：

- ETag 验收改为"发送 `If-None-Match` 并处理 304"（或删除该项）；
- "Cargo.lock 与 workspace 一致"从最终验收前移为阶段 0 出口条件。

SPEC §19 的链路定义与硬约束清单其余部分**原样采纳**。

---

## 7.5 实施终态记录（2026-08-24）

阶段 0-8 的主体与前两轮外部评审整改（docs/codex-review-update.md）已落地；第三轮评审的整改与阶段 9 收尾进行中。"落地"不含下列显式延后项——在它们关闭前，本移植不应描述为完整达到 SPEC §19 验收：

- **remote compaction v2 主体延后**：wire 分析、beta header/comp_hash/body 塑形管道、operation-scoped turn-state 已落地并测试，但端到端请求路径与 catalog 能力插线未完成（评审发现 14 的处置＝显式延后，不宣称已实现；设计文档与剩余清单见会话 scratchpad 的 remote-compaction-v2-design）。
- **Code Mode 的两处已记录限制**：客户端 reverse-request PreToolUse 的 **deny** 已在 nested 路径消费（决策/效果拆分后共享同一门）；仍不可表达的是客户端侧 **rewrite**——`ClientHookResponse` 无 `updatedInput` 字段，扩展协议后两个调用点即可直接消费。deferred nested tools 机制已接线但当前全量投影。
- **V8 feature 边界未实施**（评审 lower 项）：方案已成文（shell 的 code-mode 依赖 optional + feature 门控），涉及 default/CI feature 集变更，留待构建策略决策。
- **跨进程 login/logout**：进程内代次 + 磁盘 logout epoch 已闭环；同机双进程并发交错由文件锁 + epoch 双检兜底。
- Code Mode 能力默认全域关闭：无任何内置模型声明 tool_mode，仅 live catalog 显式声明才激活（fail closed）。

## 8. 遗留的未决问题（需工具链或上游信息）

1. fork `async-openai@95b52ebd` 是否有 `rs::Tool::Custom` —— 待 `cargo doc`/源码核实（不阻塞，见 §3 第 3 条）；
2. v8 149.2.0 在目标发布平台（darwin/linux × arch）的编译矩阵 —— 阶段 0 实测；
3. Codex catalog endpoint 的真实响应 schema 与 `codex_models.rs` 反序列化的匹配度 —— 需一次真实联调或上游 fixture；
4. remote compaction v2 的 wire 细节 —— 依赖上游提交 `97925ae9` 的行为分析，放阶段 8 前完成。
