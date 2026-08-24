use super::*;

fn summary_with_kind(kind: Option<&str>) -> Summary {
    Summary {
        session_kind: kind.map(String::from),
        hidden: None,
        ..Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap()
    }
}

#[test]
fn summary_round_trips_and_defaults_reasoning_effort() {
    let mut s = summary_with_kind(None);
    s.reasoning_effort = None;
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        !json.contains("reasoning_effort"),
        "a None effort must not be serialized"
    );
    let back: Summary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.reasoning_effort, None);

    s.reasoning_effort = Some(ReasoningEffort::Xhigh);
    let json = serde_json::to_string(&s).unwrap();
    let back: Summary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Xhigh));
}

#[test]
fn summary_round_trips_previous_turn_compaction_contract() {
    let mut summary = summary_with_kind(None);
    summary.previous_turn_model = Some(crate::session::PreviousTurnModel {
        model_slug: "gpt-5.6-sol".to_owned(),
        context_window: 353_400,
        comp_hash: Some("3000".to_owned()),
    });

    let json = serde_json::to_string(&summary).unwrap();
    let restored: Summary = serde_json::from_str(&json).unwrap();
    let previous = restored.previous_turn_model.unwrap();
    assert_eq!(previous.model_slug, "gpt-5.6-sol");
    assert_eq!(previous.context_window, 353_400);
    assert_eq!(previous.comp_hash.as_deref(), Some("3000"));
}

#[test]
fn new_summary_pins_its_session_id_as_cache_affinity() {
    let summary = Summary::new(
        &Info {
            id: acp::SessionId::new("cache-session"),
            cwd: "/tmp".into(),
        },
        default_model_id(),
    )
    .unwrap();
    assert_eq!(summary.cache_affinity_id.as_deref(), Some("cache-session"));
    assert_eq!(summary.prompt_cache_affinity_id(), "cache-session");
}

#[test]
fn legacy_verbatim_fork_recovers_parent_cache_affinity() {
    let mut summary = summary_with_kind(Some("subagent_fork"));
    summary.cache_affinity_id = None;
    summary.fork_context_source = Some("forked_verbatim".into());
    summary.parent_session_id = Some("parent-session".into());
    assert_eq!(
        summary.restore_cache_affinity_id().as_deref(),
        Some("parent-session")
    );
}

#[test]
fn legacy_ordinary_fork_keeps_child_cache_affinity() {
    let mut summary = summary_with_kind(Some("fork"));
    summary.cache_affinity_id = None;
    summary.parent_session_id = Some("parent-session".into());
    assert_eq!(summary.restore_cache_affinity_id(), None);
    assert_eq!(summary.prompt_cache_affinity_id(), "test");
}

#[test]
fn old_summary_without_cache_affinity_deserializes() {
    let json = r#"{
        "info": { "id": "old-session", "cwd": "/tmp" },
        "session_summary": "",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "num_messages": 0,
        "num_chat_messages": 0,
        "current_model_id": "test-model"
    }"#;
    let summary: Summary = serde_json::from_str(json).unwrap();
    assert!(summary.cache_affinity_id.is_none());
    assert_eq!(summary.prompt_cache_affinity_id(), "old-session");
}

#[test]
fn hidden_for_all_subagent_kinds() {
    for kind in ["subagent", "subagent_fork", "subagent_resume"] {
        assert!(
            summary_with_kind(Some(kind)).is_hidden(),
            "{kind} should be hidden"
        );
    }
}

#[test]
fn not_hidden_for_regular_sessions() {
    assert!(!summary_with_kind(None).is_hidden());
    assert!(!summary_with_kind(Some("fork")).is_hidden());
    assert!(!summary_with_kind(Some("worktree")).is_hidden());
}

#[test]
fn explicit_hidden_overrides_session_kind() {
    let mut s = summary_with_kind(Some("subagent"));
    s.hidden = Some(false);
    assert!(!s.is_hidden(), "explicit hidden=false overrides kind");

    let mut s = summary_with_kind(None);
    s.hidden = Some(true);
    assert!(s.is_hidden(), "explicit hidden=true overrides kind");
}
