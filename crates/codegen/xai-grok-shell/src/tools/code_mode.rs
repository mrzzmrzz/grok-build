//! Session-side Code Mode state: the lazily-created V8 runtime, the effective
//! per-model tool-mode plan, and the (Send) delegate that bridges nested tool
//! calls from cell tasks back into the session actor's `LocalSet`.
//!
//! Lifecycle:
//! - The plan (`ToolMode` + transport) is recomputed per turn
//!   (`SessionActor::refresh_code_mode_for_turn`) and on model switch, always
//!   from the merged account-scoped catalog; unknown models fail closed to
//!   Classic.
//! - The runtime is created on first use with `initialize_v8(Disabled)`
//!   (jitless); creation failure is surfaced as a turn error (fail closed —
//!   the session never silently degrades to the classic tool list).
//! - Cancel/close paths call [`CodeModeSessionState::terminate_live_cells_detached`]
//!   / [`CodeModeSessionState::shutdown_detached`]; dropping the state also
//!   begins runtime shutdown via `SessionRuntime`'s `Drop`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use xai_grok_code_mode::{InProcessCodeModeSession, V8JitMode, initialize_v8};
use xai_grok_code_mode_protocol as protocol;
use xai_grok_tools::implementations::code_mode::CodeModeHandle;

pub(crate) use crate::agent::config::ToolMode;

