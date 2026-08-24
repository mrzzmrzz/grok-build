mod types;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value as JsonValue;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) use self::types::CellEvent;
pub(crate) use self::types::CellId;
pub(crate) use self::types::CreateCellRequest;
pub(crate) use self::types::Error;
pub(crate) use self::types::ImageDetail;
pub(crate) use self::types::NestedToolCall;
pub(crate) use self::types::ObserveMode;
pub(crate) use self::types::OutputItem;
pub(crate) use self::types::SessionRuntimeDelegate;
pub(crate) use self::types::ToolDefinition;
pub(crate) use self::types::ToolKind;
pub(crate) use self::types::ToolName;
use crate::TaskFailureHandler;
use crate::cell_actor::CellActor;
use crate::cell_actor::CellError;
use crate::cell_actor::CellEventFuture;
use crate::cell_actor::CellHandle;
use crate::cell_actor::CellHost;
use crate::cell_actor::CellState;
use crate::cell_actor::CellToolCall;
use crate::cell_actor::CompletionCommit;

type RuntimeEventFuture = Pin<Box<dyn Future<Output = Result<CellEvent, Error>> + Send + 'static>>;

/// Owns all cells and shared state for one transport-neutral code-mode session.
pub(crate) struct SessionRuntime<D: SessionRuntimeDelegate> {
    inner: Arc<Inner<D>>,
}

/// Session-global stored `store()` state plus its running serialized size.
///
/// `bytes` is the sum of [`crate::runtime::stored_entry_bytes`] over `values`
/// and is maintained at the single merge point
/// ([`RuntimeCellHost::commit_completion`]) so the 8 MiB cap can be enforced
/// atomically against the *global* state, not each cell's snapshot.
struct StoredState {
    values: HashMap<String, JsonValue>,
    bytes: usize,
}

struct Inner<D: SessionRuntimeDelegate> {
    stored_values: Mutex<StoredState>,
    cells: Mutex<HashMap<CellId, CellHandle>>,
    cell_tasks: TaskTracker,
    shutdown_token: CancellationToken,
    delegate: Arc<D>,
    task_failure_handler: Option<TaskFailureHandler>,
    next_cell_id: AtomicU64,
}

impl<D: SessionRuntimeDelegate> SessionRuntime<D> {
    pub(crate) fn new(delegate: Arc<D>) -> Self {
        Self::new_with_task_failure_handler(delegate, /*task_failure_handler*/ None)
    }

