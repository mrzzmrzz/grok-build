# Codex subscription port review update

## Review conclusion

Review range: `4b12cf6f..f2d14471` (17 commits).

The overall direction is reasonable, and substantial infrastructure is already
in place for Codex authentication, provider-specific sampling, model catalog
discovery, Responses streaming, Code Mode state, terminal theme updates, and
release artifacts. However, the current implementation is not ready to merge or
release as complete. Several cross-layer lifecycle and state-consistency gaps
remain, including four issues on current user-facing paths.

Recommended disposition: **changes requested**.

## Blocking findings

### 1. Live catalog-only Codex models lose their provider identity

Severity: **High**

Evidence:

- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/model_switch.rs:49`
  stores a reduced sampling config without a provider/profile field.
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:434`
  re-derives provider facts from the model slug before inference.
- `crates/codegen/xai-grok-shell/src/agent/config.rs:5124` resolves those facts
  from the static/effective config using `resolve_model_list(&cfg, None)`, not
  from the `ModelsManager` catalog containing live Codex models.
- `crates/codegen/xai-grok-shell/src/agent/config.rs:5097` defaults an unknown
  slug to `ModelProvider::Xai`.
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:498`
  consequently retains `ProviderProfile::XAI`; the Codex bearer resolver is
  installed only in the `is_codex` branch at lines 536-549.

Impact:

A model returned only by the authenticated Codex catalog can appear in the
picker, but its inference request may be reconstructed as an xAI request. It
then uses the wrong provider profile and header dialect and does not mount the
Codex OAuth bearer resolver. The bundled static model can work while other
live-discovered models fail.

Recommendation:

- Preserve provider identity as part of the session/chat-state sampling config,
  instead of re-inferring it from a different model source at request time.
- Alternatively, make reconstruction query the same merged, account-scoped
  catalog that supplied the picker entry. Do not default a selected live model
  to xAI merely because it is absent from static config.
- Add an end-to-end test that inserts a Codex-only slug into the live catalog,
  selects it through the model manager, reconstructs the sampler config, and
  asserts the Codex profile, bearer resolver, endpoint, and reserved headers.

### 2. The Codex proactive refresh loop is never started

Severity: **High**

Evidence:

- `crates/codegen/xai-grok-shell/src/codex_auth.rs:1204` defines an immediate
  and four-minute proactive refresh loop.
- No production path calls `codex_auth::start_proactive_refresh`.
- `crates/codegen/xai-grok-shell/src/codex_auth.rs:1343` only reloads the current
  access token from disk for requests.
- The retry/401 path in
  `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:945-1028`
  refreshes through the xAI `AuthManager`, not through Codex OAuth.

Impact:

A long-running Codex session eventually sends an expired token and has no
Codex-specific 401 recovery. It starts working again only if another action,
such as usage lookup, catalog refresh, or process restart, happens to refresh
the credential store.

Recommendation:

- Start the Codex refresh task from the same process lifecycle that owns its
  cancellation token.
- Add a Codex-specific forced-refresh path for an eligible 401, with a bounded
  single replay and account-identity revalidation.
- Test both proactive expiry rotation and `401 -> refresh -> one retry`, while
  asserting that xAI credentials are never used on the Codex path.

### 3. Codex login and logout do not preserve the user's latest intent

Severity: **High**

Evidence:

- `crates/codegen/xai-grok-pager/src/app/dispatch/auth.rs:37-57` dispatches each
  Codex login/logout independently.
- `crates/codegen/xai-grok-pager/src/app/effects/mod.rs:3562-3580` spawns
  independent asynchronous tasks without an operation generation or stale
  result check.
- Login persists credentials after its browser callback at
  `crates/codegen/xai-grok-shell/src/codex_auth.rs:805-813`.
- Logout revokes and deletes credentials at
  `crates/codegen/xai-grok-shell/src/codex_auth.rs:1028-1086`.

Impact:

If `/login codex` is waiting for its browser callback and the user then runs
`/logout codex`, the logout can complete first. Completing the older browser
flow afterward writes credentials again and reverses the user's final logout
intent. The reverse ordering is also unsafe because remote revocation occurs
before the logout path takes its local auth lock.

Recommendation:

- Assign an operation generation to Codex auth commands and discard completion
  from any operation older than the current generation.
- Cancel a pending login callback when logout begins, where practical.
- Serialize credential mutation and make the ordering cover remote revoke plus
  local persistence/deletion, not only the final file operation.
- Add deterministic tests for `login -> logout -> old login callback` and
  `logout in revoke -> new login -> old logout completes`.

### 4. `x-codex-turn-state` is not propagated through sampler-internal retries

Severity: **High**

Evidence:

- `crates/codegen/xai-grok-sampler/src/actor/request_task.rs:128-155` retains a
  local `ConversationRequest` and clones it for each attempt.
- Handshake metadata is emitted to the shell at
  `crates/codegen/xai-grok-sampler/src/actor/request_task.rs:734-743`.
- The shell updates its session slot at
  `crates/codegen/xai-grok-shell/src/session/acp_session_impl/session_setup.rs:517-529`,
  but the already-running sampler retry loop does not read that slot again.
- `crates/codegen/xai-grok-sampler/tests/codex_turn_state.rs:145-185`
  manually rebuilds requests and does not execute the real retry loop.

Impact:

When the first HTTP handshake returns a new turn-state and the body then fails
before a usable response, the next internal attempt clones the original request
and sends the old turn-state, commonly `None`. This violates the intended
request-affinity lifecycle for retries of the same logical prompt.

Recommendation:

- Keep the latest turn-state inside the request task/retry state, and update the
  next attempt directly from response metadata before deciding to retry.
- Do not rely on an asynchronous shell notification as the only feedback path
  for state required by the currently executing retry loop.
- Replace or supplement the manual test with an actor-level test in which the
  first mocked response returns a turn-state and then fails, and the second
  captured request must echo that new value.

## High-risk latent findings for Code Mode

These issues are not broadly user-facing yet because Code Mode ordered output
is not fully wired into the product path. They should still be fixed before
that integration is enabled.

### 5. `ToolResultItem.parts` creates two drifting sources of truth

Severity: **High when ordered tool output is enabled**

Evidence:

- `crates/codegen/xai-grok-sampling-types/src/conversation/responses.rs:287-318`
  treats non-empty `parts` as authoritative for Responses serialization.
- Existing pruning still mutates only legacy `content` at
  `crates/codegen/xai-chat-state/src/actor/request_builder.rs:151-175`.
- Image accounting and eviction mutate only legacy `images` at
  `crates/codegen/xai-chat-state/src/image_budget.rs:217-219,268-315`.
- Persisted-image sanitation only clears legacy images at
  `crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs:1747-1750`.
- Workspace path rewriting and compaction similarly mutate only mirrors at
  `crates/codegen/xai-grok-sampling-types/src/conversation.rs:2153-2156` and
  `crates/codegen/xai-chat-state/src/compaction_utils.rs:230-239`.

Impact:

Text or images removed from the legacy mirrors can remain in `parts` and still
be sent on the wire. This can bypass old-result trimming and image budgets,
retain stale workspace paths, and replay malformed persisted image data.

Recommendation:

- Use one canonical representation for ordered tool results. Derive legacy
  views only at compatibility boundaries rather than persisting two mutable
  representations.
- Until that migration is complete, every mutation must update `parts` and its
  mirrors atomically through one helper.
- Add wire-level post-mutation tests for pruning, image eviction, persisted
  history sanitation, path rewriting, and compaction.

### 6. The 8 MiB stored-state cap is cell-local, not session-wide

Severity: **High when concurrent Code Mode cells are enabled**

Evidence:

- Each cell clones a state snapshot at
  `crates/codegen/xai-grok-code-mode/src/session_runtime/mod.rs:156-178`.
- `store()` checks only isolate-local accounting at
  `crates/codegen/xai-grok-code-mode/src/runtime/callbacks.rs:211-229`.
- Completion directly merges updates without a global size check at
  `crates/codegen/xai-grok-code-mode/src/session_runtime/mod.rs:273-290`.
- `stored_entry_bytes()` at
  `crates/codegen/xai-grok-code-mode/src/runtime/mod.rs:36-38` also omits JSON
  object punctuation and key escaping overhead.

Impact:

Two cells starting from an empty snapshot can each store 5 MiB and independently
pass the 8 MiB check. Their completion updates then merge into a roughly 10 MiB
session state. Escaped keys can cause additional undercounting.

Recommendation:

- Enforce the limit atomically at the session merge/commit point against the
  current global state.
- Define whether a conflicting completion must fail entirely or whether only
  its store delta is rejected, and surface that result explicitly to the cell.
- Compute the size from the actual serialized representation, or document and
  test a conservative accounting formula.
- Add a concurrent two-cell regression test and escaped-key boundary tests.

## Non-blocking but required follow-ups

### 7. Durable stream recovery does not cover normal transport failures

Severity: **Medium**

Evidence:

- Idle timeout returns failure immediately at
  `crates/codegen/xai-grok-sampler/src/stream/responses.rs:350-362`.
- Transport/serialization error returns failure immediately at lines 365-397.
- Durable output is used only after clean EOF at lines 824-842.
- Message-only incomplete recovery is converted to a truncation failure at
  `crates/codegen/xai-grok-sampler/src/actor/request_task.rs:699-703`.

Recommendation:

- Run the same durable-output decision for clean EOF, retryable stream error,
  and idle timeout after at least one complete output item.
- Define a terminal status that lets a fully completed recovered message reach
  the caller instead of being reclassified as max-token truncation.
- Add actor-level tests for `done item -> socket error`, `done item -> timeout`,
  and message-only recovery.

### 8. The in-memory Codex model catalog is not account-scoped

Severity: **Medium**

Evidence:

- `CatalogState.codex_models` has no account fingerprint at
  `crates/codegen/xai-grok-shell/src/agent/models.rs:107`.
- It is retained by the clear path at lines 1147-1159.
- Login notifies listeners before background refresh at
  `crates/codegen/xai-grok-shell/src/extensions/codex.rs:82-88`.

Impact:

After account A logs out and account B logs in, A's retained catalog can become
visible to B before refresh. It can remain visible if refresh is disabled,
fails, or is skipped because an older request is still in flight.

Recommendation:

- Store the account fingerprint with the in-memory catalog and require a match
  before making entries visible.
- Clear or quarantine account-scoped entries on logout and account transition.
- Replace the process-wide in-flight boolean with refresh state keyed by account
  generation, or schedule the new account refresh when an obsolete request
  finishes.
- Test account A logout followed by account B login under success, failure,
  disabled-fetch, and old-refresh-in-flight conditions.

### 9. Logout can leave `currentModelId` outside `availableModels`

Severity: **Medium**

Evidence:

- Logout only refreshes model visibility at
  `crates/codegen/xai-grok-shell/src/extensions/codex.rs:107-116`.
- `available()` removes unauthorized Codex models at
  `crates/codegen/xai-grok-shell/src/agent/models.rs:515-543`.
- `current_model_id()` independently returns the old model at lines 552-554.
- The pager accepts that current ID at
  `crates/codegen/xai-grok-pager/src/app/acp_handler/settings.rs:14-37`.

Recommendation:

- Define and enforce the invariant that the active model is usable under the
  current authentication state.
- On logout, either switch to a deterministic available fallback or mark the
  session as requiring model selection before another turn.
- Add a test that selects a Codex model, logs out, consumes the model update,
  and verifies both UI state and the next-turn behavior.

### 10. Auto theme can remain stale after returning from a child process

Severity: **Medium**

Evidence:

- Child startup disables terminal theme updates at
  `crates/codegen/xai-grok-pager/src/app/event_loop.rs:457-462`.
- Child exit only re-enables updates at lines 487-492, without requesting the
  current mode.
- Lines 497-501 then drain buffered crossterm events indiscriminately.
- Initial setup correctly combines enable and one-shot request at
  `crates/codegen/xai-grok-pager/src/app/mod.rs:1460-1471`.
- The stored terminal report remains the highest-priority appearance source at
  `crates/codegen/xai-grok-pager-render/src/theme/system_appearance.rs:160-171`.

Recommendation:

- After the child exits and stale input has been drained, issue a fresh
  `RequestThemeMode` in addition to re-enabling notifications.
- Alternatively, preserve and process `ThemeModeChanged` while draining instead
  of discarding every buffered event.
- Add a PTY-level regression test that changes the reported theme while a child
  owns the terminal and verifies immediate reconciliation after resume.

## Lower-priority issues

### Announcement visibility

The Welcome fallback at
`crates/codegen/xai-grok-pager/src/app/app_view.rs:4607-4619` only rechecks
critical severity. It can redisplay a hidden, expired, or empty critical
announcement that the primary selection path correctly filtered.

Recommendation: use one visibility predicate for both primary selection and
fallback, covering severity, hidden IDs, expiry, and non-empty content.

### Custom tool call ID namespace

The v2 encoding in
`crates/codegen/xai-grok-sampling-types/src/conversation.rs:528-583` correctly
round-trips genuine custom IDs, including Unicode and colons. However, a normal
function-call ID that happens to match the reserved v2 syntax is interpreted as
a custom call.

Recommendation: preserve call kind explicitly in persisted state rather than
inferring it only from a string prefix. At minimum, document the reserved
namespace and add a collision regression test.

### Unknown Responses event logging

Unknown top-level events are ignored by the stream layer, but the decoder logs
the full raw frame at error level first:

- `crates/codegen/xai-grok-sampler/src/client.rs:137-163`
- `crates/codegen/xai-grok-sampler/src/stream/responses.rs:365-388`

Recommendation: classify unknown event kinds before error logging, log them at
debug/trace level with bounded metadata, and avoid logging an unbounded raw
payload.

### Warning and maintenance debt

- `crates/codegen/xai-grok-shell/src/codex_models.rs` introduces 21
  `unreachable_pub` warnings under the crate's existing lint policy.
- The workspace-wide vendored crossterm patch introduces a large dependency
  snapshot and six warnings for a narrowly scoped terminal protocol change.
- `.github/workflows/build.yml` builds and smoke-tests artifacts but does not run
  tests or trigger on pull requests.
- The workflow uses the deprecated `macos-14` runner image, which GitHub plans to
  remove on 2026-11-02.

Recommendation:

- Restrict internal Codex model types/functions to `pub(crate)` or private
  visibility as appropriate.
- Keep the crossterm patch minimal and document its upstream provenance and
  upgrade path if vendoring remains necessary.
- Add a pull-request validation job for targeted checks/tests, separate from
  the release artifact build.
- Migrate the macOS build before the runner removal deadline.

## Reviewed areas that appear sound

The following portions were reviewed without finding an additional concrete
defect:

- Credential precedence and provider-header isolation for models that are
  correctly identified as Codex.
- A single per-request credential snapshot for bearer, account ID, and FedRAMP
  headers.
- Account fingerprint, ETag, TTL, and conditional 304 handling for the disk
  model catalog cache.
- UTF-8 byte-length handling and genuine custom-tool v2 ID round-trips.
- Ordered tool-output serialization before any later mutation.
- Terminal mode 2031 initialization, report parsing, normal event routing,
  SSH/PTY compatibility, and shutdown cleanup.
- Removal of passive promotional UI while retaining critical operational
  announcements and controls.
- Linux x86_64 and macOS arm64 release builds, version smoke tests, binary rename
  to `grok`, and distinct artifact names.

## Validation evidence

The following targeted check completed successfully:

```text
cargo check --all-targets --locked \
  -p xai-grok-shell \
  -p xai-grok-sampler \
  -p xai-grok-pager \
  -p xai-grok-code-mode \
  -p xai-grok-code-mode-protocol \
  -p xai-chat-state