/// One message bridged from a running cell's (Send) delegate into the
/// session actor's `LocalSet` consumer.
pub(crate) enum CodeModeBridgeMsg {
    NestedCall {
        call: protocol::CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        respond_to: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Notify {
        tool_call_id: String,
        cell_id: protocol::CellId,
        text: String,
        respond_to: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

/// Effective Code Mode plan for the current model, cached so the synchronous
/// per-turn spec builders (`turn_base_tool_specs`, `hosted_tools_for_turn`)
/// can read it without an async catalog lookup.
#[derive(Clone, Debug, Default)]
pub(crate) struct CodeModeTurnPlan {
    /// Model the plan was computed for.
    pub(crate) model_id: String,
    /// Effective tool mode (catalog `tool_mode`, absent ⇒ Classic).
    pub(crate) mode: ToolMode,
    /// Transport for the `exec` tool; `None` while `mode` is Classic.
    pub(crate) transport: Option<xai_grok_sampling_types::CodeModeTransport>,
    /// Model-facing `exec` description generated from the current nested-tool
    /// projection.
    pub(crate) exec_description: String,
    /// Set when the runtime failed to initialize while the catalog declares a
    /// code mode; the turn fails closed on it.
    pub(crate) init_error: Option<String>,
}

struct CodeModeRuntime {
    session: Arc<InProcessCodeModeSession>,
    handle: CodeModeHandle,
}

/// Cloneable per-session Code Mode state, carried on
/// [`crate::tools::ToolContext`].
#[derive(Clone)]
pub struct CodeModeSessionState {
    inner: Arc<CodeModeInner>,
}

struct CodeModeInner {
    plan: parking_lot::Mutex<CodeModeTurnPlan>,
    runtime: parking_lot::Mutex<Option<CodeModeRuntime>>,
    live_cells: Arc<parking_lot::Mutex<HashSet<String>>>,
    /// Monotonic runtime generation. Bumped when a runtime is created and —
    /// synchronously, before any teardown work — whenever the runtime is
    /// invalidated (model switch, session close, rewind). The bridge consumer
    /// compares each queued nested call against this value and rejects stale
    /// work before dispatch, so a call queued under a previous runtime can
    /// never execute after the invalidation point.
    generation: std::sync::atomic::AtomicU64,
    /// Read-held for the duration of every nested dispatch. Rewind takes the
    /// write side after invalidating the generation, so it cannot restore a
    /// snapshot while an already-admitted write can still land afterwards.
    nested_dispatch_barrier: Arc<tokio::sync::RwLock<()>>,
    /// Per-path locks serializing concurrent nested writes to the same file
    /// (`Promise.all` parity with the batch path's per-file mutexes).
    nested_path_locks:
        parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Default for CodeModeSessionState {
    fn default() -> Self {
        Self {
            inner: Arc::new(CodeModeInner {
                plan: parking_lot::Mutex::new(CodeModeTurnPlan::default()),
                runtime: parking_lot::Mutex::new(None),
                live_cells: Arc::new(parking_lot::Mutex::new(HashSet::new())),
                generation: std::sync::atomic::AtomicU64::new(0),
                nested_dispatch_barrier: Arc::new(tokio::sync::RwLock::new(())),
                nested_path_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            }),
        }
    }
}

impl std::fmt::Debug for CodeModeSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeModeSessionState")
            .field("plan", &self.inner.plan.lock())
            .field("runtime_present", &self.inner.runtime.lock().is_some())
            .field("live_cells", &self.inner.live_cells.lock().len())
            .finish()
    }
}

impl CodeModeSessionState {
    const REWIND_DISPATCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

    pub(crate) fn plan(&self) -> CodeModeTurnPlan {
        self.inner.plan.lock().clone()
    }

    pub(crate) fn set_plan(&self, plan: CodeModeTurnPlan) {
        *self.inner.plan.lock() = plan;
    }

    /// Whether the cached plan is in a code mode (regardless of runtime health).
    pub(crate) fn code_mode_active(&self) -> bool {
        self.inner.plan.lock().mode.is_code_mode()
    }

    pub(crate) fn handle(&self) -> Option<CodeModeHandle> {
        self.inner.runtime.lock().as_ref().map(|r| r.handle.clone())
    }

    /// The currently valid runtime generation (see `CodeModeInner::generation`).
    pub(crate) fn current_generation(&self) -> u64 {
        self.inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Per-path lock for nested writes: nested calls dispatched concurrently
    /// (e.g. `Promise.all`) targeting the same file serialize on it, matching
    /// the batch path's per-file mutexes.
    pub(crate) fn nested_path_lock(&self, path: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .nested_path_locks
            .lock()
            .entry(path.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Hold this guard from the final generation check through completion of
    /// the nested tool effect. A rewind waits on the matching write guard.
    pub(crate) async fn nested_dispatch_guard(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        Arc::clone(&self.inner.nested_dispatch_barrier)
            .read_owned()
            .await
    }

    /// Lazily create the V8-backed session (jitless). On first creation the
    /// caller receives the bridge receiver (tagged with the new runtime
    /// generation) to consume on its `LocalSet`.
    ///
    /// Fails closed: any initialization error is returned (and should abort
    /// the turn); no runtime is stored.
    pub(crate) fn ensure_runtime(
        &self,
    ) -> Result<
        (
            CodeModeHandle,
            Option<(tokio::sync::mpsc::UnboundedReceiver<CodeModeBridgeMsg>, u64)>,
        ),
        String,
    > {
        let mut runtime = self.inner.runtime.lock();
        if let Some(existing) = runtime.as_ref() {
            return Ok((existing.handle.clone(), None));
        }
        initialize_v8(V8JitMode::Disabled)
            .map_err(|error| format!("code mode V8 initialization failed: {error}"))?;
        let (bridge_tx, bridge_rx) = tokio::sync::mpsc::unbounded_channel();
        let delegate = Arc::new(SessionBridgeDelegate {
            tx: bridge_tx,
            live_cells: Arc::clone(&self.inner.live_cells),
        });
        let session = Arc::new(
            InProcessCodeModeSession::with_delegate_and_task_failure_handler(
                delegate,
                Arc::new(|reason| {
                    tracing::error!(reason = %reason, "code mode runtime task failed");
                }),
            ),
        );
        let handle = CodeModeHandle {
            session: session.clone() as Arc<dyn protocol::CodeModeSession>,
            enabled_tools: Arc::new(parking_lot::RwLock::new(Vec::new())),
            live_cells: Arc::clone(&self.inner.live_cells),
        };
        *runtime = Some(CodeModeRuntime {
            session,
            handle: handle.clone(),
        });
        let generation = self
            .inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        Ok((handle, Some((bridge_rx, generation))))
    }

    /// Refresh the nested-tool projection the next `exec` call will expose.
    pub(crate) fn update_enabled_tools(&self, tools: Vec<protocol::ToolDefinition>) {
        if let Some(runtime) = self.inner.runtime.lock().as_ref() {
            *runtime.handle.enabled_tools.write() = tools;
        }
    }

    /// Explicitly terminate every cell that yielded and may still be running.
    /// Used by turn-cancel paths: aborting the turn future only drops the
    /// `exec`/`wait` callers, not the isolates.
    pub(crate) fn terminate_live_cells_detached(&self, reason: &'static str) {
        let session = match self.inner.runtime.lock().as_ref() {
            Some(runtime) => Arc::clone(&runtime.session),
            None => return,
        };
        let cells: Vec<String> = self.inner.live_cells.lock().drain().collect();
        if cells.is_empty() {
            return;
        }
        tracing::info!(count = cells.len(), reason, "terminating code mode cells");
        tokio::spawn(async move {
            for cell in cells {
                let _ = session.terminate(protocol::CellId::new(cell)).await;
            }
        });
    }

    /// Tear the runtime down (model switch away, session close/hard-stop).
    /// The stored `store()` state is dropped with it. Detached with a bounded
    /// wait so close budgets cannot be blown.
    pub(crate) fn shutdown_detached(&self, reason: &'static str) {
        // Synchronous invalidation boundary: bump the generation FIRST so the
        // bridge consumer rejects queued nested work before any of the
        // asynchronous teardown below runs.
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let runtime = self.inner.runtime.lock().take();
        self.inner.live_cells.lock().clear();
        self.inner.nested_path_locks.lock().clear();
        let Some(runtime) = runtime else { return };
        tracing::info!(reason, "shutting down code mode runtime");
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                runtime.session.shutdown(),
            )
            .await;
            // Dropping the session afterwards also cancels anything left via
            // `SessionRuntime`'s `Drop`.
        });
    }

    /// Invalidate Code Mode and wait until every nested tool dispatch that
    /// crossed the old generation fence has completed. Rewind must use this
    /// form before restoring files; otherwise an old write can finish after
    /// the snapshot and overwrite the requested state.
    pub(crate) async fn shutdown_for_rewind(&self) -> Result<(), String> {
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let runtime = self.inner.runtime.lock().take();
        self.inner.live_cells.lock().clear();

        // Publish runtime cancellation before waiting on the dispatch write
        // side. A nested call parked in the bridge consumer may release its
        // read guard only when this token fires; waiting first would deadlock.
        if let Some(runtime) = runtime.as_ref() {
            runtime.session.begin_shutdown();
        }
        if let Some(runtime) = runtime {
            tracing::info!(
                reason = "explicit rewind",
                "shutting down code mode runtime"
            );
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    runtime.session.shutdown(),
                )
                .await;
            });
        }

