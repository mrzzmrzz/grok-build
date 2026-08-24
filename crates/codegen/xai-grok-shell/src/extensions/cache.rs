//! `x.ai/session/cache` — Codex Responses prompt-cache telemetry.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::SessionCommand;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionCacheRequest {
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCacheResponse {
    pub summary: crate::session::CacheSummary,
    pub recent_turns: Vec<crate::session::CacheTurnRecord>,
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if args.method.as_ref() != "x.ai/session/cache" {
        return Err(acp::Error::method_not_found());
    }
    let request: SessionCacheRequest = parse_params(args)?;
    let session_id = acp::SessionId::new(request.session_id.as_str());
    let Some(handle) = agent.session_handle_waiting_for_load(&session_id).await else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {}",
            request.session_id
        ))));
    };
    let (respond_to, response) = oneshot::channel();
    handle
        .cmd_tx
        .send(SessionCommand::GetCacheInfo { respond_to })
        .map_err(|_| acp::Error::internal_error().data("session actor channel closed"))?;
    let (summary, recent_turns) = response.await.map_err(|_| {
        acp::Error::internal_error().data("session actor dropped cache query reply")
    })?;
    to_raw_response(&SessionCacheResponse {
        summary,
        recent_turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_uses_camel_case_wire_fields() {
        let response = SessionCacheResponse {
            summary: crate::session::CacheSummary {
                total_input_tokens: 1_800,
                total_cached_tokens: 600,
                steady_input_tokens: 800,
                steady_cached_tokens: 600,
                steady_hit_rate_pct: 75.0,
                total_turns: 2,
                hits: 1,
                ..Default::default()
            },
            recent_turns: Vec::new(),
        };
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["summary"]["steadyHitRatePct"], 75.0);
        assert_eq!(json["summary"]["steadyInputTokens"], 800);
        assert!(json.get("recentTurns").is_some());
    }
}
