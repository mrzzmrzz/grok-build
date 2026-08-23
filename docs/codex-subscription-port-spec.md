# grok-build Codex Subscription / Code Mode 移植 SPEC

版本：Draft 0.1  
目标仓库：/Users/mrzz/Documents/github/grok-build  
来源仓库：/Users/mrzz/Documents/open-grok  
目标分支：codex/sync-open-grok-codex

> 本文是上游检索、目标仓库静态审查和多 agent 交叉检查后的执行修改指南。当前仅保存 SPEC，不代表实现已经完成；在 Fable5 审查通过前不应继续扩大代码修改范围。

## 1. 当前基线

### 目标仓库

官方 grok-build 主线基线为：

~~~text
07b2f7144fd5c5c9d3dd1966937a87852d2dbdb8
Synced from monorepo
~~~

当前工作分支：

~~~text
codex/sync-open-grok-codex
~~~

目前已经存在 3 个 Codex 相关提交：

~~~text
048a63f8 feat(code-mode): add standalone runtime and protocol crates
9ab1810a feat(auth): add isolated Codex OAuth and live catalog
fe7eea64 feat(sampler): add native Codex Responses custom tool wire
~~~

这 3 个提交目前只能视为基础骨架：

| 提交 | 当前内容 | 当前状态 |
|---|---|---|
| 048a63f8 | Code Mode runtime/protocol 两个 crate、V8 运行时基础、协议和测试 | 已导入基础设施，但尚未接入真实 session、tool registry、模型能力 |
| 9ab1810a | Codex OAuth、PKCE、device auth、refresh、Codex catalog/cache | 认证与目录模块存在，但尚未完整接入 provider、session、sampler |
| fe7eea64 | Responses custom tool 类型、序列化、stream wire、tool call/output | 只补了部分传输层，仍缺 provider routing、credential、turn-state、history 边界 |

当前工作区已有未提交修改，应保留并继续审查，不能直接覆盖或清理。当前可见的未提交修改包括：

- Cargo.toml
- crates/codegen/xai-grok-shell/src/lib.rs
- 以及此前已经形成的 pager CLI、model picker、announcement 相关草稿，以工作区实际 git status 为准。

当前环境没有可用的 cargo、rustc 或 rustup，因此现阶段只能完成静态审查；cargo check、cargo test、V8 编译验证和真实 TUI 验证必须等 Rust 工具链可用后进行。

## 2. 不可改变的硬约束

这是本次移植的最高优先级约束。

### 2.1 必须保持 grok-build 的命名和目录

所有目标代码必须继续使用官方 grok-build 的命名：

~~~text
grok
$GROK_HOME
~/.grok
.grok
grok login --codex
grok logout --codex
~~~

Codex 文件使用同一个官方配置目录，但使用独立文件名：

~~~text
$GROK_HOME/codex-auth.json
$GROK_HOME/codex_models_cache.json
~~~

不得引入以下名称或路径：

~~~text
open-grok
OPENGROK_HOME
~/.opengrok
.opengrok
~~~

上游源码和部分 agent 审查建议中出现了 OPENGROK_HOME、~/.opengrok 和 open-grok，这些是来源仓库自身的命名，不适用于本次目标。这里明确否决路径迁移建议，不能因为来源仓库使用这些名称就照搬。

### 2.2 不能破坏原有 xAI 行为

以下行为必须保持兼容：

- 原有 grok login 继续执行 xAI 登录；
- 原有 xAI API key、session token、auth cache 不变；
- 旧配置中没有 provider 字段时，继续按照 xAI 模型解释；
- 原有 xAI 模型、xAI Responses/Chat 兼容逻辑不改变；
- xAI 的 x-grok-* header 不能被 Codex 请求继承；
- Codex logout 不能修改 xAI 登录状态；
- xAI logout 不能删除 Codex OAuth 状态。

### 2.3 provider 必须显式声明

不得通过以下信息猜测 provider：

- model slug；
- endpoint URL；
- 是否使用 Responses backend；
- 模型名称中是否包含 gpt、codex 等字符串。

至少需要明确区分：

~~~text
ModelProvider::Xai
ModelProvider::Codex

ApiBackend::Chat
ApiBackend::Responses

ProviderProfile::Xai
ProviderProfile::Codex
~~~

ApiBackend::Responses 只表示传输后端，不能隐含“这是 Codex”。

### 2.4 广告删除边界

本次删除的是“被动推荐升级”性质的广告入口，不是所有含有 Upgrade 字样的业务逻辑。

必须删除：

- 首页被动升级横幅；
- 会话中被动显示的升级 CTA；
- dashboard/header 中的套餐推荐；
- 被动 banner 的点击、hover、OSC8 链接；
- 仅用于广告曝光的 telemetry/impression；
- 被动升级按钮的鼠标命中区域；
- Ctrl+O 等专门打开升级 CTA 的路径；
- 随机公告中可能出现的推广公告；
- Welcome 页面通过 fallback 兜底展示的推广公告。