        // The write guard is fair: stale calls that were still waiting on a
        // path lock may queue here, but once they pass it they observe the
        // bumped generation and return without dispatching. Fail the rewind
        // instead of waiting forever if an external tool ignores cancellation;
        // the caller must not restore files without this exclusion boundary.
        let _dispatch_guard = self
            .wait_for_nested_dispatches(Self::REWIND_DISPATCH_DRAIN_TIMEOUT)
            .await?;
        self.inner.nested_path_locks.lock().clear();
        Ok(())
    }

    async fn wait_for_nested_dispatches(
        &self,
        timeout: Duration,
    ) -> Result<tokio::sync::OwnedRwLockWriteGuard<()>, String> {
        tokio::time::timeout(
            timeout,
            Arc::clone(&self.inner.nested_dispatch_barrier).write_owned(),
        )
        .await
        .map_err(|_| {
            format!(
                "rewind aborted: a Code Mode nested tool did not stop within {} seconds",
                timeout.as_secs_f64()
            )
        })
    }
}

/// Send delegate handed to the code-mode session; forwards nested calls and
/// notifications over an unbounded channel to the session actor's consumer
/// task (which runs on the actor's `LocalSet` and can re-enter the permission
/// and plan-mode gates).
struct SessionBridgeDelegate {
    tx: tokio::sync::mpsc::UnboundedSender<CodeModeBridgeMsg>,
    live_cells: Arc<parking_lot::Mutex<HashSet<String>>>,
}

