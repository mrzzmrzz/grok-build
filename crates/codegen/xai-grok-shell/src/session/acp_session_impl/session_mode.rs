//! Session/plan-mode concern for `SessionActor` (`handle_session_mode`,
//! plan-mode reminders and persistence, active-template detection), plus the
//! Code Mode session integration (per-turn plan refresh, nested-call bridge,
//! cell termination).
use super::*;
use super::tool_calls::{PlanEditGate, plan_mode_edit_gate};
pub(super) fn prompt_mode_from_session_mode_id(session_mode_id: &acp::SessionModeId) -> PromptMode {
    use xai_grok_tools::types::SessionMode;
    match SessionMode::from_id(session_mode_id.0.as_ref()) {
        SessionMode::Plan => PromptMode::Plan,
        SessionMode::Ask => PromptMode::Ask,
        SessionMode::Default => PromptMode::Agent,
    }
}
/// Inverse of [`prompt_mode_from_session_mode_id`]: the mode id a client
/// displays for a prompt mode. Needed wherever a transition the client did not
/// drive has to be reported back to it.
pub(super) fn session_mode_id_from_prompt_mode(prompt_mode: PromptMode) -> acp::SessionModeId {
    use xai_grok_tools::types::SessionMode;
    let mode = match prompt_mode {
        PromptMode::Plan => SessionMode::Plan,
        PromptMode::Ask => SessionMode::Ask,
        PromptMode::Agent => SessionMode::Default,
    };
    acp::SessionModeId::new(mode.as_id())
}
/// Pass-through twin: no toolset in this build carries a plan-gated tool.
pub(super) fn filter_cursor_tools_by_plan_mode(
    defs: Vec<ToolDefinition>,
    _plan_active: bool,
) -> Vec<ToolDefinition> {
    defs
}
impl SessionActor {
    pub(super) fn apply_prompt_modes_to_snapshot(&self, snapshot: &mut TurnDeltaSnapshot) {
        snapshot.start_prompt_mode = Some(self.turn_start_prompt_mode.lock().to_string());
        snapshot.end_prompt_mode = Some(self.turn_prompt_mode.lock().to_string());
    }
    /// `false` twin: this template integration is not compiled into this
    /// build, so no session runs it. Keeps ungated call sites compiling in
    /// both configurations.
    pub(super) fn is_cursor_harness(&self) -> bool {
        false
    }
    pub(super) async fn handle_session_mode(&self, session_mode_id: acp::SessionModeId) {
        use xai_grok_tools::types::SessionMode;
        let prompt_mode = prompt_mode_from_session_mode_id(&session_mode_id);
        *self.current_prompt_mode.lock() = prompt_mode;
        let mode = SessionMode::from_id(session_mode_id.0.as_ref());
        if mode.is_plan() {
            let entered = self.plan_mode.lock().enter_pending();
            if entered {
                self.persist_plan_mode_state();
                self.enqueue_current_mode_update(acp::SessionModeId::new(
                    SessionMode::Plan.as_id(),
                ));
            }
            tracing::info!(
                session_id = %self.session_info.id.0,
                entered,
                "Plan mode toggled ON (Pending)"
            );
            let turn_in_flight = self.state.lock().await.running_task.is_some();
            if entered && turn_in_flight {
                self.activate_plan_mode_mid_turn().await;
            }
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::PlanModeToggled {
                    enabled: true,
                    trigger: xai_grok_telemetry::events::PlanModeTrigger::User,
                    turn_in_flight,
                    was_previously_active: !entered,
                },
            );
            if entered {
                tracing::info_span!(
                    "session.permission_mode_changed",
                    from_mode =
                        super::telemetry::permission_mode_label(self.permissions.is_yolo_mode()),
                    to_mode = "plan",
                    trigger = "user",
                    enabled = true,
                )
                .in_scope(|| {});
            }
            return;
        }
        let was_plan = {
            let tracker = self.plan_mode.lock();
            tracker.state() != crate::session::plan_mode::PlanModeState::Inactive
        };
        if was_plan {
            let turn_in_flight = self.state.lock().await.running_task.is_some();
            self.plan_mode.lock().user_exit(turn_in_flight);
            self.persist_plan_mode_state();
            self.enqueue_current_mode_update(session_mode_id.clone());
            tracing::info!(
                session_id = %self.session_info.id.0,
                new_mode = %session_mode_id.0,
                turn_in_flight,
                "Plan mode toggled OFF"
            );
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::PlanModeToggled {
                    enabled: false,
                    trigger: xai_grok_telemetry::events::PlanModeTrigger::User,
                    turn_in_flight,
                    was_previously_active: true,
                },
            );
            tracing::info_span!(
                "session.permission_mode_changed",
                from_mode = "plan",
                to_mode = %session_mode_id.0,
                trigger = "user",
                enabled = false,
            )
            .in_scope(|| {});
        }
        let agent_def = match session_mode_id.0.as_ref() {
            "browser_use" => Some(AgentDefinition::browser_use()),
            name => {
                let cwd = self.tool_context.cwd.as_path();
                xai_grok_agent::discovery::by_name_in_cwd(name, cwd)
            }
        };
        if let Some(ref def) = agent_def {
            tracing::info!(
                session_id = %self.session_info.id.0,
                agent_name = %def.name,
                agent_scope = %def.scope,
                prompt_mode = ?def.prompt_mode,
                has_completion_req = def.completion_requirement.is_some(),
                tool_configs = def.tool_config.tools.len(),
                "Resolved AgentDefinition for session mode"
            );
            self.agent
                .borrow()
                .update_policies_from_definition(def)
                .await;
            *self.active_agent_type.lock() = Some(def.name.clone());
        }
        if let Some(ref def) = agent_def {
            let new_prompt = self.agent.borrow().render_prompt_for_definition(def).await;
            let mut conversation = self.chat_state_handle.get_conversation().await;
            for item in conversation.iter_mut() {
                if let ConversationItem::System(sys) = item {
                    sys.content = std::sync::Arc::<str>::from(new_prompt);
                    break;
                }
            }
            self.chat_state_handle.replace_conversation(conversation);
        }
    }
    /// Settle the mode a turn runs in, applying the prompt's declaration when
    /// it made one.
    ///
    /// Only a real user turn declares a mode. A synthetic turn — a background
    /// task wake, a goal summary, a notification drain — is constructed
    /// internally with a placeholder `PromptMode::Agent` that reads as "the
    /// user asked for agent mode", so reconciling one ends plan mode just by
    /// waking the session: a background task finishing while you were planning
    /// was enough to do it. Those turns inherit the session's mode instead.
    ///
    /// Returns the resolved mode rather than echoing the argument, so a
    /// synthetic turn is also *recorded* under the mode it really ran in.
    pub(super) fn resolve_turn_prompt_mode(
        &self,
        origin: &crate::session::PromptOrigin,
        declared: PromptMode,
    ) -> PromptMode {
        if !origin.is_synthetic() {
            self.reconcile_plan_mode_with_prompt(declared);
        }
        *self.current_prompt_mode.lock()
    }
    /// Bring the plan-mode tracker into agreement with the prompt's mode.
    ///
    /// Mirrors `handle_session_mode` but driven from `_meta.mode` on the
    /// prompt — the only signal the client sends. Both transitions are
    /// idempotent, so `set_mode`-driven flows are unaffected.
    ///
    /// Like `handle_session_mode`, a real transition here emits a
    /// `CurrentModeUpdate`. Without it a client that carries its mode on the
    /// prompt could enter or leave plan mode with no signal at all — and since
    /// the same line is what lands in `updates.jsonl`, a later replay could not
    /// recover the mode either.
    pub(super) fn reconcile_plan_mode_with_prompt(&self, prompt_mode: PromptMode) {
        use crate::session::plan_mode::PlanModeState;
        *self.current_prompt_mode.lock() = prompt_mode;
        match prompt_mode {
            PromptMode::Plan => {
                let entered = self.plan_mode.lock().enter_pending();
                if entered {
                    self.persist_plan_mode_state();
                    self.enqueue_current_mode_update(session_mode_id_from_prompt_mode(prompt_mode));
                }
            }
            PromptMode::Agent | PromptMode::Ask => {
                let was_plan = {
                    let tracker = self.plan_mode.lock();
                    tracker.state() != PlanModeState::Inactive
                };
                if was_plan {
                    self.plan_mode.lock().user_exit(false);
                    self.persist_plan_mode_state();
                    self.enqueue_current_mode_update(session_mode_id_from_prompt_mode(prompt_mode));
                }
            }
        }
    }
    /// Inject plan mode system-reminders into the conversation.
    ///
    /// Called once per turn from `handle_prompt()`, before the user's actual
    /// message is pushed. Handles three mutually-ordered cases:
    ///
    /// 1. **Pending → Active**: First prompt after user toggled plan mode on.
    ///    Injects the full (or reentry) reminder and transitions to Active.
    /// 2. **Already Active**: Subsequent prompts while plan mode is on.
    ///    Injects an alternating full/sparse per-turn reminder.
    /// 3. **Exit reminder**: One-shot reminder after plan mode was exited.
    ///    Injected once, then the flag is cleared.
    ///
    /// All reminders are pushed as `<system-reminder>`-wrapped user messages
    /// so the model sees them in the same turn as the user's prompt.
    /// Tool names are resolved at render time via `TemplateRenderer`.
    pub(super) async fn inject_plan_mode_reminders(&self) {
        use crate::session::plan_mode::{
            PlanModeState, plan_mode_exit_reminder_template, plan_mode_reminder_full_template,
            plan_mode_reminder_sparse_template,
        };
        let use_cursor_reminders = self.is_cursor_harness();
        let push_reminder = |this: &Self, content: &str| {
            this.push_system_reminder_with_tag(content, this.reminder_wrapper_tag());
        };
        let mut injected_this_turn = false;
        let activation = {
            let tracker = self.plan_mode.lock();
            (tracker.state() == PlanModeState::Pending)
                .then(|| (tracker.is_reentry(), tracker.plan_file_path().to_path_buf()))
        };
        if let Some((is_reentry, plan_path)) = activation {
            self.plan_mode.lock().activate();
            self.persist_plan_mode_state();
            let plan_has_content =
                crate::session::plan_mode::plan_file_has_content(&plan_path).await;
            let template = self.plan_activation_template(is_reentry);
            if let Some(rendered) = self
                .render_plan_template(template, &plan_path, plan_has_content)
                .await
            {
                push_reminder(self, &rendered);
                injected_this_turn = true;
                self.plan_mode.lock().record_reminder_injected();
                self.persist_plan_mode_state();
                tracing::info!(
                    session_id = %self.session_info.id.0,
                    is_reentry,
                    uses_template_reminders = use_cursor_reminders,
                    "Plan mode activated: injected system-reminder"
                );
            }
        }
        if !injected_this_turn {
            let per_turn = {
                let tracker = self.plan_mode.lock();
                tracker.is_active().then(|| {
                    (
                        tracker.should_use_full_reminder(),
                        tracker.plan_file_path().to_path_buf(),
                    )
                })
            };
            if let Some((use_full, plan_path)) = per_turn {
                let plan_has_content =
                    crate::session::plan_mode::plan_file_has_content(&plan_path).await;
                let template = if use_full {
                    plan_mode_reminder_full_template()
                } else {
                    plan_mode_reminder_sparse_template()
                };
                if let Some(rendered) = self
                    .render_plan_template(template, &plan_path, plan_has_content)
                    .await
                {
                    push_reminder(self, &rendered);
                    self.plan_mode.lock().record_reminder_injected();
                    self.persist_plan_mode_state();
                }
            }
        }
        if self.plan_mode.lock().has_pending_exit_reminder() {
            let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
            let template = plan_mode_exit_reminder_template();
            if let Some(rendered) = self.render_plan_template(template, &plan_path, false).await {
                push_reminder(self, &rendered);
            }
            self.plan_mode.lock().clear_pending_exit_reminder();
            self.persist_plan_mode_state();
        }
    }
    /// Activate plan mode for a turn that is already running.
    ///
    /// Mid-turn counterpart of `inject_plan_mode_reminders` case 1: the user
    /// toggled plan mode ON (Shift+Tab) while the model was thinking, so the
    /// tracker sits in `Pending` and the running turn would otherwise proceed
    /// without any plan-mode instruction. Activate immediately (so
    /// `is_active()` tool gating applies to subsequent calls) and buffer the
    /// activation reminder on the tracker; `flush_pending_skill_reminders`
    /// delivers it at the running turn's next safe point (loop top / after
    /// each tool batch) — or, if the turn ends first, the cancel/idle flush
    /// lands it for the next turn. Buffering (vs a direct conversation push)
    /// keeps the in-flight batch's tool_result blocks adjacent, and lets a
    /// toggle-off withdraw an undelivered reminder (`user_exit`).
    ///
    /// No-op unless the tracker is `Pending`: `enter_pending`'s
    /// `ExitPending → Active` re-entry needs no reminder (the model already
    /// has plan-mode context and no exit reminder was injected yet).
    ///
    /// A failed template render still activates (without a buffer), keeping
    /// gating in lockstep with the turn-start path.
    pub(super) async fn activate_plan_mode_mid_turn(&self) {
        use crate::session::plan_mode::PlanModeState;
        let activation = {
            let tracker = self.plan_mode.lock();
            (tracker.state() == PlanModeState::Pending)
                .then(|| (tracker.is_reentry(), tracker.plan_file_path().to_path_buf()))
        };
        let Some((is_reentry, plan_path)) = activation else {
            return;
        };
        let plan_has_content = crate::session::plan_mode::plan_file_has_content(&plan_path).await;
        let template = self.plan_activation_template(is_reentry);
        let rendered = self
            .render_plan_template(template, &plan_path, plan_has_content)
            .await;
        let tag = self.reminder_wrapper_tag();
        let buffered = rendered.is_some();
        let activated = match rendered {
            Some(rendered) => self
                .plan_mode
                .lock()
                .activate_mid_turn(format!("<{tag}>\n{rendered}\n</{tag}>")),
            None => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    "Mid-turn plan activation: reminder render failed; \
                     activating without a buffered reminder"
                );
                self.plan_mode.lock().activate()
            }
        };
        if !activated {
            return;
        }
        self.persist_plan_mode_state();
        tracing::info!(
            session_id = %self.session_info.id.0,
            is_reentry,
            buffered,
            "Plan mode activated mid-turn"
        );
    }
    /// The activation reminder template for the active template (no
    /// first-entry/reentry distinction), or grok's reentry/full variant.
    /// Shared by turn-start injection (`inject_plan_mode_reminders` case 1)
    /// and the mid-turn toggle (`activate_plan_mode_mid_turn`).
    fn plan_activation_template(&self, is_reentry: bool) -> &'static str {
        use crate::session::plan_mode::{
            plan_mode_reentry_reminder_template, plan_mode_reminder_full_template,
        };
        if is_reentry {
            plan_mode_reentry_reminder_template()
        } else {
            plan_mode_reminder_full_template()
        }
    }
    /// Render a plan mode template via the tool bridge's `TemplateRenderer`.
    ///
    /// Passes `plan_path` and `plan_has_content` as extra context alongside the
    /// registry's `tools.by_kind.*` mappings.
    pub(super) async fn render_plan_template(
        &self,
        template: &str,
        plan_path: &std::path::Path,
        plan_has_content: bool,
    ) -> Option<String> {
        let extra = serde_json::json!({
            "plan_path": plan_path.display().to_string(),
            "plan_has_content": plan_has_content,
        });
        self.agent
            .borrow()
            .tool_bridge()
            .render_prompt(template, &extra)
            .await
    }
    /// Persist the current plan mode state to disk.
    ///
    /// Called after every state transition so plan mode survives
    /// session reload/resume/reconnect.
    pub(super) fn persist_plan_mode_state(&self) {
        let snapshot = self.plan_mode.lock().snapshot();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::PlanModeState(snapshot));
    }
}
// ---------------------------------------------------------------------------
// Code Mode session integration
// ---------------------------------------------------------------------------
impl SessionActor {
    /// Cached effective Code Mode plan for the current model.
    pub(crate) fn code_mode_plan(&self) -> crate::tools::code_mode::CodeModeTurnPlan {
        self.tool_context.code_mode.plan()
    }
    /// Recompute the effective Code Mode plan for the current model and bring
    /// the session's runtime/tool registrations in line with it.
    ///
    /// Runs at every turn start, after the tool definitions are prepared:
    /// - Mode comes from the merged account-scoped catalog (`tool_mode`,
    ///   absent ⇒ Classic — fail closed) and the transport from the model's
    ///   provider profile.
    /// - Entering code mode lazily creates the jitless V8 runtime, registers
    ///   `exec`/`wait` on the toolset, mounts the [`CodeModeHandle`] resource,
    ///   and refreshes the nested-tool projection.
    /// - Leaving code mode (or staying Classic) tears the runtime down and
    ///   unregisters the tools.
    ///
    /// Returns `Err` when the catalog declares a code mode but the runtime
    /// cannot initialize; the caller must fail the turn (never silently fall
    /// back to the classic tool list).
    ///
    /// [`CodeModeHandle`]: xai_grok_tools::implementations::code_mode::CodeModeHandle
    pub(crate) async fn refresh_code_mode_for_turn(
        self: &Arc<Self>,
        defs: &[crate::sampling::types::ToolDefinition],
    ) -> Result<(), String> {
        use crate::tools::code_mode::CodeModeTurnPlan;
        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let mode = crate::agent::config::model_tool_mode(
            &self.models_manager.models(),
            &model_id,
        );
        if !mode.is_code_mode() {
            let previously_active = self.tool_context.code_mode.code_mode_active()
                || self.tool_context.code_mode.handle().is_some();
            self.tool_context.code_mode.set_plan(CodeModeTurnPlan {
                model_id,
                mode,
                ..CodeModeTurnPlan::default()
            });
            if previously_active {
                self.tool_context
                    .code_mode
                    .shutdown_detached("model is classic mode");
                self.agent
                    .borrow()
                    .tool_bridge()
                    .toolset()
                    .unregister_code_mode_tools();
            }
            return Ok(());
        }
        let provider = self.model_auth_facts(&model_id).model_provider;
        let transport =
            xai_grok_sampling_types::ProviderProfile::for_provider(provider).code_mode_transport;
        let raw_defs = Self::code_mode_nested_projection(defs);
        let exec_description = xai_grok_code_mode_protocol::build_exec_tool_description(
            &raw_defs,
            /*deferred_tools*/ &[],
            &std::collections::BTreeMap::new(),
            /*code_mode_only*/ mode == crate::agent::config::ToolMode::CodeModeOnly,
        );
        match self.tool_context.code_mode.ensure_runtime() {
            Ok((handle, bridge_rx)) => {
                let enabled_for_runtime = raw_defs
                    .into_iter()
                    .map(xai_grok_code_mode_protocol::augment_tool_definition)
                    .collect();
                self.tool_context
                    .code_mode
                    .update_enabled_tools(enabled_for_runtime);
                let bridge = self.agent.borrow().tool_bridge().clone();
                bridge.toolset().register_code_mode_tools();
                bridge.update_resource(handle).await;
                if let Some((rx, generation)) = bridge_rx {
                    self.spawn_code_mode_bridge_consumer(rx, generation);
                }
                self.tool_context.code_mode.set_plan(CodeModeTurnPlan {
                    model_id,
                    mode,
                    transport: Some(transport),
                    exec_description,
                    init_error: None,
                });
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    model_id = %model_id,
                    error = %error,
                    "code mode runtime initialization failed; failing turn closed"
                );
                self.tool_context.code_mode.set_plan(CodeModeTurnPlan {
                    model_id,
                    mode,
                    transport: Some(transport),
                    exec_description: String::new(),
                    init_error: Some(error.clone()),
                });
                Err(format!(
                    "This model requires Code Mode, but the Code Mode runtime failed to \
                     initialize: {error}"
                ))
            }
        }
    }
    /// Project the registry tool definitions into the code-mode nested-tool
    /// namespace, excluding `exec`/`wait` themselves.
    ///
    /// `output_schema` is deliberately `None` for every tool: nothing upstream
    /// of this projection carries one. The sampling `ToolDefinition` /
    /// `FunctionTool` the registry finalizes has `name`/`description`/
    /// `parameters` only, `ToolMetadata` declares no output schema, and the
    /// MCP client layer does not retain the servers' advertised
    /// `outputSchema`. Publishing a guessed schema would be worse than none —
    /// `build_exec_tool_description` renders it into the TypeScript
    /// declarations the model codes against. Give it a real source (thread
    /// `outputSchema` through MCP registration, or add one to `ToolMetadata`)
    /// and this is the single place to project it.
    fn code_mode_nested_projection(
        defs: &[crate::sampling::types::ToolDefinition],
    ) -> Vec<xai_grok_code_mode_protocol::ToolDefinition> {
        defs.iter()
            .filter(|d| {
                xai_grok_code_mode_protocol::is_code_mode_nested_tool(&d.function.name)
            })
            .map(|d| xai_grok_code_mode_protocol::ToolDefinition {
                name: d.function.name.clone(),
                tool_name: xai_grok_code_mode_protocol::ToolName::plain(
                    d.function.name.clone(),
                ),
                description: d.function.description.clone().unwrap_or_default(),
                kind: xai_grok_code_mode_protocol::CodeModeToolKind::Function,
                input_schema: Some(d.function.parameters.clone()),
                output_schema: None,
            })
            .collect()
    }
    /// Consume nested calls / notifications bridged from cell tasks; each
    /// nested call runs on the actor's `LocalSet` so it can re-enter the
    /// hook, permission, and plan-mode gates.
    ///
    /// Every message is fenced against the runtime `generation` this consumer
    /// was spawned for: after `shutdown_detached` bumps the generation (model
    /// switch / close / rewind), queued work is rejected synchronously here —
    /// a write queued under the previous runtime can never dispatch.
    fn spawn_code_mode_bridge_consumer(
        self: &Arc<Self>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::tools::code_mode::CodeModeBridgeMsg>,
        generation: u64,
    ) {
        use crate::tools::code_mode::CodeModeBridgeMsg;
        let weak = Arc::downgrade(self);
        tokio::task::spawn_local(async move {
            while let Some(msg) = rx.recv().await {
                let Some(session) = weak.upgrade() else { break };
                let stale =
                    session.tool_context.code_mode.current_generation() != generation;
                match msg {
                    CodeModeBridgeMsg::NestedCall {
                        call,
                        cancellation_token,
                        respond_to,
                    } => {
                        if stale {
                            let _ = respond_to.send(Err(
                                "code mode runtime was shut down; nested call rejected"
                                    .to_string(),
                            ));
                            continue;
                        }
                        tokio::task::spawn_local(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancellation_token.cancelled() => {
                                    Err("nested tool call cancelled".to_string())
                                }
                                // The generation travels with the call: this
                                // task can park for a long time in a hook,
                                // permission prompt, or path lock, so the
                                // dequeue-time check above is not the last
                                // word — `run_code_mode_nested_call` re-checks
                                // it at the dispatch boundary.
                                result = session.run_code_mode_nested_call(&call, generation) => {
                                    result
                                }
                            };
                            let _ = respond_to.send(result);
                        });
                    }
                    CodeModeBridgeMsg::Notify {
                        tool_call_id,
                        cell_id,
                        text,
                        respond_to,
                    } => {
                        if stale {
                            let _ = respond_to.send(Ok(()));
                            continue;
                        }
                        session
                            .handle_code_mode_notify(tool_call_id, cell_id, text)
                            .await;
                        let _ = respond_to.send(Ok(()));
                    }
                }
            }
        });
    }
    /// One nested tool call from a running cell. Re-enters the PreToolUse
    /// hooks (deny + rewritten-input reparse), the plan-mode write gate, and
    /// the permission gate exactly like a model-issued call, serializes
    /// same-path writes on the shared per-path lock, and fires the
    /// PostToolUse / PostToolUseFailure hooks and telemetry afterwards.
    ///
    /// Contract with the cell: logical failures (`ToolOutput::is_error`) and
    /// every rejection reject the JS promise with a clean error string;
    /// successes resolve with the tool's structured value where it has one
    /// (MCP `CallToolResult` shape, image content) and a plain string for
    /// purely textual outputs.
    ///
    /// The client-side (reverse-request) PreToolUse gate is consulted too
    /// (finding 3): the decision is taken through
    /// [`Self::pre_tool_use_client_denial`], which has no session side
    /// effects, so the deny reaches the cell as a rejected promise without the
    /// paired top-level `tool_result` a nested call must never write.
    ///
    /// `generation` is the Code Mode runtime generation this call was admitted
    /// under. It is re-checked immediately before `dispatch_tool` (finding 4):
    /// everything between admission and dispatch — hooks, the permission
    /// prompt, the per-path lock — can park for an unbounded time, and a model
    /// switch, cancel, or rewind in that window must fence the call.
    ///
    /// The one deliberate deviation from the batch path: session-lifecycle
    /// tools (plan mode enter/exit) are rejected outright rather than
    /// intercepted, since their approval dialogs require top-level handling.
    async fn run_code_mode_nested_call(
        self: &Arc<Self>,
        call: &xai_grok_code_mode_protocol::CodeModeNestedToolCall,
        generation: u64,
    ) -> Result<serde_json::Value, String> {
        let wire_name = call.tool_name.to_string();
        if !xai_grok_code_mode_protocol::is_code_mode_nested_tool(&wire_name) {
            return Err(format!("tool `{wire_name}` cannot be called from inside exec"));
        }
        let mut input_value = call
            .input
            .clone()
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        let mut tool_input = self
            .tool_bridge_handle()
            .try_parse(&wire_name, input_value.clone())
            .await
            .map_err(|e| format!("invalid input for `{wire_name}`: {e}"))?;
        Self::reject_lifecycle_nested_tool(&wire_name, &tool_input)?;
        let ui_call_id = format!("codemode-{}-{}", call.cell_id, call.runtime_tool_call_id);
        // PreToolUse hooks — same registry dispatcher and rewrite semantics
        // as prepare_tool_call.
        let mut resolved_tool_name = tool_input
            .dispatch_target_name()
            .unwrap_or_else(|| wire_name.clone());
        if self.may_have_hooks_for(xai_grok_hooks::event::HookEventName::PreToolUse) {
            let mut envelope =
                self.make_pre_tool_use_envelope(&resolved_tool_name, &ui_call_id, &input_value);
            let hook_registry_snapshot = self.hook_registry.borrow().clone();
            if let Some(registry) = hook_registry_snapshot {
                let ctx = self.hook_run_ctx();
                let pre_result =
                    xai_grok_hooks::dispatcher::dispatch_pre_tool_use(&registry, &envelope, &ctx)
                        .await;
                self.send_hook_execution(
                    "pre_tool_use",
                    Some(&resolved_tool_name),
                    None,
                    &pre_result.results,
                )
                .await;
                self.emit_hook_executed_telemetry(
                    "pre_tool_use",
                    Some(&resolved_tool_name),
                    &pre_result.results,
                )
                .await;
                if let xai_grok_hooks::result::HookDecision::Deny { reason, hook_name } =
                    pre_result.decision
                {
                    return Err(format!(
                        "Tool `{resolved_tool_name}` was denied by hook `{hook_name}`: {reason}"
                    ));
                }
                if let Some(rewrite) = pre_result.updated_input {
                    let updated = rewrite.input;
                    tool_input = self
                        .tool_bridge_handle()
                        .try_parse(&wire_name, updated.clone())
                        .await
                        .map_err(|e| {
                            format!(
                                "PreToolUse hook '{}' returned an invalid updatedInput: {e}",
                                rewrite.hook_name
                            )
                        })?;
                    Self::reject_lifecycle_nested_tool(&wire_name, &tool_input)?;
                    input_value = updated;
                    resolved_tool_name = tool_input
                        .dispatch_target_name()
                        .unwrap_or_else(|| wire_name.clone());
                    // Rebuild so the client gate below sees the rewritten
                    // input and resolved name, exactly as prepare_tool_call
                    // does.
                    envelope = self.make_pre_tool_use_envelope(
                        &resolved_tool_name,
                        &ui_call_id,
                        &input_value,
                    );
                }
            }
            // Client-side (reverse-request) PreToolUse gate — same decision the
            // top-level path takes, minus its conversation side effects
            // (finding 3). A nested deny rejects the cell's promise; it must
            // never push a paired tool_result.
            // Rejected before the ACP tool call is announced, exactly like the
            // registry-hook deny above, so no orphan ToolCallUpdate is sent.
            if let Some(denial) = self.pre_tool_use_client_denial(&wire_name, &envelope).await {
                return Err(format!(
                    "Tool `{}` was denied by hook `{}`: {}",
                    denial.tool_name, denial.hook_name, denial.reason
                ));
            }
        }
        let access_kind = AccessKind::from(&tool_input);
        // Plan-mode write gate (same funnel as prepare_tool_call).
        let plan_gate = plan_mode_edit_gate(&self.plan_mode.lock(), &tool_input, &access_kind);
        if plan_gate != PlanEditGate::Allow {
            return Err(self.plan_mode_edit_rejected_message().await);
        }
        // Register the nested call as a real ACP tool call so the permission
        // prompt has an anchor and the client renders it.
        let ui_id = acp::ToolCallId::new(Arc::from(ui_call_id.clone()));
        let marker = serde_json::json!({"codeModeCellId": call.cell_id.as_str()})
            .as_object()
            .cloned();
        let meta = self.stamp_tool_meta(marker, &wire_name, Some(&tool_input));
        let raw_input = serde_json::to_value(&tool_input).ok();
        self.send_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(ui_id.clone(), wire_name.clone())
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Pending)
                    .raw_input(raw_input.clone())
                    .meta(meta),
            ),
            None,
        )
        .await;
        // Permission gate (same resolver as prepare_tool_call).
        let tool_call_update = acp::ToolCallUpdate::new(
            ui_id.clone(),
            acp::ToolCallUpdateFields::new()
                .title(Some(wire_name.clone()))
                .raw_input(raw_input),
        );
        let path_context = Some(xai_grok_workspace::permission::types::RequestPathContext {
            real_cwd: std::path::PathBuf::from(self.session_info.cwd.as_str()),
            display_cwd: self
                .display_cwd
                .get()
                .map(|cwd| std::path::PathBuf::from(cwd.as_str())),
        });
        let resolution = self
            .permissions
            .request_with_path_context_resolved(
                access_kind.clone(),
                tool_call_update,
                path_context,
                Some(self.session_info.id.0.to_string()),
                None,
                None,
            )
            .await;
        let denial = match resolution.decision {
            Decision::Allow | Decision::Ask => None,
            Decision::PolicyDeny(ref reason) | Decision::Reject(ref reason) => Some(format!(
                "Tool `{wire_name}` was not executed: {reason}"
            )),
            Decision::Cancelled => {
                Some(format!("User cancelled the execution for tool `{wire_name}`"))
            }
            Decision::FollowupMessage(_) => Some(format!(
                "The user declined to run tool `{wire_name}` from exec"
            )),
        };
        if let Some(message) = denial {
            self.finish_code_mode_nested_ui(&ui_id, false, &message).await;
            return Err(message);
        }
        let is_read_only = matches!(
            access_kind,
            AccessKind::Read(_) | AccessKind::Grep { .. }
        );
        let prepared = PreparedToolCall {
            call_id: ui_id.0.to_string(),
            tool_call_id: ui_id.clone(),
            tool_name: wire_name.clone(),
            raw_arguments: input_value.to_string(),
            parsed_args: input_value.clone(),
            model_id: String::new(),
            concatenated_json_count: 0,
            dispatch_target_name: tool_input.dispatch_target_name(),
            is_read_only,
        };
        // Same-path write serialization (Promise.all parity with the batch
        // path's per-file mutexes).
        let path_lock = if is_read_only {
            None
        } else {
            lock_path_for_args(&input_value)
                .map(|path| self.tool_context.code_mode.nested_path_lock(path))
        };
        let session_id: Arc<str> = Arc::from(&*self.session_info.id.0);
        let result = {
            let _path_guard = match path_lock.as_ref() {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            // Last fence before the effect happens (finding 4). Everything
            // above — hooks, the client gate, the permission prompt, this very
            // path lock — can park indefinitely; if the runtime was
            // invalidated while we waited, the call belongs to a runtime (and
            // possibly a history) that no longer exists and must not land.
            // Taken while holding the path guard so it cannot go stale between
            // the check and the dispatch.
            if self.tool_context.code_mode.current_generation() != generation {
                None
            } else {
                self.signals_handle().record_tool_call(&wire_name);
                Some(
                    call_with_auth_retry(
                        self.auth_manager.as_ref(),
                        None,
                        &wire_name,
                        || async {
                            dispatch_tool(&self.workspace_ops, &prepared, session_id.as_ref())
                                .await
                        },
                    )
                    .await,
                )
            }
        };
        let Some(result) = result else {
            let message = format!(
                "Tool `{wire_name}` was not executed: the code mode runtime was invalidated \
                 (model switch, cancel, or rewind) while the call was waiting"
            );
            tracing::info!(
                session_id = %self.session_info.id.0,
                tool_name = %wire_name,
                admitted_generation = generation,
                current_generation = self.tool_context.code_mode.current_generation(),
                "nested code mode call fenced at the dispatch boundary"
            );
            self.finish_code_mode_nested_ui(&ui_id, false, &message).await;
            return Err(message);
        };
        match result {
            Ok(run_result) => {
                let failed = run_result.output.is_error();
                self.finish_code_mode_nested_ui(&ui_id, !failed, &run_result.prompt_text)
                    .await;
                if failed {
                    self.signals_handle().record_tool_failure(&wire_name);
                } else {
                    self.signals_handle().record_tool_success(&wire_name);
                }
                // PostToolUse hooks (same payload shape as the batch path).
                if self.may_have_hooks_for(xai_grok_hooks::event::HookEventName::PostToolUse) {
                    let tool_result_value = serde_json::to_value(&run_result.output)
                        .unwrap_or(serde_json::Value::Null);
                    let (tool_input_value, tool_input_truncated) =
                        xai_grok_hooks::event::truncate_payload(input_value.clone());
                    let (tool_result_val, tool_result_truncated) =
                        xai_grok_hooks::event::truncate_payload(tool_result_value);
                    self.dispatch_hook(
                        xai_grok_hooks::event::HookEventName::PostToolUse,
                        xai_grok_hooks::event::HookPayload::PostToolUse {
                            tool_name: resolved_tool_name.clone(),
                            tool_use_id: ui_call_id.clone(),
                            tool_input: tool_input_value,
                            tool_result: tool_result_val,
                            tool_input_truncated,
                            tool_result_truncated,
                            duration_ms: None,
                            is_backgrounded: false,
                            subagent_type: self.subagent_type_label(),
                        },
                        None,
                        Some(&resolved_tool_name),
                    )
                    .await;
                }
                // Contract with the cell: a logical failure rejects the JS
                // promise (finding 4); a success resolves with the tool's
                // structured value where it has one (finding 5).
                if failed {
                    return Err(run_result.prompt_text);
                }
                Ok(Self::nested_result_value(&run_result))
            }
            Err(error) => {
                let message = format!("Tool `{wire_name}` failed: {error}");
                self.signals_handle().record_tool_failure(&wire_name);
                self.finish_code_mode_nested_ui(&ui_id, false, &message).await;
                if self
                    .may_have_hooks_for(xai_grok_hooks::event::HookEventName::PostToolUseFailure)
                {
                    let (tool_input_value, tool_input_truncated) =
                        xai_grok_hooks::event::truncate_payload(input_value.clone());
                    self.dispatch_hook(
                        xai_grok_hooks::event::HookEventName::PostToolUseFailure,
                        xai_grok_hooks::event::HookPayload::PostToolUseFailure {
                            tool_name: resolved_tool_name.clone(),
                            tool_use_id: ui_call_id.clone(),
                            tool_input: tool_input_value,
                            tool_input_truncated,
                            error: message.clone(),
                            subagent_type: self.subagent_type_label(),
                        },
                        None,
                        Some(&resolved_tool_name),
                    )
                    .await;
                }
                Err(message)
            }
        }
    }
    /// Session-lifecycle tools that require top-level interception (plan
    /// approval dialogs) must not be reachable from inside `exec`.
    fn reject_lifecycle_nested_tool(
        wire_name: &str,
        tool_input: &ToolInput,
    ) -> Result<(), String> {
        if matches!(
            tool_input,
            ToolInput::ExitPlanMode(_) | ToolInput::EnterPlanMode(_)
        ) {
            return Err(format!(
                "tool `{wire_name}` manages the session lifecycle and must be called as a \
                 top-level tool, not from inside exec"
            ));
        }
        Ok(())
    }
    /// The JSON value a nested call resolves with: preserve the structured
    /// shape where the tool has one, use a plain string only for genuinely
    /// textual outputs.
    ///
    /// - MCP tools resolve with an MCP `CallToolResult`-shaped object
    ///   (`{content: [...], isError, _meta}`), matching the
    ///   `result.content[0]` contract in the exec description. Any images the
    ///   tool layer captured out of the server's reply become `image` content
    ///   blocks alongside the text block instead of being dropped, so
    ///   `image(result.content[1])` forwarding works for MCP too.
    /// - Image-producing reads resolve with `{content: [{type: "image",
    ///   data, mimeType}]}` so `image(result.content[0])` forwarding works.
    /// - Dynamic (runtime-registered) tools resolve with their JSON value
    ///   verbatim, so JS can read its fields.
    /// - Everything else resolves with the prompt-facing text.
    ///
    /// Not preserved, because this build never carries it: MCP
    /// `structuredContent`, audio, and resource blocks. `MCPOutput` stores a
    /// single already-rendered `OkayOutput(String)`/`Error(String)` (the
    /// runtime's `CallToolResult` renderer folds `structuredContent` into that
    /// text before the tool layer sees it), so there is nothing left to
    /// forward; the transport facts that *are* retained travel in `_meta`.
    fn nested_result_value(run_result: &ToolRunResult) -> serde_json::Value {
        use xai_grok_tools::types::output::MCPOutputDetails;
        match &run_result.output {
            ToolsToolOutput::MCP(mcp) => {
                let text = match mcp.output() {
                    MCPOutputDetails::OkayOutput(text) => text.clone(),
                    MCPOutputDetails::Error(error) => error.clone(),
                };
                let mut content = vec![serde_json::json!({"type": "text", "text": text})];
                // Images the tool layer extracted from the server's reply. The
                // nested path never runs the harness drain
                // (`drain_tool_layer_extracted_images`), so they are still
                // here — forward them rather than discarding them.
                content.extend(mcp.extracted_images.iter().map(|image| {
                    serde_json::json!({
                        "type": "image",
                        "data": image.data,
                        "mimeType": image.mime_type,
                    })
                }));
                serde_json::json!({
                    "content": content,
                    "isError": mcp.is_error,
                    "_meta": {
                        "x.ai/mcp": {
                            "toolName": mcp.tool_name(),
                            "serverName": mcp.server_name(),
                            "isTimeout": mcp.is_timeout,
                            "reconnectAttempted": mcp.reconnect_attempted,
                            "authRetryAttempted": mcp.auth_retry_attempted,
                        }
                    },
                })
            }
            // Runtime-registered tools carry arbitrary JSON; hand it to the
            // cell unchanged instead of flattening it to prompt text.
            ToolsToolOutput::Dynamic(dynamic) => dynamic.value.clone(),
            ToolsToolOutput::ReadFile(ReadFileOutput::ImageContent(image)) => {
                serde_json::json!({
                    "content": [{
                        "type": "image",
                        "data": image.data,
                        "mimeType": image.mime_type,
                    }],
                })
            }
            ToolsToolOutput::ReadFile(ReadFileOutput::PdfPageImages(pdf)) => {
                let pages: Vec<serde_json::Value> = pdf
                    .pages
                    .iter()
                    .map(|page| {
                        serde_json::json!({
                            "type": "image",
                            "data": page.data,
                            "mimeType": page.mime_type,
                        })
                    })
                    .collect();
                serde_json::json!({ "content": pages })
            }
            _ => serde_json::Value::String(run_result.prompt_text.clone()),
        }
    }
    /// Terminal ACP update for a nested call registered by
    /// [`Self::run_code_mode_nested_call`].
    async fn finish_code_mode_nested_ui(
        &self,
        ui_id: &acp::ToolCallId,
        success: bool,
        text: &str,
    ) {
        let status = if success {
            acp::ToolCallStatus::Completed
        } else {
            acp::ToolCallStatus::Failed
        };
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                ui_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .status(Some(status))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(text.to_string())),
                    )])),
            )),
            None,
        )
        .await;
    }
    /// `notify(...)` from a running cell → progress on the owning exec tool
    /// call.
    async fn handle_code_mode_notify(
        &self,
        tool_call_id: String,
        cell_id: xai_grok_code_mode_protocol::CellId,
        text: String,
    ) {
        tracing::debug!(
            session_id = %self.session_info.id.0,
            cell_id = %cell_id,
            "code mode notify"
        );
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from(tool_call_id)),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::InProgress))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    )])),
            )),
            None,
        )
        .await;
    }
    /// Explicitly terminate yielded cells; the abort-based turn cancel only
    /// drops the `exec`/`wait` futures, never the isolates.
    pub(super) fn terminate_code_mode_cells(&self, reason: &'static str) {
        self.tool_context
            .code_mode
            .terminate_live_cells_detached(reason);
    }
}
#[cfg(test)]
mod code_mode_nested_tests {
    use super::*;
    use xai_grok_tools::types::output::{MCPOutput, ToolRunResult};