    pub(crate) fn new_with_task_failure_handler(
        delegate: Arc<D>,
        task_failure_handler: Option<TaskFailureHandler>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                stored_values: Mutex::new(StoredState {
                    values: HashMap::new(),
                    bytes: 0,
                }),
                cells: Mutex::new(HashMap::new()),
                cell_tasks: TaskTracker::new(),
                shutdown_token: CancellationToken::new(),
                delegate,
                task_failure_handler,
                next_cell_id: AtomicU64::new(1),
            }),
        }
    }

    pub(crate) async fn execute(
        &self,
        request: CreateCellRequest,
        initial_observe_mode: ObserveMode,
    ) -> Result<StartedCell, Error> {
        if self.inner.shutdown_token.is_cancelled() {
            return Err(Error::ShuttingDown);
        }
        let cell_id = self.allocate_cell_id()?;
        let initial_event = self
            .start_cell(cell_id.clone(), request, initial_observe_mode)
            .await?;
        Ok(StartedCell {
            cell_id,
            initial_event,
        })
    }

    pub(crate) async fn observe(
        &self,
        cell_id: &CellId,
        mode: ObserveMode,
    ) -> Result<CellEvent, Error> {
        self.begin_observe(cell_id, mode).await?.event().await
    }

    pub(crate) async fn begin_observe(
        &self,
        cell_id: &CellId,
        mode: ObserveMode,
    ) -> Result<PendingEvent, Error> {
        let handle = self
            .inner
            .cells
            .lock()
            .await
            .get(cell_id)
            .cloned()
            .ok_or_else(|| Error::MissingCell(cell_id.clone()))?;
        Ok(PendingEvent {
            event: map_actor_event(cell_id.clone(), handle.observe(mode)),
        })
    }

    pub(crate) async fn terminate(&self, cell_id: &CellId) -> Result<CellEvent, Error> {
        let handle = self
            .inner
            .cells
            .lock()
            .await
            .get(cell_id)
            .cloned()
            .ok_or_else(|| Error::MissingCell(cell_id.clone()))?;
        handle
            .terminate()
            .await
            .map_err(|error| actor_error(cell_id, error))
    }

    pub(crate) async fn shutdown(&self) -> Result<(), Error> {
        self.begin_shutdown();
        // Taking the registry lock ensures every cell that passed the shutdown
        // check has registered its actor with the tracker before we wait.
        let cells = self.inner.cells.lock().await;
        self.inner.cell_tasks.close();
        drop(cells);
        self.inner.cell_tasks.wait().await;
        Ok(())
    }

    fn allocate_cell_id(&self) -> Result<CellId, Error> {
        self.inner
            .next_cell_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next_cell_id| {
                next_cell_id.checked_add(1)
            })
            .map(|cell_id| CellId::new(cell_id.to_string()))
            .map_err(|_| Error::CellIdSpaceExhausted)
    }

    async fn start_cell(
        &self,
        cell_id: CellId,
        request: CreateCellRequest,
        initial_observe_mode: ObserveMode,
    ) -> Result<RuntimeEventFuture, Error> {
        let stored_values = self.inner.stored_values.lock().await.values.clone();
        let host = Arc::new(RuntimeCellHost {
            cell_id: cell_id.clone(),
            inner: Arc::clone(&self.inner),
        });
        let mut cells = self.inner.cells.lock().await;
        if self.inner.shutdown_token.is_cancelled() {
            return Err(Error::ShuttingDown);
        }
        if cells.contains_key(&cell_id) {
            return Err(Error::DuplicateCell(cell_id));
        }
        let cell_state = Arc::new(CellState::new(self.inner.shutdown_token.child_token()));
        let (handle, initial_event, task) = CellActor::prepare(
            request,
            stored_values,
            host,
            initial_observe_mode,
            cell_state,
            self.inner.task_failure_handler.clone(),
        )
        .map_err(Error::Runtime)?;
        cells.insert(cell_id.clone(), handle);
        let task = self.inner.cell_tasks.spawn(task);
        if let Some(task_failure_handler) = self.inner.task_failure_handler.clone() {
            let failed_cell_id = cell_id.clone();
            let _failure_watcher = self.inner.cell_tasks.spawn(async move {
                if let Err(err) = task.await {
                    task_failure_handler(format!(
                        "code-mode cell {failed_cell_id} task failed: {err}"
                    ));
                }
            });
        }
        drop(cells);
        Ok(map_actor_event(cell_id, initial_event))
    }

    pub(crate) fn begin_shutdown(&self) {
        self.inner.shutdown_token.cancel();
        self.inner.cell_tasks.close();
    }
}

impl<D: SessionRuntimeDelegate> Drop for SessionRuntime<D> {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

/// A cell admitted by [`SessionRuntime::execute`].
pub(crate) struct StartedCell {
    pub(crate) cell_id: CellId,
    initial_event: RuntimeEventFuture,
}

impl StartedCell {
    pub(crate) async fn initial_event(self) -> Result<CellEvent, Error> {
        self.initial_event.await
    }
}

/// An admitted observation that has not reached its requested frontier yet.
pub(crate) struct PendingEvent {
    event: RuntimeEventFuture,
}

impl PendingEvent {
    pub(crate) async fn event(self) -> Result<CellEvent, Error> {
        self.event.await
    }
}

struct RuntimeCellHost<D: SessionRuntimeDelegate> {
    cell_id: CellId,
    inner: Arc<Inner<D>>,
}

impl<D: SessionRuntimeDelegate> CellHost for RuntimeCellHost<D> {
    async fn invoke_tool(
        &self,
        invocation: CellToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        self.inner
            .delegate
            .invoke_tool(
                NestedToolCall {
                    cell_id: self.cell_id.clone(),
                    runtime_tool_call_id: invocation.id,
                    tool_name: invocation.name,
                    tool_kind: invocation.kind,
                    input: invocation.input,
                },
                cancellation_token,
            )
            .await
    }