必须保留：

- 真实额度耗尽提示；
- 429、403、rate limit、quota、subscription restriction；
- 用户主动触发后的 paywall 或 billing 错误；
- 登录失败和认证过期提示；
- usage 信息；
- 权限、YOLO、安全、隐私相关提示；
- critical operational announcements；
- critical announcement 的隐藏和恢复控制。

不能通过全局删除 Upgrade、SuperGrok 或 subscription 字符串来实现，否则很容易误删真正的功能性错误提示。

## 3. 上游提交的选择性移植原则

来源仓库和目标仓库没有可直接复用的 Git merge base，因此不应尝试整体 merge 或整批 cherry-pick。

应当采用：

~~~text
上游提交行为分析
    -> 映射到 grok-build 当前抽象
    -> 只移植 Codex 所需逻辑
    -> 保持 grok-build 命名、配置和目录
    -> 增加针对性测试
~~~

### 3.1 需要参考或选择性重做的来源提交

#### Codex OAuth / Catalog

~~~text
e4264777  Codex OAuth account/store
9a0cb5c5  live Codex catalog
0cc46ec4  Max / Ultra / multi-agent capability
~~~

要求：

- 认证文件改为 $GROK_HOME/codex-auth.json；
- catalog cache 改为 $GROK_HOME/codex_models_cache.json；
- 不能使用来源仓库的 .opengrok 路径；
- 不能把 Codex catalog 并入 xAI model cache；
- 必须保留 account fingerprint、ETag、TTL 和 401 refresh 语义。

#### Provider / credential boundary

~~~text
3c8933a9  shared provider profiles
cf079433  sampling adapters
8959e918  auth/shell routing
71247805  provider-scoped credentials
27e48d8f  provider-scoped refresh/model defaults
2a07373c  Codex parity across harness
~~~

不建议整批复制 provider 重构。目标只需要实现 Codex 所需的最小 provider seam。

#### Responses/session 优化

~~~text
1ca8645b  instructions/context totals
6ac67234  response metadata / prompt-cache affinity
84cd3723  item-scoped streamed summaries
457b0b02  durable output items
ca402042  catalog-driven reasoning summary
97925ae9  remote compaction v2
92bece5d  session affinity headers
cd452ccd  invalid image preparation
~~~

这些是完成 Codex subscription parity 时必须重点审查的行为。

#### Code Mode

~~~text
19a328ca  embedded Code Mode runtime
883c98dd  model-first Code Mode policy
4be4a498  persistent Code Mode runtime
20ed866f  nested Code Mode results
f455d9ce  Code Mode Only integration
9ebec3a6  Responses custom tool calls
1ecf52f3  Codex dispatch semantics
1ff56345  live-stream exec/wait
7d5bae7d  sandbox/approval policy
7fee3349  permissions across subagents/streaming
~~~

不能只导入 runtime crate 就把 Codex 模型标记为 code_mode_only。

### 3.2 明确排除的内容

不应直接移植：

- OpenGrok branding；
- open-grok 命令；
- OPENGROK_HOME、~/.opengrok、.opengrok；
- OpenGrok 专属文档、release metadata、updater；
- Kimi、Fireworks、DeepSeek、Meta、Gemini、OpenRouter 等其他 provider；
- 与 Codex 无关的 whole-file provider 重构；
- 只有 OpenGrok 版本才需要的 UI 或发布逻辑；
- 不属于本次范围的 OpenAI Images 或额外独立服务。

## 4. 目标架构闭环

最终必须形成以下完整链路：

~~~text
TOML / CLI / default model / live catalog
        |
        v
ModelInfo
  - provider
  - backend
  - profile
  - capabilities
        |
        v
provider-aware credential resolution
  - xAI auth
  - Codex OAuth
  - explicit API key
        |
        v
Session state
  - provider identity
  - account identity
  - prompt cache affinity
  - turn-state
  - reasoning policy
        |
        v
SamplerConfig / ProviderAdapter
        |
        v
Codex request
  - Bearer
  - account headers
  - Codex originator
  - turn-state
  - prompt_cache_key
        |
        v
Responses stream
  - reasoning
  - custom tools
  - durable output
  - history/replay
        |
        v
session persistence / compaction / subagent / TUI
~~~

只完成认证文件、模型列表或 custom-tool serializer，不能视为 Codex subscription 已接入。

## 5. Workstream A：模型和配置 schema

### 5.1 需要统一补齐的字段

在现有模型配置结构中补齐 Codex 所需信息，具体名称应尽量复用目标仓库已有 schema，不要在多个 crate 重复定义。

涉及重点位置：

- crates/codegen/xai-grok-shell/src/agent/config.rs
- crates/codegen/xai-grok-shell/src/agent/models.rs
- crates/codegen/xai-grok-sampler/src/config.rs
- crates/codegen/xai-grok-sampling-types/src/types.rs
- crates/codegen/xai-grok-models/default_models.json

