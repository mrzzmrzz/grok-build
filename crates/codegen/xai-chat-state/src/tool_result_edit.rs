//! Single source of truth for mutating a [`ToolResultItem`]'s content.
//!
//! A tool result carries three representations of the same payload:
//!
//! - `parts` — the ordered mixed text/image content. When non-empty the API
//!   conversion layers emit **exactly** this sequence on the wire.
//! - `content` — the legacy plain-text mirror (the text parts joined).
//! - `images` — the legacy image mirror.
//!
//! Any mutation that touches only the legacy mirrors silently leaves the
//! removed text or image alive in `parts` — and therefore on the wire —
//! bypassing old-result trimming, image budgets, persisted-history
//! sanitation, and workspace path rewriting. Every in-place mutation of a
//! tool result must go through the helpers in this module so all three
//! representations move atomically.
//!
//! (The long-term fix is a single canonical representation with legacy views
//! derived at compatibility boundaries; until that migration these helpers
//! are the one mutation chokepoint.)

use std::sync::Arc;

use xai_grok_sampling_types::{ContentPart, ConversationItem, ToolResultItem};

fn is_image(part: &ContentPart) -> bool {
    matches!(part, ContentPart::Image { .. })
}

/// Replace the entire text of a tool result while keeping its images.
///
/// Legacy layout (`parts` empty): only `content` changes, exactly as before.
/// Ordered layout: the text parts collapse into one leading text part carrying
/// the new text (interleaving with images is necessarily lost — the caller is
/// replacing the text wholesale), image parts keep their relative order, and
/// the `images` mirror is rebuilt from the surviving image parts.
pub fn set_tool_result_text(t: &mut ToolResultItem, text: impl Into<Arc<str>>) {
    let text = text.into();
    if !t.parts.is_empty() {
        let mut parts: Vec<ContentPart> = Vec::with_capacity(1 + t.parts.len());
        parts.push(ContentPart::Text {
            text: Arc::clone(&text),
        });
        parts.extend(t.parts.iter().filter(|p| is_image(p)).cloned());
        t.images = parts.iter().filter(|p| is_image(p)).cloned().collect();
        t.parts = parts;
    }
    t.content = text;
}

/// Append a model-visible note (e.g. an image-eviction notice) to a tool
/// result's text, in both the legacy mirror and the ordered parts.
pub fn append_tool_result_note(t: &mut ToolResultItem, note: &str) {
    t.content = Arc::<str>::from(format!("{}\n\n{note}", t.content));
    if !t.parts.is_empty() {
        t.parts.push(ContentPart::Text {
            text: Arc::<str>::from(note),
        });
    }
}

/// Remove the first image of a tool result from **both** the `images` mirror
/// and the ordered `parts` (matched by URL so a mirror/parts skew cannot
/// remove the wrong part). Returns the removed image's URL, or `None` when
/// the result holds no image.
pub fn remove_first_tool_result_image(t: &mut ToolResultItem) -> Option<Arc<str>> {
    let index = t.parts.iter().position(is_image);
    let mirror_index = t.images.iter().position(is_image);
    let url = match (mirror_index, index) {
        (Some(mi), _) => {
            let ContentPart::Image { url } = t.images.remove(mi) else {
                unreachable!("position() matched an image part");
            };
            url
        }
        // Defensive: an image present only in `parts` must still be removable,
        // otherwise it would survive on the wire unaccounted.
        (None, Some(pi)) => {
            let ContentPart::Image { url } = t.parts.remove(pi) else {
                unreachable!("position() matched an image part");
            };
            return Some(url);
        }
        (None, None) => return None,
    };
    if let Some(pi) = t
        .parts
        .iter()
        .position(|p| matches!(p, ContentPart::Image { url: u } if *u == url))
    {
        t.parts.remove(pi);
    }
    Some(url)
}

/// Retain only the images accepted by `keep`, in both the `images` mirror and
/// the ordered `parts`. Text parts are never touched. Returns the number of
/// images removed (an image present in both representations counts once).
pub fn retain_tool_result_images(
    t: &mut ToolResultItem,
    keep: impl Fn(&ContentPart) -> bool,
) -> usize {
    let images_before = t.images.len();
    t.images.retain(|part| !is_image(part) || keep(part));
    let images_removed = images_before - t.images.len();

    let parts_before = t.parts.len();
    t.parts.retain(|part| !is_image(part) || keep(part));
    let parts_removed = parts_before - t.parts.len();

    images_removed.max(parts_removed)
}

