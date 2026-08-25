# Codex subscription port review update

## Review conclusion

Review snapshot:

- Branch: `codex/sync-open-grok-codex`.
- HEAD: `1c83261a0e00b1b7101c8f3f8d49a5434eebdd80`.
- Full committed range: `4b12cf6f..1c83261a` (37 commits).
- Changes since the previous review snapshot: `48386052..1c83261a`
  (9 commits).
- Worktree inspected at 2026-08-24 23:32 +0800: clean, with no untracked
  files.
- Claude pushed `1c83261a` while this review was running. That commit changes
  only `.github/workflows/build.yml`; the provider, Code Mode, sampler, auth,
  and session findings below were rechecked against the same production code
  now present at HEAD.

The new commits are another substantial improvement. Live web-search gating,
local-only auxiliary consent provenance, ACP client deny handling inside Code
Mode, dequeue-to-dispatch generation checks, rewind runtime/store reset,
per-request Codex 401 recovery, fail-closed logout epochs, Dynamic JSON results,
successful MCP image forwarding, and truncation-marker accounting are now
implemented in reasonable locations with focused tests. The old findings for
those corrected behaviors have been deleted rather than retained as history.

The implementation is still not ready to merge as complete. Three High
boundaries remain: the automatic session-summary client is still pinned to the
provider active at session creation; several trace-upload paths still bypass
the live Codex-provenance latch; and a stale Code Mode call can retry or land
after invalidation/rewind. The public nested MCP result contract and Code Mode
output limits also remain incomplete.

Recommended disposition: **changes requested**.

## Blocking findings

### 1. Session-summary and other auxiliary calls are not fully live-provider fenced

Severity: **High**

The previous defect in which remote settings counted as user consent is fixed:
`AuxModelPin::Remote` is now non-explicit, and web search, image description,
and Auto Mode have provider checks. The automatic session-summary client,
however, is still created once from the provider active at session creation.

Evidence:

- `agent/mvp_agent/agent_ops.rs:94-135` calls
  `build_summary_client(primary)` and decides whether to resolve the auxiliary
  model using the initial `primary` provider and `session_summary_pin`.
- `agent/mvp_agent/session_setup.rs:427,441-450` gives that client/model to the
  persistence actor once.
- `session/persistence.rs:2324-2333,2379-2385` stores the client for the
  persistence actor's lifetime. Model switch has no update or revocation path
  for it.
- The first prompt is sent to persistence as a `ContentChunk` at
  `session/acp_session_impl/turn.rs:737-742`, triggers summary generation at
  `session/persistence.rs:1845-1855`, and is sampled with the stored client at
  `session/summary.rs:73-82`.
- Auto Mode rechecks only the sampling-config provider at
  `session/acp_session_impl/sampler_turn.rs:833-850,1012-1021` before using the
  auxiliary client at lines 851-913.
- A switch marks the synchronous Codex latch before awaiting the web-search
  gate at `model_switch.rs:30-40`, but does not publish the new sampling config
  until lines 76-90. The classifier can therefore observe the old xAI provider
  after Codex provenance is already marked. A switch after the check but before
  the HTTP call is another open window.
- Image description similarly snapshots provider/client before its request
  loop at `prompt_build.rs:962-1006`; a concurrent switch does not revoke the
  already-selected auxiliary client.

Impact:

One deterministic scenario is:

1. Create an unpinned or remote-default xAI session.
2. Before its first prompt, switch the session to Codex.
3. Send the first prompt.
4. The main request samples through Codex, but automatic title generation sends
   the original user prompt to the xAI summary client cached at session setup.

Auto Mode and a multi-image description request can cross the same privacy
boundary when a model switch interleaves with their snapshot/check and send.

Recommendation:

- Resolve or authorize the summary client at the actual side-call boundary
  from the live provider and the consent-grade pin; do not store an ungated
  client for the whole persistence lifetime.
- Make the monotonic Codex latch an immediate fail-closed condition for every
  xAI-hosted auxiliary call, including classifier and image description.
- Add xAI-spawn -> Codex-switch -> first-summary and barrier-controlled
  classifier/image-description switch tests.

### 2. Trace revocation still has direct-upload and subagent-completion bypasses

Severity: **High**

The shared `PromptTraceContext` path now checks the live monotonic latch at its
main enqueue/direct-upload boundaries. Not every trace egress uses that path.

Evidence:

- The common gate is in `upload/turn.rs:67-110`, and the generic upload paths
  consult it at `upload/trace.rs:537-551,1386-1401,1471-1482`.