至少需要支持：

~~~text
provider
backend
responses dialect / provider profile
context_window
reasoning_efforts
default_reasoning_effort
reasoning_summary_supported
default_reasoning_summary
hosted_search
tool_mode
multi_agent_version
auto_compact_token_limit
comp_hash
~~~

建议语义：

~~~text
provider = "codex"
backend = "responses"
profile = "codex"
~~~

旧配置没有 provider 时，默认解释为 xAI，以保持兼容。

### 5.2 模型解析顺序

模型能力的优先级必须明确：

~~~text
embedded fallback
    -> live Codex catalog
    -> persisted model override
    -> CLI override
~~~

也就是说：

- live catalog 可以覆盖内置能力；
- 用户显式配置最后生效；
- 不能通过模型名字推断 reasoning 或 Code Mode；
- 未知 provider/backend 必须 fail closed；
- Codex catalog 不能覆盖 xAI catalog；
- 同名模型必须通过 provider identity 区分。

### 5.3 Codex fallback model

没有网络或没有 live catalog 时，需要有离线 fallback，至少保证已经声明支持的 Codex 模型仍然可以被解析。

如果某个模型尚未完整实现 Code Mode、compaction 或 multi-agent，不得在 fallback metadata 中宣称这些能力。

尤其要避免：

~~~text
catalog 宣称 code_mode_only
但 session/tool registry 尚未接入 Code Mode
~~~

这种配置不一致必须在 schema 或 startup 阶段被拒绝。

## 6. Workstream B：Codex OAuth 和 headless login

### 6.1 CLI 语义

必须支持：

~~~text
grok login
grok login --codex
grok login --codex --device-auth

grok logout --codex
grok logout --all
~~~

语义要求：

- grok login 保持原有 xAI 行为；
- grok login --codex 只进入 Codex OAuth；
- grok login --codex 成功后不能继续执行 xAI login；
- --device-auth 适用于无 GUI、无浏览器的服务器；
- 交互式 OAuth 和 device auth 都必须能返回清晰错误；
- headless 模式不能尝试自动打开浏览器；
- grok logout --codex 只清理 Codex 状态；
- grok logout --all 才清理两种 provider；
- 裸 grok logout 保持原有官方语义，不能被隐式改成双 provider logout。

需要检查：

- CLI early-exit；
- command enum；
- process_identity；
- ensure_authenticated；
- exit code；
- TUI 和 headless 两条入口是否使用同一个 auth API。

如果官方 TUI 有 slash auth 命令，还需要提供等价入口，例如：

~~~text
/login codex
/logout codex
~~~

具体名称以现有 slash command 设计为准，但不能只做 CLI 而让 TUI 无法操作。

### 6.2 Auth store

Codex auth 必须独立存储：

~~~text
$GROK_HOME/codex-auth.json
~~~

必须满足：

- 与 xAI auth 文件完全隔离；
- owner-only 文件权限；
- 原子写入；
- lock；
- PKCE；
- callback；
- device auth；
- access token 过期检查；
- refresh；
- best-effort revoke；
- refresh 失败状态隔离；
- account identity 变更时清理旧账户关联状态；
- 不允许旧 bearer 和新 account ID 混搭；
- 失败时 fail closed，不继续使用陈旧 token。

### 6.3 Credential precedence

必须明确以下优先级：

~~~text
显式 model API key
    >
model/env provider-specific key
    >
Codex OAuth
~~~

特别要求：

- Codex 模型有显式 API key 时，不能被 OAuth 覆盖；
- Codex 没有 API key 时才读取 codex-auth.json；
- Codex 不能 fallback 到 xAI auth.json；
- Codex 不能 fallback 到 xAI session token；
- xAI 模型不能自动读取 Codex OAuth；
- sampler 不负责“猜测”当前应该用哪种 auth。

建议将一次请求需要的认证信息设计为同一个 snapshot：

~~~text
ResolvedBearerAuth {
    bearer
    account_id
    fedramp
    expires_at
    provider
}
~~~

Bearer、account ID 和 FedRAMP 标记必须来自同一个 snapshot，不能在不同时间点分别读取。

### 6.4 Catalog

Codex live catalog 使用独立缓存：

~~~text
$GROK_HOME/codex_models_cache.json
~~~

缓存行为：

- endpoint 使用 Codex catalog endpoint；
- 带 Codex bearer；
- 发送 ChatGPT-Account-ID；
- 必要时发送 FedRAMP header；
- 5 秒级网络超时；
- 支持 ETag；
- 缓存 TTL 约 5 分钟；
- 缓存绑定 account fingerprint；
- 账户切换时不能复用旧账户 catalog；
- 401 只允许 Codex 自己 forced refresh 一次；
- refresh 失败不能修改 xAI model cache；
- 旧 account 的 in-flight 结果不能覆盖新 account 的目录。

