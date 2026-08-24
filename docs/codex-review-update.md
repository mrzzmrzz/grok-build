# Codex subscription port review update

## Review conclusion

Review range:

- Committed history: 4b12cf6f..1c28bf9d (21 commits).
- Current uncommitted Stage 7/8 working tree inspected at
  2026-08-24 17:48 +0800: 76 tracked files changed and 4 untracked files.

The fixes in 0169a51d and 1c28bf9d correctly address most findings from the
previous review: main-session live catalog identity, proactive refresh,
the simple one-login/one-logout race, sampler-internal turn-state propagation,
durable stream recovery, account-scoped catalog publication, logout model
selection, terminal theme resync, announcement visibility, unknown-event
logging, and the documented custom-call ID namespace.

The current Stage 7/8 work is not ready to merge. Code Mode is not activated by
the live Codex catalog, its nested-call path bypasses existing policy hooks and
misreports logical failures to JavaScript, and the new Codex provenance gate
does not cover every xAI egress path.

Recommended disposition: **changes requested**.

## Blocking findings

### 1. Live catalog-only Codex identity is still lost by subagents

Severity: **High**

Evidence:

- The main session now resolves provider identity through ModelsManager, but
  crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs:721-729 still uses
  static-config-only credential/provider resolution for the inherited model.
- A live-only slug therefore reaches is_codex == false; lines 766-792 do not
  install the Codex provider profile and bearer resolver.
- The fallback path at lines 826-831 can overwrite a correctly inherited
  resolver using the same incomplete model lookup.
- There is no regression test that spawns a subagent from a parent using a
  Codex model present only in the live account catalog.

Impact:

A live-discovered Codex model can work in the parent while its child is
reconstructed as xAI or loses the Codex bearer resolver.

Recommendation:

- Resolve child provider/auth facts from ctx.models_manager, using the same
  account-scoped catalog entry as the parent turn.
- Preserve an already-correct provider profile and bearer resolver in the
  fallback path.
- Add an end-to-end live-only Codex parent-to-child test.

### 2. Live Codex tool_mode is discarded, so Code Mode never activates

Severity: **High**

Evidence:

- crates/codegen/xai-grok-shell/src/codex_models.rs:91 and 656-659 retain the
  server's tool_mode string.
- The only live-catalog-to-model-manager mapping,
  crates/codegen/xai-grok-shell/src/agent/models/codex.rs:36-56, never copies
  it into ModelInfo.tool_mode; its comment still says no Code Mode is declared.
- crates/codegen/xai-grok-shell/src/agent/config.rs:3866-3872 treats a missing
  tool_mode as Classic.
- session/acp_session_impl/session_mode.rs:447-451 reads only that mapped
  ModelInfo field.
- Bundled models intentionally declare no Code Mode. Current tests activate it
  only by manually constructing model entries or plans.

Impact:

Even when /models returns tool_mode: code_mode_only, the model runs with the
Classic manifest. The new V8 runtime, native custom exec transport, nested
tools, and Pager rendering are unreachable through the real live-catalog path.

Recommendation:

- Parse known live values into ToolMode at the single catalog mapping site;
  warn and fail closed for unknown values.
- Test the complete wire-catalog -> ModelsManager -> effective turn plan ->
  native custom exec path.

### 3. Code Mode nested calls bypass PreToolUse/PostToolUse policy hooks

Severity: **High**

Evidence:

- The normal tool path executes server/client PreToolUse hooks, honors deny,
  reparses rewritten input, and then executes post-use/failure hooks at
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs:
  1116-1187 and the surrounding execution path.
- The nested path at session/acp_session_impl/session_mode.rs:595-701 performs
  parse, plan-mode, and permission checks and calls dispatch_tool directly.
- The nested projection excludes only exec and wait, so apply_patch,
  exit_plan_mode, and other special tools remain callable from JavaScript.

Impact:

A repository policy that denies or rewrites a write through PreToolUse can be
bypassed by invoking the same tool inside exec. Post-use/failure automation
does not run, and special tools can miss their required lifecycle handling.

Recommendation:

- Route nested calls through one shared preparation/execution funnel with the
  normal path, including hooks, rewritten-input parsing, special lifecycle
  handling, telemetry, and completion hooks.
- Test a hook-denied write, rewritten input, failure hook, and exit_plan_mode
  from inside exec.

### 4. Logical nested-tool failures resolve the JavaScript promise

Severity: **High**

Evidence:

- session/acp_session_impl/session_mode.rs:703-709 computes
  run_result.output.is_error() and marks the ACP row failed, but unconditionally
  returns Ok(String(prompt_text)) to the runtime.