impl protocol::CodeModeSessionDelegate for SessionBridgeDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: protocol::CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> protocol::ToolInvocationFuture<'a> {
        let tx = self.tx.clone();
        Box::pin(async move {
            let (respond_to, response) = tokio::sync::oneshot::channel();
            tx.send(CodeModeBridgeMsg::NestedCall {
                call: invocation,
                cancellation_token: cancellation_token.clone(),
                respond_to,
            })
            .map_err(|_| "code mode nested-tool bridge is closed".to_string())?;
            tokio::select! {
                biased;
                result = response => {
                    result.unwrap_or_else(|_| Err("nested tool dispatch was dropped".to_string()))
                }
                _ = cancellation_token.cancelled() => {
                    Err("nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: protocol::CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> protocol::NotificationFuture<'a> {
        let tx = self.tx.clone();
        Box::pin(async move {
            let (respond_to, response) = tokio::sync::oneshot::channel();
            tx.send(CodeModeBridgeMsg::Notify {
                tool_call_id: call_id,
                cell_id,
                text,
                respond_to,
            })
            .map_err(|_| "code mode notify bridge is closed".to_string())?;
            tokio::select! {
                biased;
                result = response => {
                    result.unwrap_or_else(|_| Err("notify delivery was dropped".to_string()))
                }
                _ = cancellation_token.cancelled() => Err("notify cancelled".to_string()),
            }
        })
    }

    fn cell_closed(&self, cell_id: &protocol::CellId) {
        self.live_cells.lock().remove(cell_id.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_code_mode_protocol::CodeModeSessionDelegate as _;

    fn delegate() -> (
        SessionBridgeDelegate,
        tokio::sync::mpsc::UnboundedReceiver<CodeModeBridgeMsg>,
        Arc<parking_lot::Mutex<HashSet<String>>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let live_cells = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        (
            SessionBridgeDelegate {
                tx,
                live_cells: Arc::clone(&live_cells),
            },
            rx,
            live_cells,
        )
    }

    fn nested_call() -> protocol::CodeModeNestedToolCall {
        protocol::CodeModeNestedToolCall {
            cell_id: protocol::CellId::new("1".to_string()),
            runtime_tool_call_id: "tool-1".to_string(),
            tool_name: protocol::ToolName::plain("read_file"),
            tool_kind: protocol::CodeModeToolKind::Function,
            input: Some(serde_json::json!({"path": "x"})),
        }
    }

    /// A nested call is bridged over the channel and the consumer's reply
    /// resolves the delegate future.
    #[tokio::test]
    async fn nested_call_round_trips_through_the_bridge() {
        let (delegate, mut rx, _cells) = delegate();
        let token = CancellationToken::new();
        let call_future = delegate.invoke_tool(nested_call(), token);
        let consumer = async {
            let Some(CodeModeBridgeMsg::NestedCall {
                call, respond_to, ..
            }) = rx.recv().await
            else {
                panic!("expected a nested call");
            };
            assert_eq!(call.tool_name.to_string(), "read_file");
            respond_to
                .send(Ok(serde_json::Value::String("file contents".to_string())))
                .unwrap();
        };
        let (result, ()) = tokio::join!(call_future, consumer);
        assert_eq!(
            result.expect("nested call should succeed"),
            serde_json::Value::String("file contents".to_string())
        );
    }

    /// Cancellation unblocks a nested call even when the consumer never
    /// replies (the cell's promise rejects cleanly).
    #[tokio::test]
    async fn nested_call_cancellation_rejects_cleanly() {
        let (delegate, _rx, _cells) = delegate();
        let token = CancellationToken::new();
        token.cancel();
        let err = delegate
            .invoke_tool(nested_call(), token)
            .await
            .expect_err("cancelled call must error");
        assert!(err.contains("cancelled"), "{err}");
    }

    /// A dropped bridge (session gone) fails the nested call instead of
    /// hanging the cell.
    #[tokio::test]
    async fn closed_bridge_fails_nested_calls() {
        let (delegate, rx, _cells) = delegate();
        drop(rx);
        let err = delegate
            .invoke_tool(nested_call(), CancellationToken::new())
            .await
            .expect_err("closed bridge must error");
        assert!(err.contains("bridge is closed"), "{err}");
    }

    /// `cell_closed` clears live-cell tracking (cancel sweep bookkeeping).
    #[tokio::test]
    async fn cell_closed_untracks_live_cells() {
        let (delegate, _rx, cells) = delegate();
        cells.lock().insert("7".to_string());
        delegate.cell_closed(&protocol::CellId::new("7".to_string()));
        assert!(cells.lock().is_empty());
    }

    /// Finding-6 fence: `shutdown_detached` bumps the runtime generation
    /// synchronously — before any asynchronous teardown — so the bridge
    /// consumer can reject queued work the moment the invalidation point
    /// (model switch / close / rewind) is reached.
    #[test]
    fn shutdown_bumps_the_generation_synchronously() {
        let state = CodeModeSessionState::default();
        let before = state.current_generation();
        state.shutdown_detached("test invalidation");
        assert_eq!(
            state.current_generation(),
            before + 1,
            "generation must advance before teardown work runs"
        );
        // Repeated invalidations keep advancing (idempotent teardown, fresh
        // fence each time).
        state.shutdown_detached("again");
        assert_eq!(state.current_generation(), before + 2);
    }

    /// Same path ⇒ same lock instance (Promise.all writes serialize);
    /// different paths stay independent. Invalidation clears the map.
    #[test]
    fn nested_path_locks_key_by_path() {
        let state = CodeModeSessionState::default();
        let a1 = state.nested_path_lock("/repo/a.rs");
        let a2 = state.nested_path_lock("/repo/a.rs");
        let b = state.nested_path_lock("/repo/b.rs");
        assert!(Arc::ptr_eq(&a1, &a2), "same path must share one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "different paths must not share");
        state.shutdown_detached("reset");
        let a3 = state.nested_path_lock("/repo/a.rs");
        assert!(!Arc::ptr_eq(&a1, &a3), "invalidation clears the lock map");
    }

    /// A nested dispatch that ignores cancellation cannot make rewind hang
    /// forever. The write-side drain fails closed, so the caller can refuse to
    /// restore the snapshot while the old effect is still live.
    #[tokio::test]
    async fn rewind_dispatch_drain_times_out_on_hung_dispatch() {
        let state = CodeModeSessionState::default();
        let _hung_dispatch = state.nested_dispatch_guard().await;
        let error = state
            .wait_for_nested_dispatches(Duration::from_millis(10))
            .await
            .expect_err("a held dispatch guard must time out");
        assert!(error.contains("rewind aborted"), "{error}");
    }
}