## 7. Workstream C：Provider 和 session 绑定

这是当前最重要的缺口。

### 7.1 Session 构造顺序

必须将 session 初始化改成：

~~~text
resolved model
    -> ModelProvider
    -> ProviderProfile
    -> credential source
    -> credential snapshot
    -> SamplerConfig
    -> session
~~~

而不是：

~~~text
session
    -> 固定 xAI auth
    -> 固定 xAI sampler
    -> 再根据模型名猜 Codex
~~~

Codex session 的启动条件：

- 没有 xAI 登录时，只要有 Codex OAuth，Codex 模型仍可启动；
- xAI -> Codex 切换时，必须重新绑定 endpoint、auth、headers、tool profile；
- Codex -> xAI 切换时，必须清除 Codex header、turn-state、profile；
- session resume 时不能根据当前 catalog drift 把 Codex session 当成 xAI session；
- model switch 不能只更新显示文字而不重建 sampler。

### 7.2 Provider adapter

建议在 sampler 或 shell/session 边界增加最小 provider adapter：

~~~text
XaiProviderAdapter
CodexProviderAdapter
~~~

adapter 负责：

- endpoint；
- request dialect；
- header policy；
- hosted tool 映射；
- reasoning 映射；
- stream event policy；
- compaction policy。

adapter 不负责：

- 读取 auth 文件；
- refresh token；
- 修改认证状态；
- 直接决定用户选择的 provider。

认证解析应在 session/provider seam 完成后，以 immutable credential snapshot 交给 sampler。

## 8. Workstream D：Codex 请求 header、turn-state 和 prompt cache

涉及重点：

- crates/codegen/xai-grok-sampler/src/client.rs
- crates/codegen/xai-grok-sampler/src/config.rs
- crates/codegen/xai-grok-sampler/src/actor/request_task.rs
- crates/codegen/xai-grok-sampler/src/stream/responses.rs
- crates/codegen/xai-grok-sampling-types/src/types.rs

### 8.1 Codex headers

Codex 请求至少按 profile 生成：

~~~text
Authorization: Bearer <Codex token>
ChatGPT-Account-ID: <account id>
X-OpenAI-Fedramp: <when applicable>
Codex originator header
x-codex-turn-state: <when bound>
x-codex-beta-features: remote_compaction_v2
~~~

Codex 请求不得带：

~~~text
x-grok-*
x_search
xAI-only tracing metadata
xAI auth header
xAI provider-specific retry metadata
~~~

不能通过 generic extra_headers 不加区分地继承 xAI headers。

### 8.2 Turn state

x-codex-turn-state 生命周期：

1. 第一个成功的 Codex Responses 或 compact 响应返回 turn-state；
2. 保存到当前 logical prompt；
3. 同一 prompt 的 retry、tool continuation、client rebuild、in-turn compaction 复用；
4. 401 refresh/retry 仍然复用同一 turn-state；
5. 新用户 prompt 清除；
6. 切换到 xAI 时清除；
7. 不允许并发 prompt 互相覆盖；
8. manual compaction 应有自己的 operation-scoped state；
9. 不能将 turn-state 永久写入下一个 session。

### 8.3 Prompt cache

必须区分：

~~~text
prompt_cache_key
x-codex-turn-state
~~~

二者不是同一个生命周期。

prompt_cache_key 要求：

- 从稳定 session identity 派生；
- 同一 session 内保持稳定；
- compaction 和普通请求使用明确且一致的 affinity 策略；
- 不能包含 bearer；
- 不能包含完整用户 prompt；
- 不同 provider 不得复用；
- 不同安全边界的 fork/subagent 要有明确是否复用的策略；
- full-input HTTP 模式不能发送不支持的 previous_response_id；
- 请求应继续发送 provider-visible 的完整 history。

## 9. Workstream E：Responses custom tool 和 stream 完整性

当前 fe7eea64 只完成了部分 wire，需要继续审查。

### 9.1 Custom tool ID

当前已发现一个明确风险：

~~~text
custom_tool_call:<call_id>:<item_id>
~~~

如果 call_id 本身包含冒号，使用简单 split_once 会破坏 ID。

必须改为无歧义编码，例如：

- length-prefix；
- escaping；
- 明确的结构化 JSON；
- 或兼容旧格式的新 envelope。

必须增加 round-trip 测试：

- 普通 ID；
- 包含冒号的 ID；
- Unicode ID；
- 空 ID；
- 长 ID；
- 旧格式兼容解析。

### 9.2 Mixed content order

当前 custom tool output 使用独立的 content 和 images 字段，可能导致原始混合顺序丢失：

~~~text
text A
image 1
text B
~~~

不能被序列化成：

~~~text
text A
text B
image 1
~~~

需要使用有序 content representation，或者在兼容旧结构的同时新增 ordered content。必须测试文本、图片、空内容和混合顺序。

### 9.3 Typed custom tool