    fn run_result(output: ToolsToolOutput, prompt_text: &str) -> ToolRunResult {
        ToolRunResult {
            output,
            prompt_text: prompt_text.to_string(),
            effective_tool_name: None,
        }
    }

    /// Session-lifecycle tools are rejected from inside exec (finding 3).
    #[test]
    fn plan_lifecycle_tools_are_rejected_from_exec() {
        use xai_grok_tools::implementations::grok_build::exit_plan_mode::ExitPlanModeInput;
        let exit = ToolInput::ExitPlanMode(ExitPlanModeInput {});
        let err = SessionActor::reject_lifecycle_nested_tool("exit_plan_mode", &exit)
            .expect_err("exit_plan_mode must be rejected");
        assert!(err.contains("top-level tool"), "{err}");
        let read = ToolInput::ReadFile(
            serde_json::from_value(serde_json::json!({"target_file": "/x"})).unwrap(),
        );
        assert!(SessionActor::reject_lifecycle_nested_tool("read_file", &read).is_ok());
    }

    /// Structured nested-result contract (finding 5): MCP results resolve as
    /// CallToolResult-shaped objects, image reads as image content, plain
    /// text as a string.
    #[test]
    fn nested_result_preserves_structured_shapes() {
        let mcp = run_result(
            ToolsToolOutput::MCP(MCPOutput::okay_output(
                "linear__save_issue".to_string(),
                "linear".to_string(),
                "issue saved".to_string(),
            )),
            "issue saved",
        );
        let value = SessionActor::nested_result_value(&mcp);
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "issue saved");
        assert_eq!(value["isError"], false);

