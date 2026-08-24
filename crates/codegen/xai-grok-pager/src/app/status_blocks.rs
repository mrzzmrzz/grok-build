//! Read-only system-block text for `/queue`, `/tasks`, and `/usage`.
//!
//! Plain text committed into scrollback — the primary inspection surface in
//! minimal mode (no interactive panes). Kept out of `dispatch` for easy
//! unit tests.

use crate::app::agent::BgTaskStatus;
use crate::app::agent_view::AgentView;
use crate::app::subagent::format_subagent_label;
use crate::util::{format_duration, group_thousands};

/// `/queue` body — a read-only list of the queued prompts.
///
/// Server-authoritative shared-queue rows (the in-flight prompt excluded) come
/// first in broadcast order, then the local drip-feed queue — matching
/// [`crate::views::queue_pane::QueuePane::sync_from_merged`]'s ordering.
pub(crate) fn queue_block_text(agent: &AgentView) -> String {
    let running_id = agent.session.current_prompt_id.as_deref();

    let mut rows: Vec<String> = Vec::new();
    let mut pos = 1usize;
    for wire in &agent.shared_queue {
        if running_id == Some(wire.id.as_str()) {
            continue;
        }
        rows.push(format_queue_row(pos, &wire.text));
        pos += 1;
    }
    for prompt in &agent.session.pending_prompts {
        rows.push(format_queue_row(pos, &prompt.text));
        pos += 1;
    }

    if rows.is_empty() {
        "Queue is empty.".to_string()
    } else {
        let header = format!(
            "Queued prompt{} ({}):",
            if rows.len() == 1 { "" } else { "s" },
            rows.len()
        );
        join_header_rows(header, rows)
    }
}

///
/// [`crate::views::tasks_pane::TasksPane`] without its styled rows.
pub(crate) fn tasks_block_text(agent: &AgentView) -> String {
    let mut rows: Vec<String> = Vec::new();

    let mut workflows: Vec<_> = agent.workflow_runs.iter().collect();
    workflows.sort_by(|a, b| {
        b.is_active()
            .cmp(&a.is_active())
            .then(b.received_at.cmp(&a.received_at))
            .then(a.run_id.cmp(&b.run_id))
    });
    for run in workflows {
        let active = run.active_agent_count();
        let agents = match active {
            0 => String::new(),
            1 => " · 1 agent".to_string(),
            n => format!(" · {n} agents"),
        };
        let phase = run
            .current_phase
            .as_deref()
            .map(str::trim)
            .filter(|phase| !phase.is_empty())
            .map(|phase| format!(" · {phase}"))
            .unwrap_or_default();
        rows.push(format!(
            "  {:<9}Workflow · {}{phase}{agents}  ({})",
            if run.is_active() {
                "running".to_string()
            } else {
                run.status.replace('_', " ")
            },
            run.name,
            format_duration(std::time::Duration::from_millis(run.live_elapsed_ms()))
        ));
    }

    // ── Subagents ──
    let mut subs: Vec<_> = agent
        .subagent_sessions
        .values()
        .filter(|s| s.workflow_run_id.is_none())
        .collect();
    subs.sort_by(|a, b| {
        b.is_running()
            .cmp(&a.is_running())
            .then(b.started_at.cmp(&a.started_at))
            .then(a.child_session_id.cmp(&b.child_session_id))
    });
    for info in subs {
        let (type_label, desc) = format_subagent_label(info);
        let status = if info.pending_kill {
            "stopping"
        } else if info.is_running() {
            "running"
        } else {
            info.status.as_deref().unwrap_or("done")
        };
        let label = if desc.is_empty() {
            type_label
        } else {
            format!("{type_label} · {desc}")
        };
        rows.push(format!(
            "  {status:<9}{label}  ({})",
            format_duration(info.display_elapsed())
        ));
    }

    // ── Background tasks / monitors ──
    let mut tasks: Vec<_> = agent.session.bg_tasks.values().collect();
    tasks.sort_by(|a, b| {
        let (ar, br) = (
            a.status == BgTaskStatus::Running,
            b.status == BgTaskStatus::Running,
        );
        br.cmp(&ar)
            .then(b.start_time.cmp(&a.start_time))
            .then(a.task_id.cmp(&b.task_id))
    });
    for task in tasks {
        let kind = if task.is_monitor { "Monitor" } else { "Task" };
        let one_line = task
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_nonempty_line(&task.command));
        let status = if task.pending_kill {
            "stopping"
        } else {
            match task.status {
                BgTaskStatus::Running => "running",
                BgTaskStatus::Done => "done",
                BgTaskStatus::Failed => "failed",
            }
        };
        rows.push(format!(
            "  {status:<9}{kind} · {one_line}  ({})",
            format_duration(task.elapsed())
        ));
    }

    // ── Scheduled (/loop) tasks ──
    let mut sched: Vec<_> = agent.session.scheduled_tasks.values().collect();
    sched.sort_by(|a, b| {
        a.tag
            .cmp(&b.tag)
            .then(a.human_schedule.cmp(&b.human_schedule))
            .then(a.task_id.cmp(&b.task_id))
    });
    for info in sched {
        rows.push(format!(
            "  {:<9}{} · {} · {}",
            "scheduled",
            info.tag,
            info.human_schedule,
            first_nonempty_line(&info.prompt)
        ));
    }

    if rows.is_empty() {
        "No background tasks, workflows, or subagents.".to_string()
    } else {
        let header = format!(
            "Task{} ({}):",
            if rows.len() == 1 { "" } else { "s" },
            rows.len()
        );
        join_header_rows(header, rows)
    }
}