- Only a dispatch-level Err reaches the rejection branch at lines 710-714.
- xai-grok-tools/src/types/output.rs:668-700 classifies ordinary results such
  as missing files, failed edits, non-zero shell exits, and MCP errors as
  logical failures.

Impact:

For example, await tools.read_file(...) on a missing file resolves with an
error string rather than throwing. JavaScript can continue with later writes
and store-state commits while the UI simultaneously reports failure.

Recommendation:

- Return Err(prompt_text) for a logical failure, or define and consistently
  implement a structured success/error contract.
- Test the actual bridge with a logical error result, not only a transport Err.

### 5. Nested results are flattened to strings despite a structured contract

Severity: **High**

Evidence:

- The Code Mode description says nested tools can return objects or strings and
  demonstrates accessing result.content[0].
- session/acp_session_impl/session_mode.rs:703-709 returns only
  Value::String(run_result.prompt_text), even though ToolRunResult.output is a
  serializable typed value.

Impact:

MCP, image, audio, and structured tool results lose their shape. A program that
follows the advertised result.content[0] contract receives a string and fails
with undefined/TypeError; image()/audio() forwarding cannot work as described.

Recommendation:

- Preserve the tool's raw/structured result shape and use a string only for
  genuinely textual outputs.
- Add bridge tests for MCP structured content and image/audio forwarding.

### 6. Stale Code Mode cells are not fenced across model switch or rewind

Severity: **High**

Evidence:

- Bridge messages have no runtime generation in
  crates/codegen/xai-grok-shell/src/tools/code_mode.rs:29-41 or the consumer at
  session_mode.rs:550-589.
- shutdown_detached removes the runtime and schedules shutdown asynchronously
  at tools/code_mode.rs:190-206; queued nested calls can race that shutdown.
- Cancel uses detached termination. Rewind does not reset Code Mode at all, so
  yielded cells and session store state can survive a history rewind.
- There is no integration test for queued nested writes during cancel/model
  switch or a yielded cell across rewind.

Impact:

A write from the previous model/turn can execute after switch, cancel, or
rewind, and state created by a discarded future can remain visible later.

Recommendation:

- Fence bridge messages with the active runtime generation and reject stale
  work synchronously before dispatch.
- Define and test a synchronous invalidation boundary for switch, cancel,
  close, and rewind.

### 7. Retained-history pruning still leaves ordered tool output alive

Severity: **High**

Evidence:

- The new tool_result_edit helpers synchronize request-copy pruning, image
  eviction, image sanitation, CWD rewriting, and compaction truncation.
- crates/codegen/xai-chat-state/src/actor/mutations.rs:374-385 still hard-clears
  only ToolResultItem.content during retained-history pruning.
- Responses serialization treats non-empty ToolResultItem.parts as
  authoritative at xai-grok-sampling-types/src/conversation/responses.rs:
  289-320.

Impact:

An aged Code Mode result can show the placeholder in content while retaining
the complete old text/images in parts. That content remains persisted and is
replayed on the next Responses request.

Recommendation:

- Route retained-history hard clear through set_tool_result_text.
- Add an actor-level retained-prune test using tool_result_with_parts, then
  assert both persisted state and wire JSON omit the old content.

### 8. ever_used_codex is established after content can reach xAI sync

Severity: **High**

Evidence:

- Initial Codex sessions are marked during spawn, but an xAI-to-Codex switch at
  session/acp_session_impl/model_switch.rs:49-86 updates sampling state without
  marking provenance.
- The user message is emitted/persisted before sampling at
  session/acp_session_impl/turn.rs:615-630 and 737-742.
- Persistence continues queueing notifications to remote/relay while false at
  session/persistence.rs:1443-1454.
- The only runtime mark after spawn is session_setup.rs:524-538, conditional
  on receiving a non-empty Codex turn-state response header.

Impact:

After switching an xAI-synced session to Codex, the Codex prompt can be queued
to xAI remote/relay before the response. If the request fails or no turn-state
header arrives, the session may never be marked.

Recommendation:

- Mark chat state and persistence synchronously when the effective turn
  provider becomes Codex, before prompt persistence or sampling.
- Use provider identity, not an optional response header, as the provenance
  signal.
- Test request failure and missing-header cases as well as success.

### 9. Codex provenance does not gate prompt-trace uploads

Severity: **High**

Evidence:

- Summary.ever_used_codex claims prompt traces are disabled, but the flag is
  read only by chat-state snapshotting and persistence remote/relay setup.
- crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs:3773-3814
  decides whether to create a trace context without reading session provenance
  or effective provider.
- session/acp_session_impl/turn.rs:2304-2317 still attaches a
  ConversationRequestTrace whenever trace upload is enabled.

Impact:

Even a session that starts on Codex and is correctly marked can upload
Codex-derived prompts, history, images, and turn artifacts through the xAI
trace pipeline. The implementation contradicts its own privacy contract.

Recommendation:

- Gate trace-context creation and turn upload on monotonic provenance before
  capture begins.
- Test initial Codex, xAI-to-Codex, resume, fork, and switch-back cases.

## Other required findings

### 10. Codex subagent output does not taint the parent session

Severity: **Medium-High**

Evidence:

- A parent can select a different subagent model at
  agent/subagent/handle_request.rs:394-403 and spawn it at 971-999.
- The child completion envelope carries no provider/ever_used_codex provenance;
  child_run_output at agent/subagent/mod.rs:1872-1881 forwards only result,
  completion data, and a snapshot reference.
- The parent marks itself only from its own initial provider or response
  metadata.

Impact:

An xAI parent can merge a Codex child's output and remain unmarked, making that
derived content eligible for xAI remote/relay and prompt-trace egress.

Recommendation:

- Propagate monotonic provider provenance through child completion and mark the
  parent before merge/persistence.
- Test successful, failed, and cancelled child paths.

### 11. Pager parses the wrong serialized shape for Code Mode output

Severity: **Medium**

Evidence:

- ToolOutput is internally tagged with serde(tag = \"type\") at
  xai-grok-tools/src/types/output.rs:623-624.
- The shell sends serde_json::to_value(&result.output) at
  session/acp_session_impl/tool_calls.rs:2462-2474, producing an object with
  type: CodeMode and sibling fields.
- Pager expects an externally tagged CodeMode child object at
  xai-grok-pager/src/acp/tracker.rs:2155-2164.
- Tests exercise the block directly, not tracker mapping from the shell value.

Impact:

Parsing always falls back. The block loses cell_id, yielded/completed/
terminated state, and detailed errors.

Recommendation:

- Deserialize the internally tagged raw_output shape.
- Add a shell-serialized-value-to-Pager-block regression test.

### 12. Advertised Code Mode output limits are ignored

Severity: **Medium**

Evidence:

- exec advertises/parses max_output_tokens, but the service conversion to
  CreateCellRequest drops it and the runtime request has no matching field.
- wait exposes max_tokens, but WaitTool::run ignores it and WaitRequest carries
  no limit.

Impact:

Calls requesting small caps can return unbounded output relative to their
documented contract and consume much more context than requested.

Recommendation:

- Plumb both limits to the output collection/truncation point, or remove the
  unsupported fields until implemented.
- Test initial exec output and subsequent wait chunks at the boundary.

### 13. Concurrent nested writes bypass the normal same-path lock

Severity: **Medium**

Evidence:

- The normal batch path serializes non-read-only operations targeting the same
  path at session/acp_session_impl/tool_calls.rs:593-653.
- The Code Mode consumer spawns each nested call separately and dispatches it
  directly at session_mode.rs:560-572 and 695-701.
- Promise.all can therefore start multiple writes to the same file without the
  existing per-path lock.

Impact:

Two edits based on the same file version can race, lose one update, or fail
nondeterministically.

Recommendation:

- Reuse the normal path-lock mechanism for nested calls.
- Add a Promise.all same-file edit regression test.

### 14. Remote compaction and comp_hash handling are non-functional scaffolding

Severity: **Medium**

Evidence:

- The new body-shaping and beta-header helpers in xai-grok-sampler/src/client.rs
  are called only by unit tests; no compaction request invokes them.
- session/compaction.rs:2067-2077 hardcodes current_codex_comp_hash() to None.
- The same-slug hash-change branch can therefore never fire and no live model
  capability enables remote compaction.

Impact:

The patch adds public surface and state fields but no runtime behavior. Unit
tests prove isolated shaping helpers, not a real request or hash transition.

Recommendation:

- Complete the catalog capability/hash plumbing and an end-to-end request, or
  remove/defer the unused scaffold under YAGNI.
- Do not advertise remote compaction as implemented yet.

### 15. Login/logout latest-intent still fails for queued and cross-process operations

Severity: **Medium**

Evidence:

- extensions/codex.rs serializes operations through a FIFO mutex, but only a
  logout cancels the login that is already registered as pending. A login
  waiting behind the mutex has not registered its cancellation token yet.
- With login A active, login B queued, and logout C queued, C cancels A; B then
  acquires the mutex and starts a fresh callback wait before C can run. Pager
  generation fencing hides stale UI results but does not change credential
  state or queue latency.
- The new auth mutex and LOGOUT_GENERATION fence in codex_auth.rs are
  process-local statics.
- The file lock serializes mutations but carries no persisted generation
  between a TUI process and a separate CLI process.

Impact:

The user's final logout can be delayed behind a login that was requested
earlier but had not started, potentially for the full callback timeout. Across
processes, logout can complete and an older login can later acquire the file
lock and recreate credentials.

Recommendation:

- Allocate operation generations/cancellation at enqueue time so a logout
  invalidates active and queued older logins; add a real login-login-logout
  concurrency test.
- Persist a logout epoch/tombstone under the same cross-process lock and check
  it immediately before login persistence, or explicitly prevent concurrent
  cross-process flows.
- Add a two-process auth-file regression test.

### 16. Codex 401 recovery is not bounded to one replay

Severity: **Medium**

Evidence:

- The Codex branch force-refreshes on each eligible failure in
  session/acp_session_impl/sampler_turn.rs.
- It then uses the shared AuthRetrySchedule, which allows several credentialed
  retries rather than one replay for the logical request.
- Tests cover low-level refresh and no-anchor failure, but not a successful
  401 -> refreshed bearer -> one replay sequence or persistent-401 bound.

Impact:

A persistent 401 can trigger repeated OAuth refresh traffic and request
replays, contrary to the comments and prior remediation requirement.

Recommendation:

- Track whether Codex recovery has already run for the logical request.
- Add captured-wire tests for success and persistent-401 exhaustion.

### 17. Identity-less valid Codex credentials are treated as logged out

Severity: **Medium**

Evidence:

- account_fingerprint returns None when credentials have no account ID, user
  ID, or email.
- ModelsManager uses that optional fingerprint both for account scoping and as
  its logged-in predicate.

Impact:

A usable bearer/refresh-token file without those optional claims hides bundled
Codex models and suppresses live-catalog refresh.

Recommendation:

- Separate is_logged_in from optional account_fingerprint.
- Keep authenticated visibility while conservatively disabling reusable
  account-scoped cache publication when identity is unavailable.

## Lower-priority maintenance issues

- stored_entry_bytes claims to overcount serialized object size by one byte,
  but it actually undercounts a non-empty JSON object by one: the real object
  has two braces and one fewer comma. The global atomic merge fix is sound, but
  an accounted 8 MiB map can serialize to 8 MiB + 1 byte.
- xai-grok-shell and xai-grok-tools now pull the V8/ICU Code Mode dependency
  graph into ordinary checks/tests unconditionally, even for Classic-only
  builds. A code-mode feature boundary would avoid paying this compile cost
  when the capability is disabled.
- The workspace-wide vendored crossterm patch still needs a documented
  upstream/upgrade path.
- .github/workflows/build.yml still has no pull-request trigger or test job and
  still uses macos-14.

## Reviewed areas that now appear sound

The following previous findings were rechecked and removed from the active
list:

- main-session live-catalog provider reconstruction;
- proactive refresh startup and provider-isolated Codex 401 routing;
- the single active-login followed by logout cancellation/mutation race (the
  queued multi-operation and cross-process cases remain in finding 15);
- sampler-internal x-codex-turn-state propagation;
- durable recovery after retryable stream errors, idle timeout, and clean EOF;
- account-fingerprinted catalog publication and stale-fetch fencing;
- logout restoration of currentModelId to an available model;
- child-process terminal theme resync and unified announcement visibility;
- bounded unknown Responses-event logging and warning cleanup;
- provider-gated hosted-search serialization;
- session-global atomic stored-state merge enforcement;
- custom-tool ID namespace documentation and regression coverage;
- ordered result construction and the mutation paths now using
  tool_result_edit, except retained-history pruning in finding 7.

## Validation evidence

This was a read-only multi-agent code review. No production source file was
modified by the review; only this document was updated.

The following check completed successfully against the reviewed snapshot:

    git diff --check

Claude's concurrent xai-grok-shell and xai-grok-pager test processes were still
running when the snapshot was taken, so this document does not claim those
suites as completed evidence. Green unit tests would not resolve the catalog
wiring, hook bypass, provenance ordering, or serialization-shape findings.

## Recommended remediation order

Before enabling or merging Code Mode:

1. Map live Codex tool_mode into the effective catalog.
2. Route nested calls through the normal hook/policy/lifecycle and same-path
   locking funnel.
3. Preserve structured results and reject logical failures.
4. Fence runtime generations and define cancel/switch/rewind semantics.
5. Finish the retained-history ordered-parts mutation path.

Before treating provider privacy as complete:

6. Mark Codex provenance before prompt persistence or xAI sync.
7. Gate prompt traces on monotonic provenance.
8. Propagate Codex subagent provenance to parents.

Before release:

9. Fix live-only Codex provider reconstruction in subagents.
10. Correct Pager raw-output parsing and implement/remove output caps.
11. Wire remote compaction end to end or remove the unused scaffold.
12. Bound Codex 401 recovery and decide the cross-process auth contract.