        let image = run_result(
            ToolsToolOutput::ReadFile(ReadFileOutput::ImageContent(
                xai_grok_tools::types::output::ImageContent {
                    data: "QUJD".to_string(),
                    mime_type: "image/png".to_string(),
                    annotations: None,
                    uri: None,
                    meta: None,
                },
            )),
            "[Image content...]",
        );
        let value = SessionActor::nested_result_value(&image);
        assert_eq!(value["content"][0]["type"], "image");
        assert_eq!(value["content"][0]["mimeType"], "image/png");
        assert_eq!(value["content"][0]["data"], "QUJD");

        let text = run_result(
            ToolsToolOutput::Text(xai_grok_tools::types::output::TextOutput::from(
                "plain".to_string(),
            )),
            "plain",
        );
        assert_eq!(
            SessionActor::nested_result_value(&text),
            serde_json::Value::String("plain".to_string())
        );
    }

    /// Finding 6: a dynamic (runtime-registered) tool's JSON value reaches the
    /// cell verbatim, so JS can read its fields instead of a rendered string.
    #[test]
    fn dynamic_output_round_trips_its_structured_value() {
        let value = serde_json::json!({
            "id": 42,
            "nested": {"ok": true, "items": ["a", "b"]},
        });
        let dynamic = run_result(
            ToolsToolOutput::Dynamic(xai_grok_tools::types::output::DynamicOutput::from(
                value.clone(),
            )),
            "id=42 (rendered for the prompt)",
        );
        assert_eq!(SessionActor::nested_result_value(&dynamic), value);
    }

    /// Finding 6: a real `MCPOutput` forwards its extracted images as image
    /// content blocks (they used to be dropped), reports `isError`, and keeps
    /// the transport facts it does retain in `_meta`.
    #[test]
    fn mcp_output_forwards_images_error_flag_and_metadata() {
        use xai_grok_tools::util::base64_images::ExtractedImage;

        let mut mcp = MCPOutput::errored(
            "figma__export".to_string(),
            "figma".to_string(),
            "render failed".to_string(),
        );
        mcp.is_timeout = true;
        mcp.reconnect_attempted = true;
        mcp.extracted_images = vec![
            ExtractedImage {
                data: "QUJD".to_string(),
                mime_type: "image/png".to_string(),
            },
            ExtractedImage {
                data: "REVG".to_string(),
                mime_type: "image/jpeg".to_string(),
            },
        ];
        let value = SessionActor::nested_result_value(&run_result(
            ToolsToolOutput::MCP(mcp),
            "render failed",
        ));

        assert_eq!(value["isError"], true);
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "render failed");
        assert_eq!(value["content"][1]["type"], "image");
        assert_eq!(value["content"][1]["data"], "QUJD");
        assert_eq!(value["content"][1]["mimeType"], "image/png");
        assert_eq!(value["content"][2]["mimeType"], "image/jpeg");
        assert_eq!(
            value["content"].as_array().map(Vec::len),
            Some(3),
            "one text block plus every extracted image: {value}"
        );
        let meta = &value["_meta"]["x.ai/mcp"];
        assert_eq!(meta["toolName"], "figma__export");
        assert_eq!(meta["serverName"], "figma");
        assert_eq!(meta["isTimeout"], true);
        assert_eq!(meta["reconnectAttempted"], true);
        assert_eq!(meta["authRetryAttempted"], false);
    }

    /// Finding 6, schema half: the nested projection drops `exec`/`wait` and
    /// carries each tool's real input schema. `output_schema` stays `None`
    /// because nothing upstream carries one — pinned here so a future source
    /// of output schemas has to update this test deliberately rather than
    /// leaving the advertised contract silently false.
    #[test]
    fn nested_projection_carries_input_schema_and_no_output_schema() {
        use crate::sampling::types::ToolDefinition;

        let params = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
        });
        let defs = vec![
            ToolDefinition::function("read_file", Some("read a file"), params.clone()),
            ToolDefinition::function(
                xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME,
                Some("exec"),
                serde_json::json!({}),
            ),
            ToolDefinition::function(
                xai_grok_code_mode_protocol::WAIT_TOOL_NAME,
                Some("wait"),
                serde_json::json!({}),
            ),
        ];
        let projected = SessionActor::code_mode_nested_projection(&defs);
        assert_eq!(projected.len(), 1, "exec/wait must not be nested tools");
        assert_eq!(projected[0].name, "read_file");
        assert_eq!(projected[0].description, "read a file");
        assert_eq!(projected[0].input_schema.as_ref(), Some(&params));
        assert!(
            projected[0].output_schema.is_none(),
            "no registry surface carries an output schema in this build"
        );
    }
}

