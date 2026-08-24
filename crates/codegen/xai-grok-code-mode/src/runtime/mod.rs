mod callbacks;
mod globals;
mod module_loader;
mod timers;
mod value;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::mpsc as std_mpsc;
use std::thread;

use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use xai_grok_code_mode_protocol::CodeModeToolKind;
use xai_grok_code_mode_protocol::EnabledToolMetadata;
use xai_grok_code_mode_protocol::ExecuteRequest;
use xai_grok_code_mode_protocol::FunctionCallOutputContentItem;
use xai_grok_code_mode_protocol::ToolName;
use xai_grok_code_mode_protocol::enabled_tool_metadata;

use crate::TaskFailureHandler;
use crate::v8_init::ensure_v8_initialized;

// Sentinel value kept byte-identical to upstream OpenAI Codex so diffs against
// the source revision stay clean; it never leaves the isolate.
const EXIT_SENTINEL: &str = "__codex_code_mode_exit__";

/// Ceiling on the total serialized size of `store()`d session state. The map
/// is re-injected into every fresh isolate and persisted with the session, so
/// unbounded growth degrades every later `exec` call in the conversation.
///
/// The isolate-local check in [`callbacks`] is only a fast-fail heuristic
/// against the cell's snapshot; the authoritative, session-wide check runs
/// atomically at the completion merge point (see
/// `session_runtime::RuntimeCellHost::commit_completion`), so two concurrent
/// cells cannot each pass a local check and jointly exceed the cap.
pub(crate) const MAX_STORED_STATE_BYTES: usize = 8 * 1024 * 1024;

/// Serialized footprint of one stored entry, as counted against
/// [`MAX_STORED_STATE_BYTES`].
///
/// Counts the serialized representation of one `"key":value` member of the
/// stored-state JSON object: the quoted-and-escaped key (so escaped keys are
/// not undercounted), one colon, the compact serialization of the value, and
/// **two** bytes of structural overhead per entry. A real object with `n`
/// entries carries `n - 1` commas plus 2 braces = `n + 1` structural bytes;
/// charging 2 per entry (`2n ≥ n + 1` for `n ≥ 1`) keeps the accounting
/// strictly conservative — a map accounted at the cap can never serialize
/// larger than the cap.
pub(crate) fn stored_entry_bytes(key: &str, value: &JsonValue) -> usize {
    let serialized_key_bytes = serde_json::to_string(key)
        .map(|serialized| serialized.len())
        .unwrap_or(key.len() + 2);
    serialized_key_bytes + 1 + value.to_string().len() + 2
}

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse { id: String, result: JsonValue },
    ToolError { id: String, error_text: String },
    TimeoutFired { id: u64 },
    ObservePendingFrontier,
    Terminate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PendingRuntimeMode {
    #[cfg(test)]
    Continue,
    PauseUntilResumed,
}

#[derive(Debug)]
pub(crate) enum RuntimeControlCommand {
    Continue,
    Resume,
    Terminate,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    Pending,
    ContentItem(FunctionCallOutputContentItem),
    YieldRequested,
    ToolCall {
        id: String,
        name: ToolName,
        kind: CodeModeToolKind,
        input: Option<JsonValue>,
    },
    Notify {
        call_id: String,
        text: String,
    },
    Result {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
    },
    ThreadPanicked,
}

pub(crate) fn spawn_runtime(
    stored_values: HashMap<String, JsonValue>,
    request: ExecuteRequest,
    event_tx: mpsc::Sender<RuntimeEvent>,
    pending_mode: PendingRuntimeMode,
    task_failure_handler: Option<TaskFailureHandler>,
) -> Result<
    (
        std_mpsc::Sender<RuntimeCommand>,
        std_mpsc::Sender<RuntimeControlCommand>,
        v8::IsolateHandle,
    ),
    String,