需要针对目标仓库锁定的 async-openai 版本确认：

- 是否已经存在 typed rs::Tool::Custom；
- 是否能直接表达 Codex hosted/freeform tool；
- 如果能表达，优先使用 typed API；
- raw JSON 只用于目标库无法表达的 hosted tool；
- raw JSON 不能作为所有 provider 的默认通道。

### 9.4 Stream / durable output

不能只依赖 response.completed，因为它可能只有 metadata 和 usage，没有完整 output。

必须支持：

- response.output_item.done 作为 durable output carrier；
- assistant message 持久化；
- custom/function tool arguments 持久化；
- reasoning item ID；
- encrypted_content；
- summary delta 按 output item 和 summary index 归属；
- 避免 terminal output 与 durable item 重复；
- 对未知 future side-channel event 可按 provider 选择忽略；
- 已知 event malformed 时必须报错；
- stream 中断后的 retry 不能破坏历史；
- tool output 必须可以 replay。

## 10. Workstream F：Reasoning、search、image 和 compaction

### 10.1 Reasoning

Reasoning capability 必须来自 catalog metadata，而不是模型名猜测。

需要明确：

- 是否支持 reasoning.summary；
- 支持哪些 summary mode；
- 默认 summary mode；
- auto、concise、detailed 的映射；
- Max/Ultra 到 wire 的映射；
- service tier 或 priority routing。

规则：

- 不支持 summary 的模型不得发送 reasoning.summary；
- catalog 声明 none 时省略该字段；
- live catalog 能力覆盖 fallback；
- 用户显式配置最后生效；
- summary delta 不能串到其他 reasoning item。

### 10.2 Hosted search

Codex 和 xAI hosted search 必须分开：

- Codex 使用 Codex 支持的 web search/tool 形态；
- xAI 使用原有 x_search；
- Codex 请求不能误带 xAI x_search；
- provider 切换时必须重新计算工具集合；
- nested Code Mode search 不能泄漏到错误 provider。

### 10.3 Image preparation

如果保留 Codex image/tool capability，需要移植：

- invalid image URL preparation；
- data URL 检查；
- tool output image 的合法化；
- retry 前的 image normalization；
- Code Mode 中只允许明确支持的 data scheme。

### 10.4 Remote compaction

如果模型 metadata 声明支持自动或手动 compaction，则必须实现：

- remote compaction v2；
- feature header；
- comp_hash；
- absolute auto compact token limit；
- manual compaction 的独立 turn-state；
- legacy unary compaction 只能显式 opt-in；
- 不允许 silent fallback；
- compaction payload 不能跨 provider；
- Codex opaque history 不能被 xAI parser 重新解释。

如果暂时不实现 compaction，则不能在模型能力中宣称已经完整支持。

## 11. Workstream G：Code Mode 完整接入

当前 crates/codegen/xai-grok-code-mode 和 crates/codegen/xai-grok-code-mode-protocol 只是 runtime/protocol 基础设施。

### 11.1 Cargo 集成

必须：

- 保留 workspace member；
- 保留所需 v8 = "=149.2.0"；
- 保留 deno_core_icudata = "0.77.0"；
- 更新并提交 Cargo.lock；
- 确认 V8 feature 与目标编译平台一致；
- 确认 ICU/native V8 依赖；
- 不允许在 Cargo.toml 更新后留下过期 lockfile。

当前 Cargo.lock 尚未包含新增的 V8、ICU 和 Code Mode package entries，这是 P0 构建阻塞项。

### 11.2 Session/tool registry 接入

需要完成：

- model capability；
- tool_mode；
- session effective mode；
- Code Mode runtime 创建与销毁；
- persistent V8 session；
- model switch 时重新计算；
- session close；
- stale generation fail-closed；
- rewind；
- cancellation；
- timeout；
- nested progress；
- tool output projection；
- permission/approval gate；
- plan mode gate。

### 11.3 Codex native tools

Codex Code Mode 至少需要：

~~~text
exec
wait
~~~

普通工具应隐藏在 JavaScript 的 nested tools.* namespace 中，TUI 不应直接显示 raw JavaScript transport wrapper。

必须验证：

- exec 参数和输出；
- wait 参数和完成状态；
- nested tool call；
- tool history request/response；
- subagent；
- timeout/cancel；
- stale generation；
- approval policy；
- Code Mode 与普通 tool mode 切换。

在这些内容没有完成前，不允许只因为 runtime crate 已存在，就把 Codex model 标记为 code_mode_only。

## 12. Workstream H：Session、subagent、memory 和隐私边界

### 12.1 ever_used_codex

需要在 session 及相关持久化状态中记录 Codex 使用边界：

~~~text
ever_used_codex
~~~

语义：

- 一旦 session 使用过 Codex，标记单调保持；
- 即使后来切回 xAI，也不能重新开启只适用于 xAI 的 remote sync、relay、prompt trace、memory export 等路径；
- resume、fork、subagent 都要保留该边界；
- Codex opaque history、encrypted reasoning、compaction payload 不得跨 provider。