- Tool definitions are checked in the caller at
  `session/acp_session_impl/turn.rs:2110-2134`, including once inside the
  spawned task.
- The actual `upload_tool_definitions` function at `upload/trace.rs:17-47`
  receives only a `TraceExportConfig`, auth manager, definitions, and tracker.
  It has no session handle/latch and calls `upload_bytes` directly after
  serialization. A model switch can land after the spawned-task check and
  before line 41 sends the bytes.
- After a child turn, `agent/subagent/handle_request.rs:1290-1307` reads
  `child_ever_used_codex` and correctly blocks the main turn trace.
- The same path still passes the original `gcs_upload_ctx` into
  `persist_subagent_completion` at `handle_request.rs:1496-1497`.
- `agent/subagent/mod.rs:2447-2477` does not check Codex provenance and spawns an
  upload of `subagent.json`, including model, status, error, role, and other
  completion metadata.
- The new revocation tests cover the common generic/deferred paths, not the
  tool-definition helper or Codex-tainted child completion upload.

Impact:

A switch can happen after the last caller-side tool-definition check, allowing
that artifact to enter the xAI-only trace pipeline. More directly, a child that
starts on xAI and switches to Codex suppresses its turn trace but can still
upload Codex-associated completion metadata through the stale context.

Recommendation:

- Pass `PromptTraceContext` or an equivalent live provenance guard into every
  direct upload helper and perform the final check inside that helper.
- When child Codex provenance is detected, clear or revoke both the turn-trace
  and completion-metadata upload contexts.
- Add barrier tests at check -> send and an xAI-child -> Codex-switch ->
  completion test.

### 3. Code Mode invalidation does not fence auth retry or synchronize rewind with in-flight writes

Severity: **High**

The old dequeue/hook/permission/path-lock gap and the missing rewind runtime
reset are fixed. The new final fence is still outside a retrying dispatch, and
rewind does not join already-dispatched effects.

Evidence:

- `session/acp_session_impl/session_mode.rs:840-867` checks the admitted Code
  Mode generation once, then enters `call_with_auth_retry`.
- `session/acp_session_impl/sampler_turn.rs:142-160` executes the call, awaits
  authentication recovery for an auth-shaped failure, and invokes the closure
  a second time without a generation check.
- `tools/code_mode.rs:230-247` bumps generation synchronously but performs
  runtime shutdown in a detached task. It also clears the shared path-lock map
  at line 239.
- Explicit rewind calls that detached shutdown at
  `session/acp_session_impl/rewind.rs:282-310` and then immediately starts
  restoring files. It does not wait for a nested call that already passed the
  generation fence and entered `dispatch_tool`.
- An old write can still hold an `Arc` to the removed path lock; rewind does not
  acquire that lock, so the write can complete after the restored snapshot.
- Tests at `session_mode.rs:1533-1605` invalidate while the call is still in a
  client gate, before the final check. Rewind tests verify generation/handle
  reset, not an auth retry or already-dispatched write racing restoration.

Impact:

For a nested side-effecting MCP/tool call:

1. The first attempt returns an auth-shaped error.
2. Model switch, cancel, or rewind bumps the generation while auth recovery is
   waiting.
3. Recovery succeeds and the old runtime retries the side effect anyway.

Separately, a file write that already dispatched can finish after rewind has
restored the file, overwriting the requested rewind state.

Recommendation:

- Recheck generation inside the retry closure before every attempt, including
  the post-refresh attempt.
- Track/join or cancel in-flight nested dispatches before committing a rewind;
  do not clear the only lock registry while old lock holders remain live.
- Add barrier tests for first-attempt 401 -> invalidate -> recovered retry and
  dispatch-started -> rewind-restore -> old-write completion.

## Other required findings

### 4. Nested MCP results still do not preserve the advertised `CallToolResult` contract

Severity: **Medium-High**

Dynamic JSON and successful MCP extracted images are now preserved. The actual
MCP transport result is still flattened before Code Mode sees it.

Evidence:

- `xai-grok-mcp/src/servers.rs:1545-1585` converts the server's
  `CallToolResult` into rendered strings. Error results retain only text;
  successful results drop audio and render resources/other blocks instead of
  retaining their typed shape. `structuredContent`, original `_meta`, and
  annotations are not preserved.
- `xai-grok-tools/src/types/output.rs:1185-1207` stores only a string
  `MCPOutputDetails`, transport flags, `is_error`, and extracted images.
- `session_mode.rs:974-1028` reconstructs a useful object from those remaining
  fields, but cannot recreate data discarded upstream.