> {
    ensure_v8_initialized()?;

    let (command_tx, command_rx) = std_mpsc::channel();
    let (control_tx, control_rx) = std_mpsc::channel();
    let runtime_command_tx = command_tx.clone();
    let (isolate_handle_tx, isolate_handle_rx) = std_mpsc::sync_channel(1);
    let enabled_tools = request
        .enabled_tools
        .iter()
        .map(enabled_tool_metadata)
        .collect::<Vec<_>>();
    let config = RuntimeConfig {
        tool_call_id: request.tool_call_id,
        enabled_tools,
        source: request.source,
        stored_values,
    };

    spawn_supervised_runtime_thread(event_tx.clone(), task_failure_handler, move || {
        run_runtime(
            config,
            event_tx,
            command_rx,
            control_rx,
            pending_mode,
            isolate_handle_tx,
            runtime_command_tx,
        );
    });

    let isolate_handle = isolate_handle_rx
        .recv()
        .map_err(|_| "failed to initialize code mode runtime".to_string())?;
    Ok((command_tx, control_tx, isolate_handle))
}

fn spawn_supervised_runtime_thread(
    event_tx: mpsc::Sender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
    runtime: impl FnOnce() + Send + 'static,
) {
    thread::spawn(move || {
        if catch_unwind(AssertUnwindSafe(runtime)).is_err() {
            if let Some(task_failure_handler) = task_failure_handler {
                task_failure_handler("code-mode V8 runtime thread panicked".to_string());
            }
            let _ = event_tx.blocking_send(RuntimeEvent::ThreadPanicked);
        }
    });
}

#[derive(Clone)]
struct RuntimeConfig {
    tool_call_id: String,
    enabled_tools: Vec<EnabledToolMetadata>,
    source: String,
    stored_values: HashMap<String, JsonValue>,
}

pub(super) struct RuntimeState {
    event_tx: mpsc::Sender<RuntimeEvent>,
    pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    stored_values: HashMap<String, JsonValue>,
    stored_value_writes: HashMap<String, JsonValue>,
    /// Running total of [`stored_entry_bytes`] across `stored_values`.
    stored_bytes: usize,
    enabled_tools: Vec<EnabledToolMetadata>,
    next_tool_call_id: u64,
    next_timeout_id: u64,
    tool_call_id: String,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    exit_requested: bool,
}

pub(super) enum CompletionState {
    Pending,
    Completed {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
    },
}