/// `/usage` body — per-session token and cost totals, scoped to the ledger's
/// lifetime: since session start, or since the last `/resume`.
pub(crate) fn session_usage_block_text(
    usage: &xai_grok_shell::extensions::notification::PromptUsage,
) -> String {
    let t = &usage.totals;
    if t.model_calls == 0 && usage.model_usage.is_empty() {
        return if usage.usage_is_incomplete {
            "Session usage: none recorded, but tracking is incomplete and may under-count."
                .to_string()
        } else {
            "Session usage: no model calls yet in this session.".to_string()
        };
    }

    let mut rows = Vec::new();
    rows.push(format!(
        "  Input tokens:   {} ({} cached)",
        group_thousands(t.input_tokens),
        group_thousands(t.cached_read_tokens),
    ));
    rows.push(format!(
        "  Output tokens:  {} ({} reasoning)",
        group_thousands(t.output_tokens),
        group_thousands(t.reasoning_tokens),
    ));
    rows.push(format!(
        "  Total tokens:   {}",
        group_thousands(t.total_tokens)
    ));
    rows.push(format!(
        "  Model calls:    {} · API time: {}",
        group_thousands(t.model_calls),
        format_duration(std::time::Duration::from_millis(t.api_duration_ms)),
    ));
    rows.push(format!("  Cost:           {}", format_cost(t)));

    if usage.model_usage.len() > 1 {
        rows.push("  By model:".to_string());
        for (model, m) in &usage.model_usage {
            rows.push(format!(
                "    {model}: {} in / {} out · {}",
                group_thousands(m.input_tokens),
                group_thousands(m.output_tokens),
                format_cost(m),
            ));
        }
    }

    if usage.usage_is_incomplete {
        rows.push("  Note: usage is incomplete and may under-count.".to_string());
    }

    join_header_rows(
        "Session usage (since start or last resume):".to_string(),
        rows,
    )
}

