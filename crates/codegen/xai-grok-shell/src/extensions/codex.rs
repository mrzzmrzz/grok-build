//! Codex account extension handlers: `x.ai/codex/login`, `x.ai/codex/logout`,
//! and `x.ai/usage/codex`.
//!
//! These let the TUI drive the OpenAI Codex (ChatGPT OAuth) account without
//! leaving the alternate screen. All three are provider-isolated: they never
//! touch xAI credentials, and a Codex failure is reported in-band (the
//! response carries an `error` field) so the client can render it next to a
//! healthy xAI surface instead of failing the whole request.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, to_raw_response};
use crate::agent::MvpAgent;

/// Wire response for `x.ai/codex/login` and `x.ai/codex/logout`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexAuthActionResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    /// Logout only: whether local credentials existed before removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was_logged_in: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Wire response for `x.ai/usage/codex`.
///
/// `logged_in: false` is a normal outcome (not an error): the client should
/// suggest `grok login --codex`. A fetch failure while logged in sets
/// `error`; the other fields are then absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexUsageResponse {
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::codex_auth::CodexRateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<crate::codex_auth::CodexCredits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CodexUsageResponse {
    fn logged_out() -> Self {
        Self {
            logged_in: false,
            email: None,
            plan_type: None,
            rate_limit: None,
            credits: None,
            error: None,
        }
    }
}

/// Serializes Codex login/logout end-to-end (each handler holds the guard
/// for its full duration, browser wait included) so interleaved operations
/// cannot reverse the user's final intent. Cancellation is allocated at
/// ENQUEUE time: a login registers its `LoginAttemptGuard` *before* queueing
/// here, and a logout cancels every registered attempt (active AND queued)
/// *before* queueing — so with login A active, login B queued, and logout C
/// queued, C invalidates both A and B instead of letting B start a fresh
/// callback wait ahead of C. The logout fence inside `codex_auth` (process
/// generation + persisted on-disk epoch, both checked under the auth file
/// lock) is the backstop that keeps a completed stale callback — from this
/// process or another one — from resurrecting credentials.
static CODEX_AUTH_OP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The enqueue-time fence, checked right after the FIFO lock is acquired: a
/// queued login that a logout invalidated while it waited reports itself as
/// superseded instead of opening a browser the user no longer wants.
fn queued_login_superseded(
    attempt: &crate::codex_auth::LoginAttemptGuard,
) -> Option<CodexAuthActionResponse> {
    attempt.is_cancelled().then(|| CodexAuthActionResponse {
        ok: false,
        email: None,
        plan_type: None,
        was_logged_in: None,
        error: Some("Codex login was superseded by a logout".to_string()),
    })
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/codex/login" => handle_login(agent).await,
        "x.ai/codex/logout" => handle_logout(agent).await,
        "x.ai/usage/codex" => handle_usage().await,
        _ => Err(acp::Error::method_not_found()),
    }
}