```

It completed in approximately 6 minutes 57 seconds. It also surfaced the 21
new `xai-grok-shell` visibility warnings and six vendored crossterm warnings.

The full workspace all-target check reached an unrelated pre-existing test
compile error at
`crates/codegen/xai-fast-worktree/src/nfs/remove.rs:317`; blame attributes that
line to the base history rather than this review range.

GitHub Actions run `32691191173` passed release build, version smoke, rename, and
artifact upload for both `linux-x86_64` and `macos-aarch64`. The workflow does
not run the state-machine and retry tests needed to detect the findings above.

## Recommended remediation order

Before merge or release:

1. Preserve live Codex provider identity through inference reconstruction.
2. Start Codex proactive refresh and implement bounded Codex 401 recovery.
3. Order/cancel login and logout operations according to the latest user intent.
4. Propagate the latest turn-state inside the actual sampler retry loop.
5. Scope the in-memory model catalog to the authenticated account.
6. Restore the `currentModelId`/`availableModels` invariant after logout.
7. Make durable recovery work for transport errors, timeout, and message-only
   output.

Before enabling Code Mode product wiring:

8. Establish one canonical ordered tool-result representation.
9. Enforce stored-state limits atomically at the session commit point.

Follow-up quality work:

10. Reconcile terminal theme after child-process resume.
11. Unify announcement visibility filtering.
12. Resolve warnings, CI coverage gaps, and the macOS runner lifecycle issue.