/// `/usage` Codex section — account-level OpenAI Codex usage as plain text.
/// First line is a header (the usage modal styles it bold; minimal mode
/// commits the whole block to scrollback). "Not connected" and fetch errors
/// are rendered here too, so a Codex failure never hides the xAI sections.
pub(crate) fn codex_usage_block_text(
    resp: &xai_grok_shell::extensions::codex::CodexUsageResponse,
) -> String {
    let header = "OpenAI Codex usage:".to_string();
    if !resp.logged_in {
        return join_header_rows(
            header,
            vec!["  Not connected. Run /login codex (or grok login --codex).".to_string()],
        );
    }
    if let Some(error) = &resp.error {
        return join_header_rows(header, vec![format!("  Couldn't load Codex usage: {error}")]);
    }
    let mut rows: Vec<String> = Vec::new();
    if let Some(email) = resp.email.as_deref().filter(|s| !s.is_empty()) {
        rows.push(format!("  Account:  {email}"));
    }
    if let Some(plan) = resp.plan_type.as_deref().filter(|s| !s.is_empty()) {
        rows.push(format!("  Plan:     {plan}"));
    }
    if let Some(rl) = &resp.rate_limit {
        if let Some(w) = &rl.primary_window {
            rows.push(codex_window_row(w));
        }
        if let Some(w) = &rl.secondary_window {
            rows.push(codex_window_row(w));
        }
        if rl.limit_reached {
            rows.push("  Rate limit reached.".to_string());
        }
    }
    if let Some(credits) = &resp.credits {
        rows.push(format!("  Credits:  {}", codex_credits_cell(credits)));
    }
    if rows.is_empty() {
        rows.push("  No usage data reported.".to_string());
    }
    join_header_rows(header, rows)
}

/// One rate-limit window row: `  5h limit: 34% used · resets in 2h 10m`.
fn codex_window_row(w: &xai_grok_shell::codex_auth::CodexRateLimitWindow) -> String {
    let used = w.used_percent.clamp(0.0, 100.0).floor() as i64;
    let reset = format_duration(std::time::Duration::from_secs(
        w.reset_after_seconds.max(0) as u64,
    ));
    format!(
        "  {}: {used}% used \u{b7} resets in {reset}",
        codex_window_label(w.limit_window_seconds)
    )
}

/// Human label for a rate-limit window length in seconds.
fn codex_window_label(secs: i64) -> String {
    const HOUR: i64 = 3_600;
    const DAY: i64 = 24 * HOUR;
    if secs == 7 * DAY {
        "Weekly limit".to_string()
    } else if secs > 0 && secs % DAY == 0 {
        format!("{}d limit", secs / DAY)
    } else if secs > 0 && secs % HOUR == 0 {
        format!("{}h limit", secs / HOUR)
    } else {
        format!("{}m limit", secs.max(0) / 60)
    }
}