fn run_runtime(
    config: RuntimeConfig,
    event_tx: mpsc::Sender<RuntimeEvent>,
    command_rx: std_mpsc::Receiver<RuntimeCommand>,
    control_rx: std_mpsc::Receiver<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
    isolate_handle_tx: std_mpsc::SyncSender<v8::IsolateHandle>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
) {
    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
    let isolate_handle = isolate.thread_safe_handle();
    if isolate_handle_tx.send(isolate_handle).is_err() {
        return;
    }
    isolate.set_host_import_module_dynamically_callback(module_loader::dynamic_import_callback);

    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let stored_bytes = config
        .stored_values
        .iter()
        .map(|(key, value)| stored_entry_bytes(key, value))
        .sum();
    scope.set_slot(RuntimeState {
        event_tx: event_tx.clone(),
        pending_tool_calls: HashMap::new(),
        pending_timeouts: HashMap::new(),
        stored_values: config.stored_values,
        stored_value_writes: HashMap::new(),
        stored_bytes,
        enabled_tools: config.enabled_tools,
        next_tool_call_id: 1,
        next_timeout_id: 1,
        tool_call_id: config.tool_call_id,
        runtime_command_tx,
        exit_requested: false,
    });

    if let Err(error_text) = globals::install_globals(scope) {
        send_result(&event_tx, HashMap::new(), Some(error_text));
        return;
    }

    let _ = event_tx.blocking_send(RuntimeEvent::Started);

    let pending_promise = match module_loader::evaluate_main_module(scope, &config.source) {
        Ok(pending_promise) => pending_promise,
        Err(error_text) => {
            capture_scope_send_error(scope, &event_tx, Some(error_text));
            return;
        }
    };

    match module_loader::completion_state(scope, pending_promise.as_ref()) {
        CompletionState::Completed {
            stored_value_writes,
            error_text,
        } => {
            send_result(&event_tx, stored_value_writes, error_text);
            return;
        }
        CompletionState::Pending => {}
    }

    let mut pending_promise = pending_promise;
    while let Some(command) =
        next_runtime_command(&event_tx, &command_rx, &control_rx, pending_mode)
    {
        match command {
            RuntimeCommand::Terminate => break,
            RuntimeCommand::ToolResponse { id, result } => {
                if let Err(error_text) =
                    module_loader::resolve_tool_response(scope, &id, Ok(result))
                {
                    capture_scope_send_error(scope, &event_tx, Some(error_text));
                    return;
                }
            }
            RuntimeCommand::ToolError { id, error_text } => {
                if let Err(runtime_error) =
                    module_loader::resolve_tool_response(scope, &id, Err(error_text))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::TimeoutFired { id } => {
                if let Err(runtime_error) = timers::invoke_timeout_callback(scope, id) {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::ObservePendingFrontier => {}
        }

        scope.perform_microtask_checkpoint();
        match module_loader::completion_state(scope, pending_promise.as_ref()) {
            CompletionState::Completed {
                stored_value_writes,
                error_text,
            } => {
                send_result(&event_tx, stored_value_writes, error_text);
                return;
            }
            CompletionState::Pending => {}
        }

        if let Some(promise) = pending_promise.as_ref() {
            let promise = v8::Local::new(scope, promise);
            if promise.state() != v8::PromiseState::Pending {
                pending_promise = None;
            }
        }
    }
}

fn next_runtime_command(
    event_tx: &mpsc::Sender<RuntimeEvent>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    control_rx: &std_mpsc::Receiver<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
) -> Option<RuntimeCommand> {
    loop {
        match command_rx.try_recv() {
            Ok(command) => return Some(command),
            Err(std_mpsc::TryRecvError::Disconnected) => return None,
            Err(std_mpsc::TryRecvError::Empty) => {}
        }

        let _ = event_tx.blocking_send(RuntimeEvent::Pending);
        match pending_mode {
            #[cfg(test)]
            PendingRuntimeMode::Continue => return command_rx.recv().ok(),
            PendingRuntimeMode::PauseUntilResumed => match control_rx.recv().ok()? {
                RuntimeControlCommand::Continue => return command_rx.recv().ok(),
                RuntimeControlCommand::Resume => continue,
                RuntimeControlCommand::Terminate => return Some(RuntimeCommand::Terminate),
            },
        }
    }
}

fn capture_scope_send_error(
    scope: &mut v8::PinScope<'_, '_>,
    event_tx: &mpsc::Sender<RuntimeEvent>,
    error_text: Option<String>,
) {
    let stored_value_writes = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.stored_value_writes.clone())
        .unwrap_or_default();

    send_result(event_tx, stored_value_writes, error_text);
}

fn send_result(
    event_tx: &mpsc::Sender<RuntimeEvent>,
    stored_value_writes: HashMap<String, JsonValue>,
    error_text: Option<String>,
) {
    let _ = event_tx.blocking_send(RuntimeEvent::Result {
        stored_value_writes,
        error_text,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::ExecuteRequest;
    use super::PendingRuntimeMode;
    use super::RuntimeCommand;
    use super::RuntimeControlCommand;
    use super::RuntimeEvent;
    use super::spawn_runtime;
    use super::spawn_supervised_runtime_thread;
    use crate::FunctionCallOutputContentItem;

    fn execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
        }
    }

    /// Escaped-key boundary: the accounting must charge the serialized
    /// (quoted, escaped) key representation, not the raw key length, plus the
    /// member punctuation, so escape-heavy keys cannot undercount.
    #[test]
    fn stored_entry_bytes_counts_escaped_keys_and_punctuation() {
        let value = serde_json::json!({"a": 1});
        let value_len = value.to_string().len();

        let plain = super::stored_entry_bytes("plain_key", &value);
        // "plain_key" serializes to 9 + 2 quote bytes; +1 colon +2 structural.
        assert_eq!(plain, 9 + 2 + value_len + 3);

        let escaped_key = "quote\"backslash\\newline\n";
        let escaped = super::stored_entry_bytes(escaped_key, &value);
        let serialized_key_len = serde_json::to_string(escaped_key).unwrap().len();
        assert_eq!(escaped, serialized_key_len + value_len + 3);
        assert!(
            escaped > escaped_key.len() + 2 + value_len + 3,
            "escaped key must count larger than its raw length"
        );
    }

    /// The per-entry accounting must never undercount the real serialized
    /// map: `sum(stored_entry_bytes) >= serde_json::to_string(map).len()`.
    #[test]
    fn accounting_is_conservative_against_real_serialization() {
        for entries in [1usize, 2, 5] {
            let mut map = serde_json::Map::new();
            for i in 0..entries {
                map.insert(format!("key_{i}\""), serde_json::json!({"v": i}));
            }
            let accounted: usize = map
                .iter()
                .map(|(k, v)| super::stored_entry_bytes(k, v))
                .sum();
            let actual = serde_json::Value::Object(map).to_string().len();
            assert!(
                accounted >= actual,
                "accounted {accounted} < actual {actual} for {entries} entries"
            );
        }
    }

    #[tokio::test]
    async fn runtime_thread_panic_before_initialization_is_reported_directly() {
        let (event_tx, event_rx) = mpsc::channel(256);
        drop(event_rx);
        let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            Some(std::sync::Arc::new(move |reason| {
                let _ = failure_tx.send(reason);
            })),
            || panic!("runtime thread panic probe"),
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
                .await
                .expect("runtime failure timeout")
                .expect("runtime failure"),
            "code-mode V8 runtime thread panicked"
        );
    }

    #[tokio::test]
    async fn runtime_thread_panic_is_forwarded_without_owner_supervision() {
        let (event_tx, mut event_rx) = mpsc::channel(256);
        spawn_supervised_runtime_thread(
            event_tx,
            /*task_failure_handler*/ None,
            || panic!("runtime thread panic probe"),
        );

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("runtime panic event timeout"),
            Some(RuntimeEvent::ThreadPanicked)
        ));
    }

    #[tokio::test]
    async fn terminate_execution_stops_cpu_bound_module() {
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let (_runtime_tx, _runtime_control_tx, runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request("while (true) {}"),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let started_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(started_event, RuntimeEvent::Started));

        assert!(runtime_terminate_handle.terminate_execution());

        let result_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::Result { error_text, .. } = result_event else {
            panic!("expected runtime result after termination");
        };
        assert!(error_text.is_some());

        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn pending_mode_freezes_runtime_commands_until_resume() {
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let (runtime_tx, runtime_control_tx, _runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request(
                r#"
await new Promise((resolve) => setTimeout(resolve, 60_000));
text("after");
await new Promise(() => {});
"#,
            ),
            event_tx,
            PendingRuntimeMode::PauseUntilResumed,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Started
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Pending
        ));

        runtime_tx
            .send(RuntimeCommand::TimeoutFired { id: 1 })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .is_err()
        );

        runtime_control_tx
            .send(RuntimeControlCommand::Resume)
            .unwrap();

        let content_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) =
            content_event
        else {
            panic!("expected resumed runtime output");
        };
        assert_eq!(text, "after");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Pending
        ));

        runtime_control_tx
            .send(RuntimeControlCommand::Terminate)
            .unwrap();
    }
}
