use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cell_actor::CompletionCommit;

struct RecordingDelegate;

struct PanickingClosedDelegate;

impl SessionRuntimeDelegate for RecordingDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        Ok(JsonValue::Null)
    }

    async fn notify(
        &self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl SessionRuntimeDelegate for PanickingClosedDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        Ok(JsonValue::Null)
    }

    async fn notify(
        &self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }

    fn cell_closed(&self, _cell_id: &CellId) {
        panic!("cell close panic probe");
    }
}

#[tokio::test]
async fn reports_cell_actor_panics_to_the_owner() {
    let (failure_tx, mut failure_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = SessionRuntime::new_with_task_failure_handler(
        Arc::new(PanickingClosedDelegate),
        Some(Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
    );
    let started = runtime
        .execute(
            execute_request(r#"text("done");"#),
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        )
        .await
        .expect("start cell");
    assert_eq!(
        started.initial_event().await,
        Ok(CellEvent::Completed {
            content_items: vec![OutputItem::Text {
                text: "done".to_string(),
            }],
            error_text: None,
        })
    );
    runtime.shutdown().await.expect("shutdown runtime");
    let failure = failure_rx
        .try_recv()
        .expect("shutdown should wait for the cell failure watcher");
    assert!(failure.contains("code-mode cell 1 task failed"));
}

#[tokio::test]
async fn termination_rejects_a_waiting_store_commit_before_the_next_cell_can_load_it() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let cell_state = Arc::new(CellState::new(CancellationToken::new()));
    let host = RuntimeCellHost {
        cell_id: CellId::new("terminating-writer"),
        inner: Arc::clone(&runtime.inner),
    };
    let completion = CellEvent::Completed {
        content_items: vec![OutputItem::Text {
            text: "uncommitted output".to_string(),
        }],
        error_text: None,
    };

    let stored_values = runtime.inner.stored_values.lock().await;
    let commit = host.commit_completion(
        HashMap::from([(
            "candidate".to_string(),
            JsonValue::String("lost".to_string()),
        )]),
        completion.clone(),
        /*pending_initial_yield_items*/ None,
        Arc::clone(&cell_state),
    );
    tokio::pin!(commit);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    assert!(matches!(commit.as_mut().poll(&mut context), Poll::Pending));

    let termination = cell_state.request_termination();
    drop(stored_values);
    assert_eq!(commit.await, CompletionCommit::Rejected(completion));
    let terminated = CellEvent::Terminated {
        content_items: Vec::new(),
    };
    assert_eq!(
        cell_state.finish_termination(terminated.clone()),
        Some(terminated.clone())
    );
    assert_eq!(termination.await, Ok(terminated));
    assert!(
        !runtime
            .inner
            .stored_values
            .lock()
            .await
            .values
            .contains_key("candidate")
    );

    let reader = runtime
        .execute(
            CreateCellRequest {
                tool_call_id: "reader".to_string(),
                enabled_tools: Vec::new(),
                source: r#"text(String(load("candidate")));"#.to_string(),
                max_output_tokens: 10_000,
            },
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    assert_eq!(
        reader.initial_event().await,
        Ok(CellEvent::Completed {
            content_items: vec![OutputItem::Text {
                text: "undefined".to_string(),
            }],
            error_text: None,
        })
    );
    runtime.shutdown().await.unwrap();
}

fn execute_request(source: &str) -> CreateCellRequest {
    CreateCellRequest {
        tool_call_id: "call-1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        max_output_tokens: 10_000,
    }
}

/// Test delegate that parks every nested tool call on a shared barrier, so a
/// test can hold several cells at the same execution point (each with a
/// snapshot taken before any of them committed).
struct BarrierDelegate {
    barrier: tokio::sync::Barrier,
}

impl SessionRuntimeDelegate for BarrierDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        self.barrier.wait().await;
        Ok(JsonValue::Null)
    }

    async fn notify(
        &self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn gated_store_request(tool_call_id: &str, key: &str, value_bytes: usize) -> CreateCellRequest {
    CreateCellRequest {
        tool_call_id: tool_call_id.to_string(),
        enabled_tools: vec![ToolDefinition {
            name: "gate".to_string(),
            tool_name: ToolName {
                name: "gate".to_string(),
                namespace: None,
            },
            description: String::new(),
            kind: ToolKind::Function,
        }],
        source: format!(
            r#"await tools.gate(); store("{key}", "x".repeat({value_bytes})); text("stored");"#
        ),
        max_output_tokens: 10_000,
    }
}

/// Finding 6 regression: two cells that each snapshot an empty store and each
/// stay under the cell-local cap must not jointly exceed the session-wide cap
/// at the merge point. Exactly one delta merges; the loser's completion
/// carries an explicit error and its delta is discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cells_cannot_jointly_exceed_the_global_stored_state_cap() {
    const FIVE_MIB: usize = 5 * 1024 * 1024;
    let runtime = SessionRuntime::new(Arc::new(BarrierDelegate {
        barrier: tokio::sync::Barrier::new(2),
    }));
    let first = runtime
        .execute(
            gated_store_request("call-a", "a", FIVE_MIB),
            ObserveMode::YieldAfter(Duration::from_secs(60)),
        )
        .await
        .expect("start first cell");
    let second = runtime
        .execute(
            gated_store_request("call-b", "b", FIVE_MIB),
            ObserveMode::YieldAfter(Duration::from_secs(60)),
        )
        .await
        .expect("start second cell");

    let (first_event, second_event) = tokio::join!(first.initial_event(), second.initial_event());
    let events = [
        first_event.expect("first completion"),
        second_event.expect("second completion"),
    ];
    let mut rejected = 0;
    let mut committed = 0;
    for event in &events {
        let CellEvent::Completed { error_text, .. } = event else {
            panic!("expected a completion, got {event:?}");
        };
        match error_text {
            Some(text) => {
                assert!(
                    text.contains("store() writes were discarded at session merge"),
                    "unexpected completion error: {text}"
                );
                rejected += 1;
            }
            None => committed += 1,
        }
    }
    assert_eq!((committed, rejected), (1, 1), "events: {events:?}");

    let stored = runtime.inner.stored_values.lock().await;
    assert_eq!(stored.values.len(), 1, "exactly one delta must merge");
    assert!(
        stored.bytes <= crate::runtime::MAX_STORED_STATE_BYTES,
        "global accounting over cap: {}",
        stored.bytes
    );
    let expected_bytes: usize = stored
        .values
        .iter()
        .map(|(key, value)| crate::runtime::stored_entry_bytes(key, value))
        .sum();
    assert_eq!(stored.bytes, expected_bytes);
    drop(stored);
    runtime.shutdown().await.expect("shutdown runtime");
}

/// The merge-point check counts against the live global state: a delta that
/// fits merges even when the committing cell raced another writer, as long as
/// the merged total stays under the cap.
#[tokio::test]
async fn merge_point_accepts_deltas_that_fit_the_global_state() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let writer = runtime
        .execute(
            CreateCellRequest {
                tool_call_id: "writer".to_string(),
                enabled_tools: Vec::new(),
                source: r#"store("small", "value"); text("ok");"#.to_string(),
                max_output_tokens: 10_000,
            },
            ObserveMode::YieldAfter(Duration::from_secs(10)),
        )
        .await
        .expect("start writer");
    assert_eq!(
        writer.initial_event().await,
        Ok(CellEvent::Completed {
            content_items: vec![OutputItem::Text {
                text: "ok".to_string(),
            }],
            error_text: None,
        })
    );
    let stored = runtime.inner.stored_values.lock().await;
    assert_eq!(
        stored.values.get("small"),
        Some(&JsonValue::String("value".to_string()))
    );
    assert_eq!(
        stored.bytes,
        crate::runtime::stored_entry_bytes("small", &JsonValue::String("value".to_string()))
    );
    drop(stored);
    runtime.shutdown().await.expect("shutdown runtime");
}

#[tokio::test]
async fn cell_id_allocation_fails_before_wrapping() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    runtime
        .inner
        .next_cell_id
        .store(u64::MAX, Ordering::Relaxed);

    assert_eq!(
        runtime
            .execute(
                execute_request(r#"text("unreachable");"#),
                ObserveMode::YieldAfter(Duration::from_secs(1)),
            )
            .await
            .err(),
        Some(Error::CellIdSpaceExhausted)
    );
}

#[tokio::test]
// The test intentionally holds the registry lock to force admission ahead of
// shutdown. Codex configures this lock type for `await_holding_invalid_type`;
// Grok Build does not, so use an allow instead of an unfulfilled expectation.
#[allow(clippy::await_holding_invalid_type)]
async fn shutdown_rejects_cell_admission_queued_before_the_registry_lock() {
    let runtime = Arc::new(SessionRuntime::new(Arc::new(RecordingDelegate)));
    let cells = runtime.inner.cells.lock().await;

    let execution = runtime.execute(
        execute_request("while (true) {}"),
        ObserveMode::YieldAfter(Duration::from_millis(/*millis*/ 1)),
    );
    tokio::pin!(execution);
    std::future::poll_fn(|context| match execution.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(Ok(_)) => panic!("execution completed before the registry lock was released"),
        Poll::Ready(Err(error)) => {
            panic!("execution failed before the registry lock was released: {error}")
        }
    })
    .await;

    let shutdown = runtime.shutdown();
    tokio::pin!(shutdown);
    std::future::poll_fn(|context| match shutdown.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(Ok(())) => panic!("shutdown completed before acquiring the registry lock"),
        Poll::Ready(Err(error)) => {
            panic!("shutdown failed before acquiring the registry lock: {error}")
        }
    })
    .await;

    drop(cells);
    assert!(matches!(execution.await, Err(Error::ShuttingDown)));
    assert_eq!(shutdown.await, Ok(()));
}

#[tokio::test]
async fn drop_terminates_cells_when_the_registry_is_locked() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let started = runtime
        .execute(
            execute_request("while (true) {}"),
            ObserveMode::YieldAfter(Duration::from_millis(/*millis*/ 1)),
        )
        .await
        .unwrap();
    assert_eq!(started.cell_id, CellId::new("1"));
    assert_eq!(
        started.initial_event().await,
        Ok(CellEvent::Yielded {
            content_items: Vec::new(),
        })
    );

    let inner = Arc::clone(&runtime.inner);
    let cells = inner.cells.lock().await;
    drop(runtime);
    drop(cells);

    tokio::time::timeout(Duration::from_secs(/*secs*/ 1), inner.cell_tasks.wait())
        .await
        .unwrap();
    assert!(inner.cell_tasks.is_empty());
}