/// Credits cell: unlimited > explicit balance > available/none.
fn codex_credits_cell(c: &xai_grok_shell::codex_auth::CodexCredits) -> String {
    if c.unlimited {
        return "unlimited".to_string();
    }
    if let Some(balance) = &c.balance {
        return match balance {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if c.has_credits {
        "available".to_string()
    } else {
        "none".to_string()
    }
}

/// `/login codex` result line(s) for scrollback (or toast fallback).
pub(crate) fn codex_login_result_text(
    result: &Result<Box<xai_grok_shell::extensions::codex::CodexAuthActionResponse>, String>,
) -> String {
    const DEVICE_AUTH_HINT: &str =
        "No browser on this machine? Run: grok login --codex --device-auth";
    match result {
        Ok(resp) if resp.ok => {
            let account = resp
                .email
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("your ChatGPT account");
            match resp.plan_type.as_deref().filter(|s| !s.is_empty()) {
                Some(plan) => format!("Connected to OpenAI Codex as {account} (plan: {plan})."),
                None => format!("Connected to OpenAI Codex as {account}."),
            }
        }
        Ok(resp) => {
            let error = resp.error.as_deref().unwrap_or("unknown error");
            format!("Codex login failed: {error}\n{DEVICE_AUTH_HINT}")
        }
        Err(error) => format!("Codex login failed: {error}\n{DEVICE_AUTH_HINT}"),
    }
}

/// `/logout codex` result line for scrollback (or toast fallback).
pub(crate) fn codex_logout_result_text(
    result: &Result<Box<xai_grok_shell::extensions::codex::CodexAuthActionResponse>, String>,
) -> String {
    match result {
        Ok(resp) if resp.ok => match resp.was_logged_in {
            Some(false) => "OpenAI Codex was not connected.".to_string(),
            _ => "Disconnected OpenAI Codex.".to_string(),
        },
        Ok(resp) => format!(
            "Codex logout failed: {}",
            resp.error.as_deref().unwrap_or("unknown error")
        ),
        Err(error) => format!("Codex logout failed: {error}"),
    }
}

/// Cost cell. Ticks are 1e10 per USD; partial sums are scrubbed to absent.
fn format_cost(m: &xai_grok_shell::extensions::notification::PromptUsageModel) -> String {
    use xai_grok_shell::extensions::notification::ticks_to_usd;
    match m.cost_usd_ticks {
        Some(ticks) => format!("${:.4}", ticks_to_usd(ticks)),
        None if m.cost_is_partial => "not available (not reported for some calls)".to_string(),
        None => "not available (not reported)".to_string(),
    }
}

/// First non-empty, trimmed line of `text` (empty string if none). Collapses a
/// multi-line prompt/command to a single display line.
pub(crate) fn first_nonempty_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Format one `/queue` row as `  #N  <first non-empty line>` with a
/// `(+K more lines)` suffix for multi-line prompts.
fn format_queue_row(pos: usize, text: &str) -> String {
    let first_line = first_nonempty_line(text);
    let extra = text.lines().count().saturating_sub(1);
    if extra > 0 {
        format!(
            "  #{pos}  {first_line}  (+{extra} more line{})",
            if extra == 1 { "" } else { "s" }
        )
    } else {
        format!("  #{pos}  {first_line}")
    }
}

/// Join a header line above its rows into a single block string.
fn join_header_rows(header: String, rows: Vec<String>) -> String {
    std::iter::once(header)
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_shell::extensions::notification::{PromptUsage, PromptUsageModel};

    fn model_row(input: u64, output: u64, ticks: Option<i64>) -> PromptUsageModel {
        PromptUsageModel {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_read_tokens: 0,
            cache_creation_tokens: 0,
            reasoning_tokens: 0,
            model_calls: 1,
            api_duration_ms: 1_000,
            cost_usd_ticks: ticks,
            cost_is_partial: false,
            cost_missing_calls: 0,
        }
    }

    #[test]
    fn session_usage_block_empty_ledger() {
        let usage = PromptUsage::default();
        assert_eq!(
            session_usage_block_text(&usage),
            "Session usage: no model calls yet in this session."
        );

        // Empty but incomplete must not read as a clean zero.
        let incomplete = PromptUsage {
            usage_is_incomplete: true,
            ..Default::default()
        };
        assert!(session_usage_block_text(&incomplete).contains("incomplete"));
    }

    #[test]
    fn session_usage_block_formats_tokens_and_cost() {
        let mut totals = model_row(1_234_567, 45_678, Some(12_345_000_000));
        totals.cached_read_tokens = 1_000_000;
        totals.reasoning_tokens = 12_000;
        totals.model_calls = 42;
        totals.api_duration_ms = 192_000;
        let usage = PromptUsage {
            totals,
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        // Snapshot pins content and column alignment together; single-model
        // sessions must skip the redundant by-model breakdown.
        insta::assert_snapshot!("session_usage_block_full", text);
    }

    #[test]
    fn session_usage_block_lists_models_when_multiple() {
        let mut usage = PromptUsage {
            totals: model_row(150, 15, None),
            ..Default::default()
        };
        usage
            .model_usage
            .insert("grok-build".into(), model_row(100, 10, None));
        usage
            .model_usage
            .insert("grok-4".into(), model_row(50, 5, None));
        let text = session_usage_block_text(&usage);
        assert!(text.contains("By model:"), "{text}");
        assert!(text.contains("grok-build: 100 in / 10 out"), "{text}");
        assert!(text.contains("grok-4: 50 in / 5 out"), "{text}");
    }

    #[test]
    fn session_usage_block_absent_cost_is_unknown_not_free() {
        let usage = PromptUsage {
            totals: model_row(100, 10, None),
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        insta::assert_snapshot!("session_usage_block_absent_cost", text);
        // Unknown cost must never read as free.
        assert!(!text.contains("$0"), "{text}");
    }

    #[test]
    fn session_usage_block_flags_partial_and_incomplete() {
        let mut totals = model_row(100, 10, None);
        totals.cost_is_partial = true;
        let usage = PromptUsage {
            totals,
            usage_is_incomplete: true,
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        assert!(text.contains("not reported for some calls"), "{text}");
        assert!(text.contains("usage is incomplete"), "{text}");
    }

    #[test]
    fn group_thousands_groups_digits() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn first_nonempty_line_skips_blank_leading_lines() {
        assert_eq!(first_nonempty_line("\n  \n  hello \nworld"), "hello");
        assert_eq!(first_nonempty_line("   "), "");
        assert_eq!(first_nonempty_line(""), "");
        assert_eq!(first_nonempty_line("only"), "only");
    }

    #[test]
    fn format_queue_row_single_line() {
        assert_eq!(format_queue_row(1, "fix the bug"), "  #1  fix the bug");
    }

    #[test]
    fn format_queue_row_multiline_reports_extra_lines() {
        assert_eq!(
            format_queue_row(2, "first\nsecond"),
            "  #2  first  (+1 more line)"
        );
        assert_eq!(
            format_queue_row(3, "first\nsecond\nthird"),
            "  #3  first  (+2 more lines)"
        );
    }

    // ── Codex usage / auth text ─────────────────────────────────────

    use xai_grok_shell::codex_auth::{CodexCredits, CodexRateLimit, CodexRateLimitWindow};
    use xai_grok_shell::extensions::codex::{CodexAuthActionResponse, CodexUsageResponse};

    fn codex_usage(logged_in: bool) -> CodexUsageResponse {
        CodexUsageResponse {
            logged_in,
            email: None,
            plan_type: None,
            rate_limit: None,
            credits: None,
            error: None,
        }
    }

    fn window(window_secs: i64, used: f64, reset_secs: i64) -> CodexRateLimitWindow {
        CodexRateLimitWindow {
            used_percent: used,
            limit_window_seconds: window_secs,
            reset_after_seconds: reset_secs,
            reset_at: 0,
        }
    }

    #[test]
    fn codex_usage_block_not_connected() {
        let text = codex_usage_block_text(&codex_usage(false));
        assert_eq!(
            text,
            "OpenAI Codex usage:\n  Not connected. Run /login codex (or grok login --codex)."
        );
    }

    #[test]
    fn codex_usage_block_error_is_isolated_to_one_line() {
        let mut resp = codex_usage(true);
        resp.error = Some("Codex usage request returned 500".to_string());
        let text = codex_usage_block_text(&resp);
        assert_eq!(
            text,
            "OpenAI Codex usage:\n  Couldn't load Codex usage: Codex usage request returned 500"
        );
    }

    #[test]
    fn codex_usage_block_full_snapshot() {
        let mut resp = codex_usage(true);
        resp.email = Some("dev@example.com".to_string());
        resp.plan_type = Some("plus".to_string());
        resp.rate_limit = Some(CodexRateLimit {
            allowed: true,
            limit_reached: false,
            primary_window: Some(window(18_000, 34.9, 7_800)),
            secondary_window: Some(window(604_800, 12.0, 3 * 86_400)),
        });
        resp.credits = Some(CodexCredits {
            has_credits: true,
            unlimited: false,
            balance: Some(serde_json::json!("4.20")),
        });
        let text = codex_usage_block_text(&resp);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "OpenAI Codex usage:");
        assert_eq!(lines[1], "  Account:  dev@example.com");
        assert_eq!(lines[2], "  Plan:     plus");
        assert!(
            lines[3].starts_with("  5h limit: 34% used \u{b7} resets in "),
            "{:?}",
            lines[3]
        );
        assert!(
            lines[4].starts_with("  Weekly limit: 12% used \u{b7} resets in "),
            "{:?}",
            lines[4]
        );
        assert_eq!(lines[5], "  Credits:  4.20");
    }

    #[test]
    fn codex_usage_block_limit_reached_and_unlimited_credits() {
        let mut resp = codex_usage(true);
        resp.rate_limit = Some(CodexRateLimit {
            allowed: false,
            limit_reached: true,
            primary_window: None,
            secondary_window: None,
        });
        resp.credits = Some(CodexCredits {
            has_credits: false,
            unlimited: true,
            balance: None,
        });
        let text = codex_usage_block_text(&resp);
        assert!(text.contains("  Rate limit reached."), "{text}");
        assert!(text.contains("  Credits:  unlimited"), "{text}");
    }

    #[test]
    fn codex_usage_block_empty_snapshot_has_placeholder() {
        let text = codex_usage_block_text(&codex_usage(true));
        assert_eq!(text, "OpenAI Codex usage:\n  No usage data reported.");
    }

    #[test]
    fn codex_window_labels() {
        assert_eq!(codex_window_label(18_000), "5h limit");
        assert_eq!(codex_window_label(604_800), "Weekly limit");
        assert_eq!(codex_window_label(2 * 86_400), "2d limit");
        assert_eq!(codex_window_label(1_800), "30m limit");
        assert_eq!(codex_window_label(-5), "0m limit");
    }

    fn auth_resp(ok: bool) -> CodexAuthActionResponse {
        CodexAuthActionResponse {
            ok,
            email: None,
            plan_type: None,
            was_logged_in: None,
            error: None,
        }
    }

    #[test]
    fn codex_login_text_success_includes_email_and_plan() {
        let mut resp = auth_resp(true);
        resp.email = Some("dev@example.com".to_string());
        resp.plan_type = Some("pro".to_string());
        assert_eq!(
            codex_login_result_text(&Ok(Box::new(resp))),
            "Connected to OpenAI Codex as dev@example.com (plan: pro)."
        );
        assert_eq!(
            codex_login_result_text(&Ok(Box::new(auth_resp(true)))),
            "Connected to OpenAI Codex as your ChatGPT account."
        );
    }

    #[test]
    fn codex_login_text_failure_hints_device_auth() {
        let mut resp = auth_resp(false);
        resp.error = Some("could not open a browser".to_string());
        let text = codex_login_result_text(&Ok(Box::new(resp)));
        assert!(text.starts_with("Codex login failed: could not open a browser"));
        assert!(text.contains("grok login --codex --device-auth"), "{text}");
        let text = codex_login_result_text(&Err("agent unreachable".to_string()));
        assert!(text.contains("agent unreachable"), "{text}");
        assert!(text.contains("grok login --codex --device-auth"), "{text}");
    }

    #[test]
    fn codex_logout_text_variants() {
        let mut resp = auth_resp(true);
        resp.was_logged_in = Some(true);
        assert_eq!(
            codex_logout_result_text(&Ok(Box::new(resp))),
            "Disconnected OpenAI Codex."
        );
        let mut resp = auth_resp(true);
        resp.was_logged_in = Some(false);
        assert_eq!(
            codex_logout_result_text(&Ok(Box::new(resp))),
            "OpenAI Codex was not connected."
        );
        let mut resp = auth_resp(false);
        resp.error = Some("revoke failed".to_string());
        assert_eq!(
            codex_logout_result_text(&Ok(Box::new(resp))),
            "Codex logout failed: revoke failed"
        );
        assert_eq!(
            codex_logout_result_text(&Err("agent unreachable".to_string())),
            "Codex logout failed: agent unreachable"
        );
    }
}