/// Session-level Code Mode invalidation and gating, driven through the real
/// `run_code_mode_nested_call` path.
#[cfg(test)]
mod code_mode_session_tests {
    use super::*;
    use crate::session::acp_session::support::{create_test_actor, test_grok_build_agent_with_todo};

    fn nested_call(cell: &str) -> xai_grok_code_mode_protocol::CodeModeNestedToolCall {
        xai_grok_code_mode_protocol::CodeModeNestedToolCall {
            cell_id: xai_grok_code_mode_protocol::CellId::new(cell.to_string()),
            runtime_tool_call_id: "t1".to_string(),
            tool_name: xai_grok_code_mode_protocol::ToolName::plain("todo_write"),
            tool_kind: xai_grok_code_mode_protocol::CodeModeToolKind::Function,
            input: Some(serde_json::json!({
                "todos": [{"id": "t1", "content": "do", "status": "completed"}]
            })),
        }
    }

    fn install_client_pre_tool_use_hook(actor: &SessionActor, callback_ids: &[&str]) {
        let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
        client_hooks.insert(
            xai_grok_hooks::event::HookEventName::PreToolUse,
            vec![crate::extensions::hooks::ClientHookGroup {
                matcher: None,
                callback_ids: callback_ids.iter().map(|s| s.to_string()).collect(),
                timeout: None,
            }],
        );
        *actor.client_hooks.borrow_mut() = client_hooks;
    }