### 12.2 Auxiliary model

需要明确 title、recap、memory、classifier、summary 等辅助模型的 provider 规则：

- Codex session 默认不能偷偷把用户内容发送给 xAI helper model；
- 用户显式配置 cross-provider auxiliary model 时才允许跨 provider；
- auto-mode classifier 不得无意继承 xAI 默认模型；
- 任何辅助请求都必须带明确的 provider provenance。

### 12.3 Subagent

需要验证：

- Codex parent session 创建 Codex subagent；
- provider boundary 传递；
- subagent prompt cache 策略；
- nested Code Mode 权限；
- subagent output durable history；
- 父 session 的 ever_used_codex 影响子 session；
- 不允许因为某个 helper model 默认是 xAI，就把 Codex 内容泄露到 xAI。

## 13. Workstream I：TUI 广告入口完整清理

当前 crates/codegen/xai-grok-pager/src/views/announcements.rs 的草稿只关闭了部分 CTA 渲染，不能视为完成。

### 13.1 必须先修 selection/filter

高风险入口包括：

- app/event_loop.rs 的 resolve_announcements()；
- app/acp_handler/settings.rs 的 announcement merge/picker；
- app/app_view.rs 的 Welcome fallback。

当前潜在问题：

~~~text
随机选择任意公告
    -> 选中推广公告
    -> CTA 虽然不渲染
    -> Welcome 或 announcement fallback 仍然显示推广内容
~~~

要求：

- 公告选择阶段过滤掉 passive promotional announcements；
- 或者 Welcome 只接受 critical operational announcements；
- 不能只在最终 renderer 中返回 None；
- hero_cta.or_else(...).or(self.announcement) 这类 fallback 必须重新审查。

### 13.2 必须清理的 UI 状态和交互路径

需要审查并移除被动广告相关的：

- header CTA；
- Welcome upgrade CTA；
- dashboard upgrade CTA；
- CTA rect；
- hit testing；
- hover state；
- mouse click；
- Ctrl+O 专用升级逻辑；
- OSC8 link；
- CTA telemetry；
- impression tracking；
- AnnouncementsOpenCta action；
- router 中的 open CTA dispatch；
- BannerHits::cta；
- _cta_hovered；
- _caption_allowed；
- promo_cta_target；
- upgrade_cta_reserve；
- render_cta_button；
- render_promo_row；
- HeaderUpgradeCta；
- welcome_upgrade_cta_rect；
- dashboard pinned upgrade fields。

重点文件：

- crates/codegen/xai-grok-pager/src/app/agent_view/render.rs
- crates/codegen/xai-grok-pager/src/app/agent_view/input.rs
- crates/codegen/xai-grok-pager/src/app/agent_view/links.rs
- crates/codegen/xai-grok-pager/src/app/agent_view/mod.rs
- crates/codegen/xai-grok-pager/src/app/mouse.rs
- crates/codegen/xai-grok-pager/src/app/actions.rs
- crates/codegen/xai-grok-pager/src/app/dispatch/router.rs
- crates/codegen/xai-grok-pager/src/views/welcome/mod.rs
- crates/codegen/xai-grok-pager/src/views/welcome/hero_box.rs
- crates/codegen/xai-grok-pager/src/views/dashboard/render.rs
- crates/codegen/xai-grok-pager/src/views/dashboard/state.rs

### 13.3 测试处理

当前草稿中有一些旧 promo tests 被 cfg(any()) 禁用。

最终不应保留这种形式。应当：

- 删除已经不存在的旧 CTA 行为测试；
- 重写仍有价值的 announcement tests；
- 增加至少一个负向回归测试，证明 passive promo 不会被选中、渲染或命中；
- 保留 critical announcement 的显示、隐藏、恢复和 expiry 测试；
- 不要用禁用测试掩盖未完成状态。

## 14. Workstream J：TUI model picker 和 usage

当前 model picker 草稿只完成了部分 provider/context label 接线。

需要继续验证：

- Codex catalog 是否真正进入正式 model manager；
- TUI picker 是否显示 Codex 模型；
- headless model resolution 是否也能解析 Codex；
- provider collision 是否可读；
- model switch 是否重建 session/sampler；
- ModelId.0 直接访问是否符合当前 API，最好改用稳定 accessor；
- provider/context label 不应只在 UI 显示而不参与实际路由。

如果完整 subscription 支持要求同时显示 usage，需补齐：

- xAI usage；
- Codex usage；
- provider 独立失败；
- /usage action/effect/task/result/view 完整链路；
- Codex usage 失败不能导致 xAI usage 消失；
- xAI usage 失败不能导致 Codex usage 消失。

## 15. 测试与验收标准

### 15.1 静态检查

工具链可用后至少执行：

~~~bash
cargo fmt --check
cargo metadata --locked
cargo check --locked
cargo test --locked
git diff --check
~~~

