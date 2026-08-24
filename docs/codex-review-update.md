# Codex subscription port review update

## Review conclusion

Review snapshot:

- Branch: `codex/sync-open-grok-codex`.
- HEAD: `48386052a80cbd33ecbcb8624be06f9f74194243`.
- Committed range: `4b12cf6f..48386052` (28 commits).
- Uncommitted implementation inspected at 2026-08-24 20:28 +0800:
  25 tracked `xai-grok-shell` files and no untracked files.
- The implementation worktree was changing concurrently. Findings below are
  tied to the snapshot and paths stated here, not to later edits.

The latest commits are a substantial improvement. Live-only Codex identity,
live `tool_mode`, retained ordered output, logical nested failures, Pager
decoding, per-call output limits, same-path nested locking, model-switch
provenance ordering, child-to-parent provenance, bounded persistent-401 replay,
identity-less login visibility, the PR workflow, macOS 15, and the crossterm
vendor record are now implemented in reasonable locations with focused tests.
Those fixed findings have been removed rather than retained as historical
noise.

The current implementation is still not ready to merge as complete. The new
auxiliary-provider gate is snapshot-based and can leak Codex-derived web-search
queries to xAI after a model switch or child override. Code Mode still bypasses
ACP client policy, does not invalidate every rewind/stale-work path, and does
not preserve the structured result contract it advertises. Prompt-trace and
logout fencing also retain smaller but real race/fail-open boundaries.

Recommended disposition: **changes requested**.

## Blocking findings

### 1. Web search remains bound to the old or parent provider

Severity: **High**

Evidence:

- `agent/mvp_agent/agent_ops.rs:4665-4666` resolves the web-search sampler once
  from the provider active when the session is created.
- `session/acp_session_impl/spawn.rs:474-504` converts that snapshot into the
  session's persistent `WebSearchConfig`.
- `session/acp_session_impl/model_switch.rs:5-128` updates sampling identity,
  Codex provenance, and Code Mode, but never replaces or disables the existing
  web-search config.
- `agent/mvp_agent/subagent_spawn.rs:220-257` similarly precomputes child web
  search from the parent/global provider.
- The child's actual provider is resolved later at
  `agent/subagent/handle_request.rs:408-414`, after which line 1134 passes the
  parent-gated config through unchanged.
- `session/acp_session_impl/spawn.rs:481-489` enables the helper whenever the
  supplied config has a key; it does not re-check the effective child provider.
- The new test at `agent/mvp_agent/agent_ops.rs:5464-5492` tests only the pure
  resolver truth table. It does not exercise model switch, child override, or
  final ToolBridge wiring.

Impact:

Two direct failure scenarios remain:

1. Start an unpinned xAI session, switch it to Codex, then call `web_search`.
   The session retains the xAI endpoint/key captured at spawn, so a
   Codex-derived query is sent to xAI.
2. Spawn a Codex child from an xAI parent through a role/persona/model override.
   The child receives the parent's enabled xAI helper. The reverse direction,
   Codex parent to explicit xAI child, is incorrectly disabled.

Recommendation:

- Make web-search availability follow the effective provider on every model
  change, or enforce the provider gate again at dispatch time.
- Resolve child web search only after `effective_sampling_config` is known.
- Add end-to-end xAI-to-Codex switch and cross-provider child-override tests.

### 2. Auxiliary-model provenance still treats server configuration as consent

Severity: **High**

Evidence:

- The specification requires a *user-explicit* cross-provider choice at
  `docs/codex-subscription-port-spec.md:850-857`.
- `config/mod.rs:512-544` documents the same rule, but `AuxModelPin::Pinned`
  combines CLI, local TOML, and remote settings.
- `config/mod.rs:642-656` turns remote `web_search_model`,
  `session_summary_model`, and `image_description_model` values into an
  explicit pin; `AuxModelPin::is_explicit()` therefore treats server-delivered
  settings as user consent.
- The new test at `config/tests.rs:4014-4083` explicitly asserts that remote
  settings are an explicit source.
- The spec also calls out the auto-mode classifier. `agent/config.rs:4741-4763`
  allows its model to come from remote settings, while
  `session/acp_session_impl/sampler_turn.rs:751-851,941-968` resolves and calls
  that auxiliary model without checking the current session provider or any
  user-consent provenance.

Impact:

A remote setting controlled by the service can authorize an xAI helper for a
Codex session without a user choosing that cross-provider route. Auto-mode can
also send the user's command/context to a remotely configured xAI classifier.
This contradicts the port's explicit privacy contract even when the stale
snapshot bug in finding 1 is fixed.

Recommendation:

- Distinguish local user pins (CLI/env/TOML) from remote defaults.
- Treat remote settings as unpinned for cross-provider consent unless a
  separately authenticated user/organization policy is intentionally defined
  and documented.
- Apply the same provider-provenance rule to classifier, title, recap, memory,
  summary, and every other auxiliary request enumerated by the spec.

### 3. Code Mode nested calls bypass ACP client `PreToolUse` policy

Severity: **High**

Evidence:

- `session/acp_session_impl/session_mode.rs:610-626` explicitly states that the
  nested path does not consult reverse-request client hooks.
- The path runs registry hooks, permission checks, and then dispatches at
  `session_mode.rs:646-874`.
- The normal top-level path calls the ACP client gate at
  `session/acp_session_impl/tool_calls.rs:1181-1186` and honors its deny result.
- `docs/fable_review.md:204-210` records this as a deferred limitation even
  though the older review classified it as a blocking policy bypass.

Impact:

An ACP client can deny or rewrite a top-level `apply_patch`, shell, edit, or MCP
call, while the same operation invoked through `exec` and `tools.*` proceeds
without that client policy. Server hooks and the permission layer do not imply
that the client's separate policy is redundant.

Recommendation:

- Parameterize the client-hook path so nested calls can consume the decision
  without emitting an invalid paired top-level `tool_result`.
- Test client deny and rewrite behavior through real `exec` calls.

### 4. Code Mode invalidation is incomplete and has a generation TOCTOU

Severity: **High**

Evidence:

- `session_mode.rs:563-589` checks runtime generation once when a bridge message
  is dequeued and then spawns a separate local task.
- That task may wait in a server hook, permission prompt, or path lock before
  reaching `dispatch_tool` at `session_mode.rs:781-804`; it does not re-check
  generation immediately before dispatch.
- `tools/code_mode.rs:230-250` bumps generation synchronously but tears the
  runtime down asynchronously.
- The explicit `SessionCommand::Rewind` path at
  `session/acp_session_impl/run_loop.rs:1088-1093` calls
  `rewind.rs:163-541`, which contains no Code Mode shutdown/generation reset.
- The named cancel-history branch resets Code Mode at
  `tasks_cancel.rs:419-429`, but the legacy rewind branch at lines 622-640 only
  aborts the turn/terminates live cells and retains the runtime/store.
- Existing generation tests verify only that the counter increases, not the
  dequeue-to-dispatch race or explicit/legacy rewind behavior.

Impact:

A write accepted from the old generation can still dispatch after a model
switch, cancellation, or rewind if invalidation happens while it is waiting.
Separately, a yielded cell and `store()` state created in discarded history can
remain visible and callable after explicit or legacy rewind.

Recommendation:

- Carry the generation into `run_code_mode_nested_call` and check it again at
  the final dispatch boundary.
- Make every history rewind synchronously invalidate Code Mode and drop its
  store before the rewind is committed.
- Add barrier tests for dequeue -> wait -> invalidate -> dispatch and for
  yielded/store state across both rewind entry points.

### 5. Prompt-trace gating retains a model-switch TOCTOU

Severity: **High**

Evidence:

- `agent/mvp_agent/agent_ops.rs:3800-3816` checks Codex provenance when it
  creates `PromptTraceContext`.
- The context at lines 3907-3916 carries no provenance generation or live gate.
- ACP prompt handling obtains it at
  `agent/mvp_agent/acp_agent.rs:1084-1114` while holding the prompt dispatch
  lock.
- `set_session_model` at `acp_agent.rs:2163-2199` does not take that lock, so it
  can switch to Codex after the context has been created.
- Model switch marks chat state and persistence at
  `session/acp_session_impl/model_switch.rs:40-50`, but does not revoke an
  already-created trace context or upload queue.
- `session/acp_session_impl/turn.rs:2310-2329` re-checks before attaching the
  conversation trace, which is useful defense in depth, but tool-definition
  upload already begins from the old context at lines 2103-2116. A switch after
  the second check can also leave the request holding the old trace object.

Impact:

The following interleaving remains possible: an xAI session creates a trace
context, a concurrent model switch marks it Codex, and the turn continues to
upload through the previously admitted xAI-only trace pipeline. Sequential
initial/switch/resume cases are fixed; the final egress boundary is not atomic
with provenance.