    /// An actor that can actually dispatch `todo_write`.
    async fn dispatching_actor(
        gateway_tx: tokio::sync::mpsc::UnboundedSender<xai_acp_lib::AcpClientMessage>,
    ) -> Arc<SessionActor> {
        let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        *actor.agent.borrow_mut() = test_grok_build_agent_with_todo().await;
        actor
            .workspace_ops
            .bind_local_session(
                &actor.session_id_string(),
                actor.tool_context.cwd.as_path().to_path_buf(),
                actor.tool_context.hunk_tracker_handle.clone(),
                actor.agent.borrow().tool_bridge().toolset(),
                None,
            )
            .expect("bind_local_session must succeed");
        Arc::new(actor)
    }

    /// Finding 3: the client's `PreToolUse` gate now covers nested calls too.
    /// A deny rejects the cell's promise — and, unlike the top-level path,
    /// leaves the conversation untouched: a nested call must never write a
    /// paired `tool_result`, which is exactly why the gate used to be skipped.
    #[tokio::test(flavor = "current_thread")]
    async fn nested_call_honors_a_client_pre_tool_use_deny() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = dispatching_actor(gateway_tx).await;
                install_client_pre_tool_use_hook(&actor, &["cb_0"]);

                let seen_input = Arc::new(std::sync::Mutex::new(None));
                let recorder = Arc::clone(&seen_input);
                tokio::task::spawn_local(async move {
                    while let Some(msg) = gateway_rx.recv().await {
                        match msg {
                            xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                                let params: serde_json::Value =
                                    serde_json::from_str(args.request.params.get()).unwrap();
                                *recorder.lock().unwrap() = Some(params["toolInput"].clone());
                                let deny: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "decision": "deny",
                                        "systemMessage": "client policy forbids todo_write",
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                            }
                            xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                                let _ = args.response_tx.send(Ok(()));
                            }
                            _ => {}
                        }
                    }
                });

                let generation = actor.tool_context.code_mode.current_generation();
                let err = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    actor.run_code_mode_nested_call(&nested_call("1"), generation),
                )
                .await
                .expect("the nested gate must not hang")
                .expect_err("a client deny must reject the nested call");

                assert!(err.contains("client:cb_0"), "{err}");
                assert!(err.contains("client policy forbids todo_write"), "{err}");
                assert_eq!(
                    seen_input.lock().unwrap().as_ref().map(|v| v["todos"][0]["id"].clone()),
                    Some(serde_json::json!("t1")),
                    "the client gate must receive the nested call's real input"
                );
                assert!(
                    actor
                        .chat_state_handle
                        .get_conversation()
                        .await
                        .iter()
                        .all(|item| !matches!(item, ConversationItem::ToolResult(_))),
                    "a nested deny must not write a paired tool_result"
                );
            })
            .await;
    }

    /// The same gate answering `continue` lets the nested call through, so the
    /// deny above is the hook's decision and not the plumbing failing closed.
    #[tokio::test(flavor = "current_thread")]
    async fn nested_call_proceeds_when_the_client_gate_allows() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = dispatching_actor(gateway_tx).await;
                install_client_pre_tool_use_hook(&actor, &["cb_0"]);

                tokio::task::spawn_local(async move {
                    while let Some(msg) = gateway_rx.recv().await {
                        match msg {
                            xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                                let ok: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "decision": "continue",
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(ok)));
                            }
                            xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                                let _ = args.response_tx.send(Ok(()));
                            }
                            _ => {}
                        }
                    }
                });

                let generation = actor.tool_context.code_mode.current_generation();
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    actor.run_code_mode_nested_call(&nested_call("1"), generation),
                )
                .await
                .expect("the nested gate must not hang");
                assert!(result.is_ok(), "an allowed nested call must run: {result:?}");
            })
            .await;
    }

    /// A registry `PreToolUse` rewrite is re-parsed and then re-published to
    /// the client gate: the client sees the *rewritten* input, not the one the
    /// cell sent. Without rebuilding the envelope after the rewrite, a client
    /// policy would be deciding on input that no longer exists.
    #[tokio::test(flavor = "current_thread")]
    async fn nested_registry_rewrite_reaches_the_client_gate() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut actor = {
                    let (persistence_tx, _persistence_rx) =
                        tokio::sync::mpsc::unbounded_channel();
                    create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await
                };
                actor.hook_resolved_workspace_root = "/tmp".to_string();
                *actor.agent.borrow_mut() = test_grok_build_agent_with_todo().await;
                *actor.hook_registry.borrow_mut() = Some(Arc::new(
                    crate::session::acp_session::client_hooks_tests::file_registry_with_spec(
                        xai_grok_hooks::event::HookEventName::PreToolUse,
                        "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"todos\":\
                         [{\"id\":\"rewritten\",\"content\":\"by the hook\",\
                         \"status\":\"pending\"}]}}}'",
                    ),
                ));
                let actor = Arc::new(actor);
                install_client_pre_tool_use_hook(&actor, &["cb_0"]);

                let seen_input = Arc::new(std::sync::Mutex::new(None));
                let recorder = Arc::clone(&seen_input);
                tokio::task::spawn_local(async move {
                    while let Some(msg) = gateway_rx.recv().await {
                        match msg {
                            xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                                let params: serde_json::Value =
                                    serde_json::from_str(args.request.params.get()).unwrap();
                                if params["hookEventName"] == "pre_tool_use" {
                                    *recorder.lock().unwrap() = Some(params["toolInput"].clone());
                                }
                                let deny: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "decision": "deny",
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                            }
                            xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                                let _ = args.response_tx.send(Ok(()));
                            }
                            _ => {}
                        }
                    }
                });

                let generation = actor.tool_context.code_mode.current_generation();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    actor.run_code_mode_nested_call(&nested_call("1"), generation),
                )
                .await
                .expect("the nested gate must not hang");

                let seen = seen_input.lock().unwrap().clone();
                assert_eq!(
                    seen.as_ref().map(|v| v["todos"][0]["id"].clone()),
                    Some(serde_json::json!("rewritten")),
                    "the client gate must see the rewritten input, got {seen:?}"
                );
            })
            .await;
    }

    /// Finding 4, the TOCTOU itself: the runtime is invalidated *after* the
    /// call was admitted and *while* it is parked in the client gate. The
    /// dequeue-time check has already passed, so only the re-check at the
    /// dispatch boundary can stop the write.
    ///
    /// The client gate doubles as the barrier: the responder holds its reply
    /// until the test has invalidated the runtime, then answers `continue`.
    #[tokio::test(flavor = "current_thread")]
    async fn nested_call_is_fenced_when_invalidated_while_it_waits() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = dispatching_actor(gateway_tx).await;
                install_client_pre_tool_use_hook(&actor, &["barrier_cb"]);

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                tokio::task::spawn_local(async move {
                    let mut entered_tx = Some(entered_tx);
                    let mut release_rx = Some(release_rx);
                    while let Some(msg) = gateway_rx.recv().await {
                        match msg {
                            xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                                if let Some(tx) = entered_tx.take() {
                                    let _ = tx.send(());
                                }
                                if let Some(rx) = release_rx.take() {
                                    let _ = rx.await;
                                }
                                let ok: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "decision": "continue",
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(ok)));
                            }
                            xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                                let _ = args.response_tx.send(Ok(()));
                            }
                            _ => {}
                        }
                    }
                });

                let generation = actor.tool_context.code_mode.current_generation();
                let call_actor = Arc::clone(&actor);
                let call = tokio::task::spawn_local(async move {
                    call_actor
                        .run_code_mode_nested_call(&nested_call("1"), generation)
                        .await
                });

                // The call is admitted and now parked inside the gate.
                entered_rx.await.expect("the gate must be entered");
                // Model switch / cancel / rewind lands here.
                actor
                    .tool_context
                    .code_mode
                    .shutdown_detached("test invalidation");
                let _ = release_tx.send(());

                let err = tokio::time::timeout(std::time::Duration::from_secs(5), call)
                    .await
                    .expect("the fenced call must resolve")
                    .expect("the nested task must not panic")
                    .expect_err("a call admitted under a dead runtime must not dispatch");
                assert!(
                    err.contains("code mode runtime was invalidated"),
                    "expected the dispatch-boundary fence, got: {err}"
                );
            })
            .await;
    }

    /// Finding 4, explicit rewind (`SessionCommand::Rewind` → `handle_rewind`):
    /// committing a rewind invalidates Code Mode before it touches anything,
    /// so the runtime — and every yielded cell and `store()` value the
    /// discarded turns created — is gone, and parked nested work is fenced.
    #[tokio::test(flavor = "current_thread")]
    async fn explicit_rewind_shuts_code_mode_down() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

                let (_handle, bridge) = actor
                    .tool_context
                    .code_mode
                    .ensure_runtime()
                    .expect("the code mode runtime must initialize");
                let generation = bridge.expect("a fresh runtime yields its bridge").1;
                assert!(actor.tool_context.code_mode.handle().is_some());

                let response = actor
                    .handle_rewind(crate::session::RewindRequest {
                        target_prompt_index: 0,
                        mode: crate::session::RewindMode::FilesOnly,
                        force: true,
                    })
                    .await
                    .expect("rewind must not error");
                assert!(response.success, "{response:?}");

                assert!(
                    actor.tool_context.code_mode.current_generation() > generation,
                    "an explicit rewind must fence work admitted before it"
                );
                assert!(
                    actor.tool_context.code_mode.handle().is_none(),
                    "an explicit rewind must drop the runtime, its cells, and its store()"
                );
            })
            .await;
    }

    /// Finding 4, legacy rewind branch (`cancel_running_task` with
    /// `RewindIfNoOutput { prompt_id: None }`): it used to only abort the turn
    /// and terminate live cells, keeping the runtime and its `store()` state
    /// alive across a history rewind. Now it disposes of the whole runtime,
    /// matching the named cancel-history branch.
    #[tokio::test(flavor = "current_thread")]
    async fn legacy_rewind_cancel_shuts_code_mode_down() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

                let (_handle, bridge) = actor
                    .tool_context
                    .code_mode
                    .ensure_runtime()
                    .expect("the code mode runtime must initialize");
                let generation = bridge.expect("a fresh runtime yields its bridge").1;

                // A rewindable in-flight turn with a user row at the front.
                *actor
                    .current_prompt_id
                    .lock()
                    .expect("current_prompt_id mutex poisoned") = Some("rw".to_string());
                let (item, _rx) = crate::session::acp_session::support::user_item_with_rx(
                    "rw", "owner",
                );
                {
                    let mut state = actor.state.lock().await;
                    state.rewindable = true;
                    state.running_task = Some(AgentTask {
                        prompt_id: "rw".into(),
                        handle: tokio::task::spawn_local(async {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        })
                        .abort_handle(),
                    });
                    state.pending_inputs.push_back(item);
                }

                let _ = actor
                    .cancel_running_task(crate::session::CancelOptions {
                        history: crate::session::CancelHistoryDisposition::RewindIfNoOutput {
                            prompt_id: None,
                        },
                        user_initiated: true,
                        ..Default::default()
                    })
                    .await;

                assert!(
                    actor.tool_context.code_mode.current_generation() > generation,
                    "a legacy rewind must fence work admitted before it"
                );
                assert!(
                    actor.tool_context.code_mode.handle().is_none(),
                    "a legacy rewind must drop the runtime, its cells, and its store()"
                );
            })
            .await;
    }
}