/// CWD rewriting for forked/synced sessions, kept in lockstep across the
/// legacy mirrors **and** the ordered `parts`.
///
/// Delegates the mirror rewrite to
/// [`xai_grok_sampling_types::transform_conversation_cwd`] and then applies
/// the same substitution to every text part of every tool result, which the
/// upstream function does not yet touch.
pub fn transform_conversation_cwd_synced(
    items: &mut [ConversationItem],
    source_cwd: &str,
    target_cwd: &str,
) {
    xai_grok_sampling_types::transform_conversation_cwd(items, source_cwd, target_cwd);
    for item in items.iter_mut() {
        let ConversationItem::ToolResult(t) = item else {
            continue;
        };
        for part in t.parts.iter_mut() {
            if let ContentPart::Text { text } = part
                && text.contains(source_cwd)
            {
                *text = Arc::<str>::from(text.replace(source_cwd, target_cwd));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::{ConversationRequest, rs};

    fn text(t: &str) -> ContentPart {
        ContentPart::Text {
            text: Arc::<str>::from(t),
        }
    }

    fn image(url: &str) -> ContentPart {
        ContentPart::Image {
            url: Arc::<str>::from(url),
        }
    }

    /// The serialized Responses request body — what actually goes on the wire.
    fn wire_json(items: Vec<ConversationItem>) -> String {
        let req = ConversationRequest::from_items(items);
        let responses: rs::CreateResponse = (&req).into();
        serde_json::to_string(&responses).unwrap()
    }

    fn ordered_tool_result(id: &str) -> ConversationItem {
        ConversationItem::tool_result_with_parts(
            id,
            vec![
                text("SECRET-BEFORE"),
                image("data:image/png;base64,EVICTME"),
                text("SECRET-AFTER"),
            ],
        )
    }

    fn assert_mirrors_consistent(item: &ConversationItem) {
        let ConversationItem::ToolResult(t) = item else {
            panic!("expected ToolResult");
        };
        let part_images: Vec<_> = t
            .parts
            .iter()
            .filter(|p| matches!(p, ContentPart::Image { .. }))
            .cloned()
            .collect();
        let mirror_images: Vec<_> = t
            .images
            .iter()
            .filter(|p| matches!(p, ContentPart::Image { .. }))
            .cloned()
            .collect();
        if !t.parts.is_empty() {
            assert_eq!(
                serde_json::to_value(&part_images).unwrap(),
                serde_json::to_value(&mirror_images).unwrap(),
                "images mirror must match the image parts"
            );
            for part in &t.parts {
                if let ContentPart::Text { text } = part {
                    assert!(
                        t.content.contains(text.as_ref()),
                        "text part {text:?} missing from the content mirror {:?}",
                        t.content
                    );
                }
            }
        }
    }

    #[test]
    fn set_text_collapses_text_parts_and_keeps_images() {
        let ConversationItem::ToolResult(mut t) = ordered_tool_result("c1") else {
            unreachable!()
        };
        set_tool_result_text(&mut t, "replaced");
        assert_eq!(t.content.as_ref(), "replaced");
        assert_eq!(t.parts.len(), 2, "one text part + one image part");
        assert!(matches!(&t.parts[0], ContentPart::Text { text } if text.as_ref() == "replaced"));
        assert!(matches!(&t.parts[1], ContentPart::Image { .. }));
        assert_eq!(t.images.len(), 1);
        assert_mirrors_consistent(&ConversationItem::ToolResult(t));
    }

    #[test]
    fn set_text_on_legacy_item_touches_only_content() {
        let ConversationItem::ToolResult(mut t) =
            ConversationItem::tool_result_with_images("c1", "old", vec![image("img")])
        else {
            unreachable!()
        };
        set_tool_result_text(&mut t, "new");
        assert_eq!(t.content.as_ref(), "new");
        assert!(t.parts.is_empty(), "legacy items must stay off `parts`");
        assert_eq!(t.images.len(), 1);
    }

    #[test]
    fn remove_first_image_updates_both_representations() {
        let ConversationItem::ToolResult(mut t) = ordered_tool_result("c1") else {
            unreachable!()
        };
        let url = remove_first_tool_result_image(&mut t).expect("an image to remove");
        assert!(url.contains("EVICTME"));
        assert!(t.images.is_empty());
        assert!(
            t.parts
                .iter()
                .all(|p| matches!(p, ContentPart::Text { .. })),
            "the evicted image must leave `parts` too"
        );
        assert_eq!(remove_first_tool_result_image(&mut t), None);
    }

    #[test]
    fn retain_images_counts_once_across_representations() {
        let ConversationItem::ToolResult(mut t) = ConversationItem::tool_result_with_parts(
            "c1",
            vec![text("keep"), image("bad"), image("good")],
        ) else {
            unreachable!()
        };
        let removed = retain_tool_result_images(&mut t, |p| {
            !matches!(p, ContentPart::Image { url } if url.as_ref() == "bad")
        });
        assert_eq!(removed, 1);
        assert_eq!(t.images.len(), 1);
        assert_eq!(t.parts.len(), 2);
        assert_mirrors_consistent(&ConversationItem::ToolResult(t));
    }

    /// Wire-level: pruning a tool result must remove the pruned text from the
    /// ordered `parts`, not just the legacy `content` mirror.
    #[test]
    fn pruning_removes_text_from_the_wire_body() {
        let mut items = vec![
            ordered_tool_result("c1"),
            ConversationItem::user("turn 1"),
            ConversationItem::user("turn 2"),
        ];
        crate::actor::request_builder::prune_conversation(
            &mut items,
            &crate::types::PruningConfig {
                enabled: true,
                keep_last_n_turns: 0,
                hard_clear_age_turns: 1,
                ..Default::default()
            },
        );
        assert_mirrors_consistent(&items[0]);
        let body = wire_json(items);
        assert!(
            !body.contains("SECRET-BEFORE") && !body.contains("SECRET-AFTER"),
            "hard-cleared text leaked to the wire: {body}"
        );
        assert!(body.contains("[Tool result omitted"), "placeholder missing");
    }

    /// Wire-level: soft-trimmed text must not survive in `parts`.
    #[test]
    fn soft_trim_removes_middle_from_the_wire_body() {
        let long = format!("HEAD{}MIDDLE-SECRET{}TAIL", "x".repeat(50), "y".repeat(50));
        let mut items = vec![
            ConversationItem::tool_result_with_parts("c1", vec![text(&long), image("img")]),
            ConversationItem::user("turn 1"),
            ConversationItem::user("turn 2"),
        ];
        crate::actor::request_builder::prune_conversation(
            &mut items,
            &crate::types::PruningConfig {
                enabled: true,
                keep_last_n_turns: 0,
                hard_clear_age_turns: 99,
                soft_trim_threshold: 20,
                soft_trim_head: 5,
                soft_trim_tail: 5,
            },
        );
        assert_mirrors_consistent(&items[0]);
        let body = wire_json(items);
        assert!(
            !body.contains("MIDDLE-SECRET"),
            "soft-trimmed text leaked to the wire: {body}"
        );
        assert!(body.contains("img"), "the image part must survive the trim");
    }

    /// Wire-level: image-budget eviction must remove the image from the
    /// ordered `parts` so it cannot ride the wire past the budget.
    #[test]
    fn image_eviction_removes_image_from_the_wire_body() {
        let items = vec![ordered_tool_result("c1")];
        let budgeted = crate::image_budget::apply_image_budget_with_limits(items, 1, 0);
        assert_eq!(budgeted.outcome.evicted, 1);
        assert_mirrors_consistent(&budgeted.items[0]);
        let body = wire_json(budgeted.items);
        assert!(
            !body.contains("EVICTME"),
            "evicted image leaked to the wire: {body}"
        );
        assert!(
            body.contains("images from this tool result were removed"),
            "eviction note missing from the wire body: {body}"
        );
    }

    /// Wire-level: forked-session CWD rewriting must reach the text parts.
    #[test]
    fn cwd_rewrite_reaches_ordered_parts_on_the_wire() {
        let mut items = vec![ConversationItem::tool_result_with_parts(
            "c1",
            vec![text("wrote /old/cwd/file.rs"), image("img")],
        )];
        transform_conversation_cwd_synced(&mut items, "/old/cwd", "/new/cwd");
        assert_mirrors_consistent(&items[0]);
        let body = wire_json(items);
        assert!(
            !body.contains("/old/cwd"),
            "stale workspace path leaked to the wire: {body}"
        );
        assert!(body.contains("/new/cwd/file.rs"));
    }
}