- `session_mode.rs:887-927` returns `Err(prompt_text)` before
  `nested_result_value` whenever `output.is_error()` is true. Consequently the
  helper's `{isError: true, content, _meta}` object is unreachable from normal
  JavaScript execution.
- All nested `output_schema` values remain `None` at
  `session_mode.rs:529-555` because registration does not retain MCP
  `outputSchema`.

Impact:

An MCP application error can include structured repair information that code
should inspect and act on. The nested call instead rejects with a flat string;
audio/resource blocks, `structuredContent`, metadata, annotations, and output
schemas remain unavailable despite the public Code Mode description advertising
`CallToolResult` handling.

Recommendation:

- Preserve the original MCP `CallToolResult` through registration, dispatch,
  `MCPOutput`, and the nested bridge.
- Decide consistently whether MCP `isError` resolves as a typed result or
  rejects; do not build an `isError: true` object that the real path cannot
  return.
- Retain `outputSchema` during MCP registration and add integration tests using
  real text/image/audio/resource/structuredContent/error results.

### 5. Code Mode's advertised output cap is bypassable and is applied after unbounded materialization

Severity: **Medium-High**

The prior truncation-marker overflow is fixed, but the implementation is still
not a hard resource/output cap.

Evidence:

- `xai-grok-tools/src/implementations/code_mode/mod.rs:260-269` estimates each
  output part independently and sums the estimates.
- `xai-token-estimation/src/lib.rs:17-18` estimates tokens with integer
  `bytes / 4`, rounding every short part down separately.
- `code_mode/mod.rs:353-377` returns all parts immediately when that summed
  estimate fits, so any number of 1-3 byte text parts has zero estimated cost.
- For example, with `max_output_tokens: 0`, 100,000 calls to `text("abc")`
  produce about 300 KB of text whose computed cost is still zero.
- `xai-grok-code-mode/src/runtime/callbacks.rs:74-95` allocates each complete
  string and sends it through an unbounded channel.
- `xai-grok-code-mode/src/cell_actor/mod.rs:254-370` pushes every item into an
  unbounded `Vec`; the budget is not applied until the shell receives the full
  response at `code_mode/mod.rs:380-419`.
- `ExecuteRequest.max_output_tokens` is passed at `code_mode/mod.rs:491-514`
  but is not consumed by the V8 runtime. The isolate also uses default create
  parameters at `xai-grok-code-mode/src/runtime/mod.rs:201-210`, with no explicit
  heap/resource limit.

Impact:

Fragmented output violates even a zero-token advertised cap. Large strings,
many `text()` calls, or image data can consume arbitrary V8/Rust memory before
the post-response truncation runs, potentially terminating the shell.

Recommendation:

- Account text by cumulative bytes or coalesce adjacent text before estimating;
  add fragmented zero/tiny-budget tests.
- Enforce output limits while the cell actor receives events, not after all
  content has been materialized.
- Add a bounded channel and an explicit V8 heap/resource policy appropriate for
  this runtime.

### 6. Remote compaction and `comp_hash` remain non-functional scaffolding

Severity: **Medium**

Evidence:

- `session/compaction.rs:2067-2077` still hardcodes
  `current_codex_comp_hash()` to `None`; the same-slug hash-change branch at
  lines 1997-2023 cannot fire.
- `shape_codex_remote_compaction_v2_body` remains a standalone helper publicly
  re-exported at `xai-grok-sampler/src/lib.rs:40-43`.
- `SamplingClient::codex_remote_compaction_v2_headers()` at
  `xai-grok-sampler/src/client.rs:1082-1129` has no live compaction caller.
- The catalog correctly advertises no remote-compaction capability, and
  `docs/fable_review.md:204-208` now labels it deferred.

Impact:

There is no false user-facing capability, but the branch contains public API,
state, and tests for a path that cannot execute. It remains speculative surface
under the repository's YAGNI rule.

Recommendation:

- Either connect catalog metadata through the end-to-end request path, or
  remove the helpers/state until the feature is scheduled.

## Validation and maintenance gaps

- `.github/workflows/build.yml:63-71` now compiles the shell/tools/pager test
  targets with `cargo test --no-run`, so the old statement that `cfg(test)` was
  never compiled has been removed. At `94a79eea`, however, Actions run
  `32743778947` failed this step; the previous two runs identify the failure as
  `could not find native static library rusty_v8`.
- HEAD `1c83261a` adds `cargo clean -p v8 || true` at workflow lines 42-48 to
  force a clean archive fetch. This is a reasonable response to the partial
  cache, but its Actions run `32745022093` was still in progress at the review
  snapshot and cannot yet be recorded as successful.