Recommendation:

- Re-check the monotonic mark at the final attach/upload boundary, or bind
  trace contexts to a provider/provenance generation invalidated by switch.
- Add a barrier-controlled context-created -> Codex-switch -> upload test.

## Other required findings

### 6. Nested structured results still do not match the advertised contract

Severity: **Medium-High**

Evidence:

- `session_mode.rs:895-942` preserves only hand-built MCP text and ReadFile
  image/PDF cases; every other `ToolOutput`, including `Dynamic`, falls back to
  `prompt_text` as a string.
- Dynamic tools preserve arbitrary JSON in `DynamicOutput.value` at
  `xai-grok-tools/src/types/output.rs:46-56,653-654`, but that value is discarded
  at the nested bridge.
- Real `MCPOutput` stores only `OkayOutput(String)`/`Error(String)` plus
  `extracted_images` at `xai-grok-tools/src/types/output.rs:1185-1207`; the
  nested conversion at `session_mode.rs:908-916` emits one text block and drops
  those images.
- The MCP/tool description advertises `CallToolResult`, `structuredContent`,
  image/audio/resource blocks, and `image(result.content[0])` at
  `xai-grok-code-mode-protocol/src/description.rs:12-35,45-121`.
- `session_mode.rs:526-544` also publishes every nested `output_schema` as
  `None`.
- Tests at `session_mode.rs:1031-1075` cover only artificial MCP text,
  ReadFile image, and plain text, not real MCP media/resource or
  `DynamicOutput.value`.

Impact:

JavaScript cannot inspect fields of a dynamic structured result, and MCP image,
audio, resource, or `structuredContent` output cannot be forwarded using the
documented helpers. The original all-string behavior is improved but the public
contract is still false for important result classes.

Recommendation:

- Preserve the runtime/MCP `CallToolResult` value before prompt formatting.
- Return `DynamicOutput.value` directly and propagate actual MCP content blocks,
  `structuredContent`, `isError`, and metadata.
- Derive/preserve output schemas and add real bridge integration tests.

### 7. The one-401 allowance is scoped to a whole prompt, not one HTTP request

Severity: **Medium**

Evidence:

- `turn.rs:2125-2127` clears `codex_401_recovery_spent` only once when the prompt
  turn starts.
- A successful sampler response resets `AuthRetrySchedule` at `turn.rs:2537`
  but does not reset the Codex allowance.
- The next independent continuation request consumes the same flag at
  `sampler_turn.rs:467-486`.
- Current tests directly exercise the claim helper or manually clear the flag;
  they do not run two HTTP requests separated by a successful tool-call
  response in one prompt.

Impact:

If the first request gets 401, refreshes once, and returns a tool call, a later
continuation request in the same prompt cannot perform its own single bounded
refresh. A token expiring or rotating while a long tool runs turns a recoverable
second request into a terminal failure. Persistent 401 replay is bounded now,
but at too broad a scope.

Recommendation:

- Reset the Codex allowance whenever a sampler HTTP request completes
  successfully, alongside `AuthRetrySchedule::reset_on_success()`.
- Test `401 -> refresh -> successful tool call -> second 401 -> one refresh`
  and a persistent-401 request that still terminates after one replay.

### 8. Cross-process logout fencing fails open when the epoch cannot persist

Severity: **Medium**

Evidence:

- `codex_auth.rs:439-445` maps every unreadable/malformed logout epoch to zero.
- `codex_auth.rs:448-464` treats an epoch write as best-effort and returns no
  error.
- `logout_at` removes credentials, bumps the in-process generation, calls that
  best-effort write, and reports success at `codex_auth.rs:1295-1309`.
- A stale login checks only generation plus the re-read epoch at lines 882-900.

Impact:

A deterministic reproduction is to create `auth.json.logout-epoch` as a
directory. An old process records epoch zero; another process removes the auth
file but cannot write the tombstone and still reports logout success; the old
process re-reads zero and is allowed to recreate credentials. Normal
enqueue-time and healthy-filesystem races are fixed, but the durable latest
intent guarantee is fail-open.

Recommendation:

- Make an unreadable/malformed epoch and failed epoch write fail closed for
  stale login persistence.
- At minimum, report logout failure when the cross-process fence could not be
  established.
- Add the deterministic directory/malformed-tombstone test.