针对性执行：

~~~bash
cargo check --locked -p xai-grok-shell
cargo check --locked -p xai-grok-sampler
cargo check --locked -p xai-grok-pager
cargo check --locked -p xai-grok-code-mode
cargo test --locked -p xai-grok-shell
cargo test --locked -p xai-grok-sampler
cargo test --locked -p xai-grok-code-mode
~~~

如果 V8 crate 的 feature 或平台要求不同，还要执行对应 --all-features、--all-targets 检查。

目标仓库变更范围内禁止出现：

~~~text
open-grok
OPENGROK_HOME
.opengrok
~~~

但不要把所有 openai 字符串当成错误，因为 Codex provider 合法涉及 OpenAI/ChatGPT endpoint 和 header。

### 15.2 Auth 测试

必须使用临时 GROK_HOME 和 mock endpoint，禁止提交真实 token：

1. grok login --codex --device-auth 不依赖浏览器；
2. 只创建 $GROK_HOME/codex-auth.json；
3. 不修改 xAI auth；
4. token refresh 原子写入；
5. refresh 失败后 fail closed；
6. account 切换清理旧 account 关联状态；
7. grok logout --codex 不改变 xAI auth；
8. grok logout 保持原有语义；
9. grok logout --all 才清理两种 provider。

### 15.3 Provider routing 测试

至少需要：

1. 没有 xAI 登录但有 Codex OAuth 时，Codex 模型可以启动；
2. xAI 模型不读取 Codex OAuth；
3. Codex 模型不读取 xAI OAuth；
4. provider 由配置明确决定；
5. 仅有 responses backend 时不能自动推断 Codex；
6. 显式 API key 优先于 OAuth；
7. 模型切换后 endpoint、auth、tool profile、reasoning policy 全部切换；
8. Codex -> xAI 后不再发送 Codex header 或 turn-state；
9. xAI -> Codex 后不继承 xAI header；
10. provider collision 不会选择错误模型。

### 15.4 Header / turn-state 测试

使用 mock HTTP server 捕获请求，验证：

- Authorization bearer；
- account ID；
- FedRAMP；
- originator；
- x-codex-turn-state；
- remote compaction feature header；
- 不存在任何 xAI-only header；
- 同一 logical prompt 的 retry/continuation/rebuild/compaction 使用同一 turn-state；
- 下一个用户 prompt 不复用 turn-state；
- 401 retry 不丢 turn-state；
- 不同 provider 不复用 prompt cache affinity；
- session 内 prompt cache key 稳定；
- 不发送不支持的 previous_response_id。

### 15.5 Responses stream 测试

必须覆盖：

- response completed 无 output；
- output item done 恢复 assistant output；
- custom tool call；
- custom tool output；
- function tool；
- reasoning item；
- encrypted reasoning；
- summary index；
- mixed text/image order；
- malformed event；
- future side-channel event；
- stream retry；
- durable output 去重；
- ID 中包含冒号；
- Unicode ID；
- 空 ID；
- 旧 envelope 兼容。

### 15.6 Code Mode 测试

必须覆盖：

- Code Mode runtime 创建；
- session 复用；
- session close；
- stale generation；
- exec；
- wait；
- timeout；
- cancel；
- nested tools；
- permission；
- approval；
- plan mode；
- subagent；
- nested progress；
- raw JavaScript 不直接暴露到普通 TUI；
- model switch 后 capability 正确更新；
- runtime 异常时 fail closed。

### 15.7 广告清理测试

必须证明：

- passive promotional announcement 不会被随机选择；
- Welcome 不会通过 fallback 显示推广内容；
- header/dashboard/welcome 无 passive upgrade CTA；
- passive CTA 没有 hit target；
- mouse/hover/OSC8/Ctrl+O 不会触发广告；
- passive CTA 不产生 telemetry；
- critical announcement 仍可显示；
- critical announcement 仍可隐藏和恢复；
- quota、403、429、paywall、billing、auth、usage 等功能性消息仍正常。

## 16. 推荐实施顺序

### 阶段 0：保存基线和清理工作区边界

- 保留现有 3 个提交；
- 保留未提交修改；
- 不执行 reset、checkout 或覆盖；
- 记录当前 diff；
- 在 Fable5 审查通过前不继续扩大实现。

### 阶段 1：Cargo 和基础 schema

- 完成 workspace member；
- 完成 dependency；
- 更新 Cargo.lock；
- 增加 ModelProvider；
- 增加 ApiBackend / ProviderProfile；
- 增加配置字段；
- 增加默认值和 serde；
- 先确保旧 xAI 配置全部通过。

### 阶段 2：Auth / catalog / CLI

- 注册 Codex auth module；
- 接通 grok login --codex；
- 接通 --device-auth；
- 接通 logout isolation；
- 接通 $GROK_HOME 路径；
- 接通 Codex catalog/cache；
- 添加 mock auth/catalog tests。