/// Browser OAuth login for the TUI. Blocks (cooperatively) until the OAuth
/// callback lands or times out; ext methods run as independent tasks, so
/// other extension traffic keeps flowing meanwhile. On success the model
/// catalog is re-announced immediately (the login-gated visibility filter now
/// admits Codex entries) and a background Codex catalog refresh is spawned so
/// live GPT models appear in the picker without a restart.
async fn handle_login(agent: &MvpAgent) -> ExtResult {
    // Register BEFORE queueing on the operation lock: a logout issued while
    // this login is still queued cancels the attempt at its enqueue
    // generation, and the check below aborts it without a callback wait.
    let attempt = crate::codex_auth::register_login_attempt();
    let _op = CODEX_AUTH_OP_LOCK.lock().await;
    if let Some(response) = queued_login_superseded(&attempt) {
        return to_raw_response(&response);
    }
    match crate::codex_auth::run_tui_login().await {
        Ok(summary) => {
            tracing::info_span!("auth.lifecycle", action = "codex_login", success = true)
                .in_scope(|| {});
            // Account transition first: scope the in-memory catalog to the
            // account that just logged in (and restore the current-model
            // invariant) before anything is re-announced.
            agent.models_manager.on_codex_auth_changed();
            // Keep the OAuth bearer fresh for long-running sessions; the
            // loop is a process-wide singleton, so a re-login is a no-op.
            crate::codex_auth::start_proactive_refresh(tokio_util::sync::CancellationToken::new());
            notify_models_updated(agent);
            agent.models_manager.spawn_background_refresh();
            to_raw_response(&CodexAuthActionResponse {
                ok: true,
                email: summary.email,
                plan_type: summary.plan_type,
                was_logged_in: None,
                error: None,
            })
        }
        Err(error) => to_raw_response(&CodexAuthActionResponse {
            ok: false,
            email: None,
            plan_type: None,
            was_logged_in: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

/// Codex-only logout: revokes/removes the Codex credential and its catalog
/// cache (never touches xAI auth), then re-announces the model catalog so
/// Codex entries disappear from pickers via the login-gated visibility filter.
async fn handle_logout(agent: &MvpAgent) -> ExtResult {
    // Wake a login stuck on its browser callback before queueing on the
    // operation lock, so this logout runs as soon as that login aborts
    // instead of waiting out the callback window.
    crate::codex_auth::cancel_pending_login();
    let _op = CODEX_AUTH_OP_LOCK.lock().await;
    let result = crate::codex_auth::run_cli_logout().await;
    // Drop the logged-out account's in-memory catalog, fence any in-flight
    // refresh, and move the current model onto an available fallback before
    // the catalog is re-announced. This runs on the failure path too: a
    // logout that removed the credentials but could not persist its
    // cross-process fence still left this process logged out, and the
    // reconciliation is a re-read of on-disk state either way.
    agent.models_manager.on_codex_auth_changed();
    notify_models_updated(agent);
    match result {
        Ok(was_logged_in) => {
            tracing::info_span!("auth.lifecycle", action = "codex_logout", success = true)
                .in_scope(|| {});
            to_raw_response(&CodexAuthActionResponse {
                ok: true,
                email: None,
                plan_type: None,
                was_logged_in: Some(was_logged_in),
                error: None,
            })
        }
        Err(error) => to_raw_response(&CodexAuthActionResponse {
            ok: false,
            email: None,
            plan_type: None,
            was_logged_in: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

async fn handle_usage() -> ExtResult {
    if !crate::codex_auth::is_logged_in() {
        return to_raw_response(&CodexUsageResponse::logged_out());
    }
    match crate::codex_auth::fetch_usage().await {
        Ok(snapshot) => {
            let account = snapshot.account.as_ref();
            to_raw_response(&CodexUsageResponse {
                logged_in: true,
                email: account.and_then(|a| a.email.clone()),
                plan_type: snapshot
                    .plan_type
                    .clone()
                    .or_else(|| account.and_then(|a| a.plan_type.clone())),
                rate_limit: snapshot.rate_limit,
                credits: snapshot.credits,
                error: None,
            })
        }
        Err(error) => to_raw_response(&CodexUsageResponse {
            logged_in: true,
            email: None,
            plan_type: None,
            rate_limit: None,
            credits: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

/// Mirror of `ModelsManager::notify_models_updated` over the agent's own
/// gateway sender: re-announce the current catalog after a Codex login/logout
/// so clients re-derive visibility without an xAI catalog refetch.
fn notify_models_updated(agent: &MvpAgent) {
    let available = agent.models_manager.available();
    let current = agent.models_manager.current_model_id();
    let model_state = acp::SessionModelState::new(current, available.values().cloned().collect());
    if let Ok(params) = serde_json::value::to_raw_value(&model_state) {
        agent
            .gateway
            .forward_fire_and_forget(acp::ExtNotification::new(
                "x.ai/models/update",
                params.into(),
            ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_response_round_trips_snapshot_fields() {
        let resp = CodexUsageResponse {
            logged_in: true,
            email: Some("dev@example.com".into()),
            plan_type: Some("plus".into()),
            rate_limit: Some(crate::codex_auth::CodexRateLimit {
                allowed: true,
                limit_reached: false,
                primary_window: Some(crate::codex_auth::CodexRateLimitWindow {
                    used_percent: 34.5,
                    limit_window_seconds: 18_000,
                    reset_after_seconds: 7_800,
                    reset_at: 1_700_000_000,
                }),
                secondary_window: None,
            }),
            credits: Some(crate::codex_auth::CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: Some(serde_json::json!("4.20")),
            }),
            error: None,
        };
        let wire = serde_json::to_value(&resp).unwrap();
        assert_eq!(wire["loggedIn"], true);
        assert_eq!(wire["planType"], "plus");
        assert_eq!(wire["rateLimit"]["primary_window"]["used_percent"], 34.5);
        let rt: CodexUsageResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(rt, resp);
    }

    #[test]
    fn usage_response_logged_out_omits_optionals() {
        let wire = serde_json::to_value(CodexUsageResponse::logged_out()).unwrap();
        assert_eq!(wire, serde_json::json!({ "loggedIn": false }));
    }

    /// Concurrent login/logout serialize on the operation lock: a logout
    /// issued while a login operation is in flight waits for it to finish
    /// (its browser wait having been cancelled up front) instead of
    /// interleaving with it.
    #[tokio::test]
    async fn concurrent_login_and_logout_serialize_on_the_op_lock() {
        // The "login" holds the operation lock, as handle_login does for its
        // full duration.
        let login_guard = CODEX_AUTH_OP_LOCK.lock().await;
        // The "logout" queues behind it, as handle_logout does.
        let logout = tokio::spawn(async {
            let _guard = CODEX_AUTH_OP_LOCK.lock().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !logout.is_finished(),
            "logout must serialize behind the in-flight login operation"
        );
        drop(login_guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), logout)
            .await
            .expect("logout proceeds once the login operation completes")
            .unwrap();
    }

    /// Enqueue-time fence: with login A active and login B queued, a logout
    /// C cancels BOTH registrations; when B finally acquires the operation
    /// lock it reports itself superseded instead of opening a browser. An
    /// attempt registered after the logout proceeds normally.
    #[test]
    fn queued_login_invalidated_by_logout_reports_superseded() {
        // The registry is process-global and sibling tests run logouts
        // concurrently, so establish the pre-logout state with a retry.
        let (login_a, login_b) = loop {
            let a = crate::codex_auth::register_login_attempt(); // active
            let b = crate::codex_auth::register_login_attempt(); // queued
            if queued_login_superseded(&a).is_none() && queued_login_superseded(&b).is_none() {
                break (a, b);
            }
        };
        // Logout C runs its up-front cancellation (see handle_logout).
        crate::codex_auth::cancel_pending_login();
        let response = queued_login_superseded(&login_b)
            .expect("queued login B must be invalidated by logout C");
        assert!(!response.ok);
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("superseded"),
            "{response:?}"
        );
        assert!(
            queued_login_superseded(&login_a).is_some(),
            "the active login A is invalidated too"
        );
        drop(login_a);
        drop(login_b);
        // A login enqueued after the logout is a new intent (bounded retry —
        // sibling logouts may keep racing).
        assert!(
            (0..100).any(|_| {
                queued_login_superseded(&crate::codex_auth::register_login_attempt()).is_none()
            }),
            "a login enqueued after the logout must proceed"
        );
    }

    #[test]
    fn auth_action_response_round_trips() {
        let resp = CodexAuthActionResponse {
            ok: true,
            email: Some("dev@example.com".into()),
            plan_type: Some("pro".into()),
            was_logged_in: None,
            error: None,
        };
        let wire = serde_json::to_value(&resp).unwrap();
        assert_eq!(wire["ok"], true);
        assert_eq!(wire["email"], "dev@example.com");
        assert!(wire.get("wasLoggedIn").is_none());
        let rt: CodexAuthActionResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(rt, resp);
    }
}
