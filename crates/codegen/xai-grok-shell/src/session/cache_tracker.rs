//! Codex Responses prompt-cache telemetry for a live session.
//!
//! The tracker records each successful model request, excludes the first
//! request from the steady-state hit rate, and keeps a bounded set of recent
//! request-prefix diagnostics for `/cache`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{ConversationItem, ConversationRequest};

const MAX_RECENT_TURNS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ItemSummary {
    kind: &'static str,
    byte_len: usize,
    content_hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSummary {
    system_prompt_hash: u64,
    tools_hash: u64,
    items: Vec<ItemSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    FirstTurn,
    Hit,
    PartialHit,
    Break,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrefixDivergence {
    FirstTurn,
    PrefixIntact {
        preserved_items: usize,
        new_items: usize,
    },
    SystemPromptChanged,
    ToolsChanged,
    ItemDiverged {
        index: usize,
        previous_kind: String,
        current_kind: String,
    },
    HistoryTruncated {
        previous_items: usize,
        current_items: usize,
    },
}

impl PrefixDivergence {
    fn is_intact(&self) -> bool {
        matches!(self, Self::FirstTurn | Self::PrefixIntact { .. })
    }

    fn diagnostic(&self) -> String {
        match self {
            Self::FirstTurn => "Initial request; the prompt cache is cold.".to_string(),
            Self::PrefixIntact {
                preserved_items,
                new_items,
            } => format!(
                "Prompt prefix remained stable ({preserved_items} items preserved, {new_items} appended)."
            ),
            Self::SystemPromptChanged => {
                "System prompt changed since the previous request.".to_string()
            }
            Self::ToolsChanged => {
                "Tool definitions changed since the previous request.".to_string()
            }
            Self::ItemDiverged {
                index,
                previous_kind,
                current_kind,
            } => format!("Conversation item #{index} changed ({previous_kind} -> {current_kind})."),
            Self::HistoryTruncated {
                previous_items,
                current_items,
            } => format!(
                "Conversation history was truncated from {previous_items} to {current_items} items."
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheTurnRecord {
    pub turn_idx: String,
    pub loop_index: u32,
    pub prompt_tokens: u32,
    pub cached_prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_hit_rate_pct: f64,
    pub status: CacheStatus,
    pub divergence: PrefixDivergence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_gap_ms: Option<u64>,
    pub diagnostic: String,
    pub timestamp_rfc3339: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSummary {
    pub total_input_tokens: u64,
    pub total_cached_tokens: u64,
    pub steady_input_tokens: u64,
    pub steady_cached_tokens: u64,
    pub steady_hit_rate_pct: f64,
    pub total_turns: usize,
    pub hits: usize,
    pub partial_hits: usize,
    pub breaks: usize,
    pub last_break_diagnostic: Option<String>,
}

#[derive(Debug, Default)]
pub struct CacheTracker {
    previous_request: Option<RequestSummary>,
    previous_recorded_at: Option<Instant>,
    recent_turns: Vec<CacheTurnRecord>,
    summary: CacheSummary,
}

impl CacheTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn summary(&self) -> CacheSummary {
        self.summary.clone()
    }

    pub fn recent_turns(&self) -> &[CacheTurnRecord] {
        &self.recent_turns
    }

    pub fn summarize_request(request: &ConversationRequest) -> RequestSummary {
        let system_prompt_hash = request
            .items
            .iter()
            .find_map(|item| match item {
                ConversationItem::System(system) => Some(hash(system.content.as_bytes())),
                _ => None,
            })
            .unwrap_or_default();
        let tools_hash = hash_serialized(&request.tools);
        let items = request
            .items
            .iter()
            .map(|item| {
                let bytes = serde_json::to_vec(item).unwrap_or_default();
                ItemSummary {
                    kind: item_kind(item),
                    byte_len: bytes.len(),
                    content_hash: hash(&bytes),
                }
            })
            .collect();
        RequestSummary {
            system_prompt_hash,
            tools_hash,
            items,
        }
    }

    fn compare_prefix(
        previous: Option<&RequestSummary>,
        current: &RequestSummary,
    ) -> PrefixDivergence {
        let Some(previous) = previous else {
            return PrefixDivergence::FirstTurn;
        };
        if previous.system_prompt_hash != current.system_prompt_hash {
            return PrefixDivergence::SystemPromptChanged;
        }
        if previous.tools_hash != current.tools_hash {
            return PrefixDivergence::ToolsChanged;
        }
        for (index, (previous_item, current_item)) in
            previous.items.iter().zip(current.items.iter()).enumerate()
        {
            if previous_item.content_hash != current_item.content_hash {
                return PrefixDivergence::ItemDiverged {
                    index,
                    previous_kind: previous_item.kind.to_string(),
                    current_kind: current_item.kind.to_string(),
                };
            }
        }
        if current.items.len() < previous.items.len() {
            return PrefixDivergence::HistoryTruncated {
                previous_items: previous.items.len(),
                current_items: current.items.len(),
            };
        }
        PrefixDivergence::PrefixIntact {
            preserved_items: previous.items.len(),
            new_items: current.items.len().saturating_sub(previous.items.len()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_turn_outcome(
        &mut self,
        session_id: &str,
        turn_idx: &str,
        loop_index: u32,
        prompt_tokens: u32,
        cached_prompt_tokens: u32,
        completion_tokens: u32,
        request_started_at: Instant,
        current_request: RequestSummary,
    ) -> CacheTurnRecord {
        let recorded_at = Instant::now();
        let request_gap_ms = self.previous_recorded_at.map(|previous| {
            request_started_at
                .saturating_duration_since(previous)
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX)
        });
        let divergence = Self::compare_prefix(self.previous_request.as_ref(), &current_request);
        let hit_rate_pct = if prompt_tokens == 0 {
            0.0
        } else {
            (f64::from(cached_prompt_tokens) / f64::from(prompt_tokens)) * 100.0
        };
        let status = if self.previous_request.is_none() {
            CacheStatus::FirstTurn
        } else if cached_prompt_tokens == 0 {
            CacheStatus::Break
        } else if hit_rate_pct >= 50.0 {
            CacheStatus::Hit
        } else {
            CacheStatus::PartialHit
        };
        let diagnostic = match status {
            CacheStatus::FirstTurn => divergence.diagnostic(),
            CacheStatus::Hit if hit_rate_pct < 90.0 && divergence.is_intact() => format!(
                "Cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} input tokens cached). Remaining tokens are newly appended content."
            ),
            CacheStatus::Hit => format!(
                "Cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} input tokens cached)."
            ),
            CacheStatus::PartialHit => format!(
                "Partial cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} input tokens cached). {}",
                divergence.diagnostic()
            ),
            CacheStatus::Break if divergence.is_intact() => format!(
                "No cached tokens despite a stable prompt prefix; the provider cache may have expired or been evicted.{}",
                request_gap_ms
                    .map(|gap| format!(" Inter-request gap: {:.1}s.", gap as f64 / 1_000.0))
                    .unwrap_or_default()
            ),
            CacheStatus::Break => format!("Cache break: {}", divergence.diagnostic()),
        };

        self.summary.total_turns = self.summary.total_turns.saturating_add(1);
        self.summary.total_input_tokens = self
            .summary
            .total_input_tokens
            .saturating_add(u64::from(prompt_tokens));
        self.summary.total_cached_tokens = self
            .summary
            .total_cached_tokens
            .saturating_add(u64::from(cached_prompt_tokens));
        if status != CacheStatus::FirstTurn {
            self.summary.steady_input_tokens = self
                .summary
                .steady_input_tokens
                .saturating_add(u64::from(prompt_tokens));
            self.summary.steady_cached_tokens = self
                .summary
                .steady_cached_tokens
                .saturating_add(u64::from(cached_prompt_tokens));
            self.summary.steady_hit_rate_pct = if self.summary.steady_input_tokens == 0 {
                0.0
            } else {
                (self.summary.steady_cached_tokens as f64 / self.summary.steady_input_tokens as f64)
                    * 100.0
            };
        }
        match status {
            CacheStatus::FirstTurn => {}
            CacheStatus::Hit => self.summary.hits = self.summary.hits.saturating_add(1),
            CacheStatus::PartialHit => {
                self.summary.partial_hits = self.summary.partial_hits.saturating_add(1)
            }
            CacheStatus::Break => {
                self.summary.breaks = self.summary.breaks.saturating_add(1);
                self.summary.last_break_diagnostic = Some(diagnostic.clone());
            }
        }

        let record = CacheTurnRecord {
            turn_idx: turn_idx.to_string(),
            loop_index,
            prompt_tokens,
            cached_prompt_tokens,
            completion_tokens,
            cache_hit_rate_pct: (hit_rate_pct * 10.0).round() / 10.0,
            status,
            divergence,
            request_gap_ms,
            diagnostic,
            timestamp_rfc3339: Utc::now().to_rfc3339(),
        };
        if self.recent_turns.len() >= MAX_RECENT_TURNS {
            self.recent_turns.remove(0);
        }
        self.recent_turns.push(record.clone());
        self.previous_request = Some(current_request);
        self.previous_recorded_at = Some(recorded_at);

        tracing::info!(
            session_id,
            turn_idx,
            loop_index,
            prompt_tokens,
            cached_prompt_tokens,
            cache_hit_rate_pct = record.cache_hit_rate_pct,
            status = ?record.status,
            "Codex prompt cache outcome"
        );
        record
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn hash_serialized(value: &impl Serialize) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| hash(&bytes))
        .unwrap_or_default()
}

fn item_kind(item: &ConversationItem) -> &'static str {
    match item {
        ConversationItem::System(_) => "system",
        ConversationItem::User(_) => "user",
        ConversationItem::Assistant(_) => "assistant",
        ConversationItem::ToolResult(_) => "tool_result",
        ConversationItem::BackendToolCall(_) => "backend_tool_call",
        ConversationItem::Reasoning(_) => "reasoning",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xai_grok_sampling_types::{ConversationItem, SystemItem};

    fn request(text: &str) -> ConversationRequest {
        ConversationRequest {
            items: vec![
                ConversationItem::System(SystemItem {
                    content: Arc::from("system"),
                }),
                ConversationItem::user(text),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn steady_rate_excludes_cold_start_and_zero_denominator_is_safe() {
        let mut tracker = CacheTracker::new();
        let first = request("one");
        tracker.record_turn_outcome(
            "s",
            "1",
            1,
            1_000,
            0,
            10,
            Instant::now(),
            CacheTracker::summarize_request(&first),
        );
        let mut second = first;
        second.items.push(ConversationItem::user("two"));
        tracker.record_turn_outcome(
            "s",
            "2",
            1,
            800,
            600,
            10,
            Instant::now(),
            CacheTracker::summarize_request(&second),
        );
        let summary = tracker.summary();
        assert_eq!(summary.total_input_tokens, 1_800);
        assert_eq!(summary.steady_input_tokens, 800);
        assert_eq!(summary.steady_cached_tokens, 600);
        assert_eq!(summary.steady_hit_rate_pct, 75.0);
        assert_eq!(summary.total_turns, 2);
        assert_eq!(summary.hits, 1);
    }

    #[test]
    fn changed_prefix_is_reported_as_break() {
        let mut tracker = CacheTracker::new();
        let first = request("one");
        tracker.record_turn_outcome(
            "s",
            "1",
            1,
            100,
            0,
            1,
            Instant::now(),
            CacheTracker::summarize_request(&first),
        );
        let changed = request("changed");
        let record = tracker.record_turn_outcome(
            "s",
            "2",
            1,
            100,
            0,
            1,
            Instant::now(),
            CacheTracker::summarize_request(&changed),
        );
        assert_eq!(record.status, CacheStatus::Break);
        assert!(matches!(
            record.divergence,
            PrefixDivergence::ItemDiverged { index: 1, .. }
        ));
        assert_eq!(tracker.summary().breaks, 1);
    }
}