### 阶段 3：Credential resolution / session

- provider-aware resolve_credentials；
- explicit API key precedence；
- Codex credential snapshot；
- session provider binding；
- model switch；
- resume/fork/subagent provider boundary；
- Codex 无 xAI 登录启动。

### 阶段 4：Sampler/provider adapter

- xAI/Codex provider adapter；
- endpoint；
- header policy；
- 401 retry policy；
- turn-state；
- prompt cache；
- reasoning policy；
- hosted tool policy。

### 阶段 5：Responses wire 完整性

- 修复 custom tool ID envelope；
- 修复 mixed content order；
- typed custom tool compatibility；
- durable output；
- reasoning stream；
- response metadata；
- retry/replay。

### 阶段 6：Code Mode

- runtime 接入 session；
- tool registry；
- native exec/wait；
- nested tools；
- permission/approval；
- persistent V8；
- cancellation/timeout；
- model capability；
- subagent/progress。

### 阶段 7：Codex 优化和隐私边界

- remote compaction；
- image preparation；
- hosted search；
- auxiliary model routing；
- ever_used_codex；
- memory/export boundary；
- subagent boundary；
- usage。

### 阶段 8：TUI 广告清理

- selection 过滤；
- Welcome fallback；
- CTA state/hit/hover；
- dashboard/header；
- mouse/OSC8/Ctrl+O；
- action/router/telemetry；
- 重写测试；
- 保留 functional billing/quota/auth messages。

### 阶段 9：最终验证

- 全量静态扫描；
- cargo fmt --check；
- cargo check --locked；
- 相关 crate tests；
- mock HTTP integration tests；
- TUI smoke test；
- diff review；
- 确认没有 OpenGrok 命名、路径或配置污染；
- 确认没有误删功能性订阅和配额逻辑。

## 17. 建议的提交边界

不要把所有内容压成一个大提交。建议至少分成：

~~~text
1. feat(config): add provider-aware Codex model schema
2. feat(auth): add isolated Codex OAuth and headless login
3. feat(models): add Codex catalog and provider-aware resolution
4. feat(sampler): route Codex Responses with isolated credentials
5. fix(responses): preserve custom tool IDs and durable output
6. feat(code-mode): integrate runtime with Codex session/tool registry
7. feat(session): add Codex affinity, reasoning, compaction boundaries
8. fix(pager): remove passive promotional surfaces
9. test: add Codex provider and advertisement regression coverage
~~~

在 Fable5 审查通过前，不建议继续扩展提交，也不建议 push。

## 18. Fable5 需要重点审查的决策点

请 Fable5 重点确认：

1. Provider seam 是否采用最小 Xai/Codex 双 adapter，还是需要引入更完整的 provider registry。
2. resolve_credentials 的 snapshot API 是否会影响现有 xAI BearerResolver 实现。
3. async-openai 当前锁定版本是否支持 typed custom tool；如果不支持，raw JSON 的兼容边界如何设计。
4. Code Mode 是否必须在第一阶段完整接通；如果未完成，是否暂时不把模型标记为 code_mode_only。
5. Codex usage 是否属于本次“完整 subscription 支持”的必须范围。
6. V8 149.2.0、ICU 和目标平台的构建条件。
7. Command::Login/Logout 变更是否破坏已有公共 API。
8. ModelId.0 直接访问是否应改用稳定 accessor。
9. 广告清理的边界是否正确：删除 passive promotion，但保留 quota/paywall/billing/auth/usage。
10. 是否接受当前所有 Codex 状态都放在 $GROK_HOME 下，并严格拒绝 .opengrok/OPENGROK_HOME。
11. Cargo.lock 是否应该作为第一阶段的必需修复，而不是最后再补。
12. 是否需要在 Codex session 使用过后永久禁止 xAI-only memory/export/telemetry 路径。

## 19. 最终验收定义

只有下面这条链路全部打通，才可以称为“grok-build 已支持 Codex subscription”：

~~~text
grok login --codex
    -> $GROK_HOME/codex-auth.json
    -> provider = codex
    -> Codex model catalog
    -> provider-aware credential resolution
    -> Codex session
    -> Codex sampler/provider adapter
    -> Codex bearer/account headers
    -> prompt_cache_key + turn-state
    -> Responses stream
    -> reasoning/tool/history persistence
    -> model switch/resume/subagent/compaction boundary
~~~

同时必须满足：

~~~text
grok-build 命名保持不变
$GROK_HOME / ~/.grok / .grok 保持不变
xAI 行为不回归
Codex 不读取 xAI auth
xAI 不读取 Codex auth
Codex 请求不携带 xAI headers
被动广告入口消失
功能性订阅/配额/权限/账单提示保留
Cargo.lock 与 workspace 一致
所有关键行为有测试
~~~

当前结论是：代码移植还没有达到实施完成标准，适合进入 Fable5 SPEC 审查阶段。