### 9. Remote compaction and `comp_hash` remain non-functional scaffolding

Severity: **Medium**

Evidence:

- `session/compaction.rs:2067-2077` explicitly hardcodes
  `current_codex_comp_hash()` to `None`.
- Consequently the same-slug hash-change path at lines 1997-2023 cannot fire.
- `shape_codex_remote_compaction_v2_body` and
  `codex_remote_compaction_v2_headers` in `xai-grok-sampler/src/client.rs` are
  still referenced only by their unit tests; no live compaction request calls
  them.
- The catalog correctly declares no compaction capability, and
  `docs/fable_review.md:204-210` now calls this deferred.

Impact:

There is no false runtime advertisement, so this is not a user-facing blocker.
It is still dead speculative surface under the repository's YAGNI rule and
must not be described as an implemented phase-8 capability.

Recommendation:

- Either complete the catalog metadata and end-to-end request path, or remove
  the unused helpers/state until that work is scheduled.

## Validation and maintenance gaps

- `.github/workflows/build.yml:14-17,42-53` deliberately excludes the entire
  `xai-grok-shell` test suite. `cargo check --workspace` does not compile
  `#[cfg(test)]` code, so most new session/subagent/privacy tests neither compile
  nor run in CI. Add at least deterministic shell library tests or a no-run test
  compilation job before treating CI green as validation of this port.
- `budgeted_parts` appends its truncation marker after consuming the requested
  token budget at `xai-grok-tools/src/implementations/code_mode/mod.rs:301-313`.
  A zero/small `max_tokens` request therefore returns a marker larger than its
  advertised cap. The limits are no longer ignored, but are not hard caps.
- Code Mode/V8/ICU dependencies remain unconditional in
  `xai-grok-shell/Cargo.toml:158-159` and `xai-grok-tools/Cargo.toml:111`, so
  Classic-only checks/builds still pay their compilation cost.
- `docs/fable_review.md:204-210` says phases 0-8 and both review remediations are
  complete while the same paragraph records the client-hook finding and remote
  compaction as deferred. That end-state claim should be narrowed.

## Reviewed areas that now appear sound

The following old findings were rechecked and removed from the active list:

- live-catalog-only provider identity in parent and child sessions;
- live Codex `tool_mode` mapping and unknown-value fail-closed behavior;
- logical nested failures rejecting the JavaScript promise;
- retained-history pruning synchronizing legacy `content` and ordered `parts`;
- immediate model-switch provenance marking before the next prompt;
- child Codex provenance tainting parent chat state and persistence before
  completion presentation/merge;
- Pager parsing the internally tagged `ToolOutput::CodeMode` shape;
- per-exec/per-wait output budgeting (the marker edge case is noted above);
- same-path serialization among concurrent nested writes;
- identity-less valid credentials counting as logged in while account-scoped
  catalog publication remains fingerprint-fenced;
- account/generation fencing of live catalog publication and current-model
  repair across logout/account change;
- stored-state global atomic merge and conservative serialized-size accounting;
- hosted-search provider gating, ordered tool results, custom-call IDs,
  unknown-event logging, durable stream recovery, and turn-state propagation;
- PR trigger/test job existence, macOS 15 migration, and the vendored crossterm
  upgrade record.

## Validation evidence

This was a read-only, three-subagent review of implementation code. The review
did not edit, stage, or commit production files; only this document was updated.

Completed checks:

    git diff --check
    git grep -iE 'opengrok|open-grok' -- ':!docs/' ':!.github/workflows/build.yml'

The forbidden-naming command returned no matches. Cargo tests and rustfmt could
not be run in this local environment because `cargo` is not installed. The
configured remote toolchain was not used because the actively changing local
worktree was not proven to be mirrored there. Existing unit tests therefore
support, but do not replace, the static lifecycle/privacy findings above.

## Recommended remediation order

1. Re-gate web search from the effective provider on switch and after child
   model resolution.
2. Separate real user auxiliary pins from remote defaults; gate classifier and
   the remaining auxiliary paths.
3. Make ACP client `PreToolUse` authoritative for nested calls.
4. Close the Code Mode generation/rewind invalidation boundaries.
5. Fence prompt traces at the final egress boundary.
6. Preserve actual dynamic/MCP structured results.
7. Scope one Codex refresh to each sampler request and fail closed on an
   unpersistable logout epoch.
8. Either finish or remove the remote-compaction scaffold, then compile/run the
   shell test suite in CI.