- Even after `--no-run` succeeds, the provider/privacy, Code Mode, 401, logout,
  and session regression tests are only compiled, not executed. The workflow's
  `Core test suites` step does not run `xai-grok-shell`, `xai-grok-update`, or
  `xai-grok-pager-bin` tests.
- Code Mode/V8/ICU dependencies remain unconditional in
  `xai-grok-shell/Cargo.toml:158-159` and `xai-grok-tools/Cargo.toml:111`, so
  Classic-only checks still pay their compile/link cost.
- `xai-grok-update/src/version_policy.rs:1,96-97` still says required bounds
  refuse startup even though `enforce_version_policy_or_exit()` now only logs.
  `check_install_target()` at lines 62-75 also still applies remote
  `required_minimum` to an explicit `grok update --version`. The implementation
  is correct if the scope is only disabling automatic updates and the startup
  kill switch; it is incomplete if remote policy must never constrain any fork
  update operation.
- `docs/fable_review.md:204-210` now correctly avoids claiming full SPEC §19
  completion, so the old end-state contradiction has been deleted. Its later
  pending list remains stale: line 206 says remote-compaction wire analysis is
  complete while line 217 says it remains to be analyzed, and line 215 still
  treats target-platform V8 release compilation as unknown despite successful
  Linux/macOS release builds.
- Removing the never-compiling fast-worktree test does not hide a proven
  production regression: it referenced a nonexistent
  `confined::tests::plant_journal` helper. The tombstone comment left at
  `xai-fast-worktree/src/nfs/remove.rs:274-276` has no test value and should be
  removed; if that security scenario remains important, replace it with a
  compilable test using existing seams.

## Reviewed areas that now appear sound

The following prior findings were rechecked and removed from the active list:

- web search follows the effective provider across parent/child model overrides
  and model switches, hiding both the tool definition and client resource;
- remote auxiliary settings no longer count as local user consent;
- Code Mode nested calls consume ACP client `PreToolUse` deny decisions without
  writing an invalid top-level `tool_result`;
- invalidation after hook/permission/path-lock waiting is checked immediately
  before dispatch, and all explicit/legacy rewind paths reset runtime/store;
- Dynamic nested results and successful MCP extracted images retain structured
  values;
- the truncation marker is paid for from the requested budget;
- Codex 401 recovery is reset after each successful sampler HTTP response while
  persistent 401 remains bounded to one replay per request;
- logout epochs distinguish absent from corrupt/unreadable state, fail login
  persistence closed, and report logout fence failure;
- live/account/generation catalog fencing remains intact;
- Codex empty terminal `output: []` preserves completed streamed items;
- Codex system messages are reshaped without changing xAI requests;
- automatic update checks and the startup version kill switch are disabled for
  the fork;
- the CI shell test targets are now present, and `fable_review` no longer claims
  all SPEC §19 acceptance work is complete.

## Validation evidence

This was a read-only, three-subagent implementation review. The subagents did
not edit, stage, or commit any file; only this document was changed afterward.

Completed locally:

    git diff --check 48386052..1c83261a
    git grep -iE 'opengrok|open-grok' -- ':!docs/' ':!.github/workflows/build.yml'

Both checks passed; the naming command returned no matches. The local machine
still has no `cargo`, so no local Rust compilation or test execution was
performed.

Confirmed from GitHub Actions before the snapshot:

- At `94a79eea`, workspace check, core test suites, Linux release build, and the
  Linux `--version` smoke run passed.
- The shell/tools/pager no-run compilation failed at that commit.
- At HEAD `1c83261a`, the V8-cache remediation run was still in progress.

The prepared Rust installation on `xm-02-direct` was verified to exist at
`/data/cargo/bin/cargo` (`cargo 1.94.0`), but no repository checkout was present
under `/data`, so the review did not create/synchronize a remote worktree or use
unmatched remote results as evidence for this local commit.

## Recommended remediation order

1. Fence session-summary, classifier, and image-description calls from the live
   provider/Codex latch at their actual send boundaries.
2. Put every trace helper and child completion upload behind the same live
   provenance gate.
3. Recheck Code Mode generation on every auth retry and join/cancel in-flight
   nested writes before rewind restoration.
4. Preserve the real MCP `CallToolResult` and its output schema end to end.
5. Enforce Code Mode output/resource limits while output is produced, including
   fragmented short parts.
6. Complete or remove remote-compaction scaffolding.
7. Make the HEAD CI run green, then execute the deterministic shell/session
   privacy and lifecycle tests rather than compiling them only.