    async fn notify(
        &self,
        call_id: String,
        text: String,
        cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        self.inner
            .delegate
            .notify(call_id, self.cell_id.clone(), text, cancellation_token)
            .await
    }

    async fn commit_completion(
        &self,
        stored_value_writes: HashMap<String, JsonValue>,
        event: CellEvent,
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        cell_state: Arc<CellState>,
    ) -> CompletionCommit {
        let cancellation_token = cell_state.cancellation_token();
        let mut stored = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                return CompletionCommit::Rejected(event);
            }
            stored = self.inner.stored_values.lock() => stored,
        };
        if stored_value_writes.is_empty() {
            return cell_state.commit_completion(event, pending_initial_yield_items, || {});
        }
        // Session-wide atomic cap check at the single merge point: project the
        // global size with this cell's writes applied. The isolate-local check
        // in `runtime::callbacks` only saw the cell's own snapshot, so two
        // concurrent cells can each pass it while their merged state exceeds
        // the cap.
        let projected = stored_value_writes
            .iter()
            .fold(stored.bytes, |projected, (key, value)| {
                let new_bytes = crate::runtime::stored_entry_bytes(key, value);
                let replaced_bytes = stored
                    .values
                    .get(key)
                    .map(|old| crate::runtime::stored_entry_bytes(key, old))
                    .unwrap_or(0);
                projected
                    .saturating_sub(replaced_bytes)
                    .saturating_add(new_bytes)
            });
        if projected > crate::runtime::MAX_STORED_STATE_BYTES {
            // Semantics: the completion itself still commits (the cell's
            // content items are delivered), but its store() delta is rejected
            // wholesale and the violation is surfaced explicitly to the cell
            // observer via `error_text`.
            let note = format!(
                "store() writes were discarded at session merge: total stored session state \
                 would be {projected} bytes, over the {} byte limit (another cell may have \
                 stored data concurrently). The cell's other output is preserved. Overwrite \
                 large keys with null to free space.",
                crate::runtime::MAX_STORED_STATE_BYTES
            );
            let event = append_completion_error(event, &note);
            return cell_state.commit_completion(event, pending_initial_yield_items, || {});
        }
        cell_state.commit_completion(event, pending_initial_yield_items, || {
            for (key, value) in stored_value_writes {
                let new_bytes = crate::runtime::stored_entry_bytes(&key, &value);
                let replaced_bytes = stored
                    .values
                    .get(&key)
                    .map(|old| crate::runtime::stored_entry_bytes(&key, old))
                    .unwrap_or(0);
                stored.bytes = stored
                    .bytes
                    .saturating_sub(replaced_bytes)
                    .saturating_add(new_bytes);
                stored.values.insert(key, value);
            }
        })
    }

    async fn closed(&self) {
        self.inner.cells.lock().await.remove(&self.cell_id);
        self.inner.delegate.cell_closed(&self.cell_id);
    }
}

/// Fold a merge-point rejection note into a completion event's `error_text`.
///
/// Only `Completed` events can carry store writes (they arrive with the
/// runtime's `Result` event); any other shape passes through unchanged.
fn append_completion_error(event: CellEvent, note: &str) -> CellEvent {
    match event {
        CellEvent::Completed {
            content_items,
            error_text,
        } => CellEvent::Completed {
            content_items,
            error_text: Some(match error_text {
                Some(existing) => format!("{existing}\n{note}"),
                None => note.to_string(),
            }),
        },
        other => other,
    }
}

fn map_actor_event(cell_id: CellId, event: CellEventFuture) -> RuntimeEventFuture {
    Box::pin(async move { event.await.map_err(|error| actor_error(&cell_id, error)) })
}

fn actor_error(cell_id: &CellId, error: CellError) -> Error {
    match error {
        CellError::Busy => Error::BusyObserver(cell_id.clone()),
        CellError::AlreadyTerminating => Error::AlreadyTerminating(cell_id.clone()),
        CellError::Closed => Error::ClosedCell(cell_id.clone()),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
