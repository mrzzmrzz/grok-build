//! Session announcement banner for critical operational notices.
//!
//! Critical layout (always 2 rows when shown):
//! ```text
//! ! Title                                  [hide]   (prefix+title error red, [hide] dim + clickable)
//!   Message…  hide: /announcements hide            (message default fg, CTA dim gray)
//! ```
//!
//! The message row indents past the `! ` prefix so its column matches the
//! title's; the CTA keeps its full reserved width and the message truncates.
//!
//! Promotional announcements remain in remote state for compatibility and
//! diagnostics, but are deliberately not selected or rendered by this module.
//!
//! `dismissible: false` suppresses every hide affordance on either kind and
//! the text reclaims the reserved columns (absent/`true` = hideable).

use std::collections::BTreeSet;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
};

use crate::render::line_utils::truncate_str;
use crate::theme::Theme;
use xai_grok_announcements::visible_announcements;

const HIDE_CTA: &str = "hide: /announcements hide";
/// Clickable hide button, far right of the title row.
const HIDE_BUTTON: &str = "[hide]";
/// Alert prefix on the title row; the message row indents by its width.
const TITLE_PREFIX: &str = "! ";
/// Columns between title/message text and the right-hand button/CTA.
const GAP: usize = 2;

fn is_critical(a: &xai_grok_announcements::RemoteAnnouncement) -> bool {
    a.severity.as_deref() == Some("critical")
}

/// The ONE visibility predicate for passive announcement surfaces:
/// critical severity, non-empty (trimmed) message, not expired, and not
/// hidden (with the non-dismissible exception in [`is_hidden`]). Every seam
/// that picks or shows an announcement for passive UI — the session-banner
/// selection, the random pick at startup, the re-pick on a settings push,
/// and the welcome hero fallback — filters through this, so no path can
/// drift and redisplay a promotional, hidden, expired, or empty notice.
pub(crate) fn is_displayable_announcement(
    a: &xai_grok_announcements::RemoteAnnouncement,
    hidden_ids: &BTreeSet<String>,
) -> bool {
    is_displayable_announcement_at(a, hidden_ids, chrono::Utc::now())
}

/// [`is_displayable_announcement`] with an injectable clock.
fn is_displayable_announcement_at(
    a: &xai_grok_announcements::RemoteAnnouncement,
    hidden_ids: &BTreeSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    has_visible_message(a) && is_live_critical(a, now) && !is_hidden(a, hidden_ids)
}

/// Non-empty trimmed message — the same "visible" meaning as the
/// announcements crate's `visible_announcements` list filter.
fn has_visible_message(a: &xai_grok_announcements::RemoteAnnouncement) -> bool {
    a.message.as_ref().is_some_and(|m| !m.trim().is_empty())
}

/// One definition of "live critical" (visible message + critical + not expired)
/// shared by every predicate below so the meanings cannot drift.
fn is_live_critical(
    a: &xai_grok_announcements::RemoteAnnouncement,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    is_critical(a) && !xai_grok_announcements::is_expired_at(a, now)
}

/// Hideable unless the server says otherwise: absent/`true` = dismissible
/// (back-compat with every pre-flag announcement), only an explicit `false`
/// pins the banner. Shared by the selection seam, both painters, and the
/// hide dispatch so the meanings cannot drift.
pub fn is_dismissible(a: &xai_grok_announcements::RemoteAnnouncement) -> bool {
    a.dismissible != Some(false)
}

/// The hidden-ids filter the selection gates share. It applies only to
/// dismissible items: an explicit `dismissible: false` stays selectable even
/// with its hide key stored, so flipping the flag server-side resurrects a
/// previously-hidden banner (the remote config stays source of truth).
fn is_hidden(
    a: &xai_grok_announcements::RemoteAnnouncement,
    hidden_ids: &BTreeSet<String>,
) -> bool {
    is_dismissible(a) && hidden_ids.contains(&xai_grok_announcements::announcement_hide_key(a))
}

/// Wall-clock [`first_critical_session_announcement_at`] — test convenience.
#[cfg(test)]
fn first_critical_session_announcement<'a>(
    announcements: &'a [xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
) -> Option<&'a xai_grok_announcements::RemoteAnnouncement> {
    first_critical_session_announcement_at(announcements, hidden_ids, chrono::Utc::now())
}

/// The critical the session banner shows: first live critical whose hide key
/// is NOT in `hidden_ids` — hiding the first critical reveals the next
/// unhidden one. Info/warning stay welcome-only and do not open the
/// in-session slot. Private: prod consumers go through
/// [`first_session_announcement`]'s `.or_else` leg so slot precedence is
/// structurally enforced. Selection is the shared
/// [`is_displayable_announcement_at`] predicate, so it skips expired items
/// at selection (draw) time — an `expires_at` crossed mid-session stops
/// rendering before the next server push; the per-call timestamp parse and
/// hide-key build are allocation-light and the gate runs at most a few
/// times per frame over a tiny list, so no caching is needed.
fn first_critical_session_announcement_at<'a>(
    announcements: &'a [xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<&'a xai_grok_announcements::RemoteAnnouncement> {
    announcements
        .iter()
        .find(|a| is_displayable_announcement_at(a, hidden_ids, now))
}

/// The single banner-slot item: the first live, unhidden critical notice.
/// Promotional announcements are intentionally excluded from all passive UI.
pub fn first_session_announcement<'a>(
    announcements: &'a [xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
) -> Option<&'a xai_grok_announcements::RemoteAnnouncement> {
    first_session_announcement_at(announcements, hidden_ids, chrono::Utc::now())
}

/// Whether a live critical session announcement exists. Critical notices are
/// operational state and remain available while promotional notices stay out
/// of the passive banner path.
pub fn has_critical_session_announcement(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
) -> bool {
    first_critical_session_announcement_at(announcements, hidden_ids, chrono::Utc::now()).is_some()
}

/// [`first_session_announcement`] with an injectable clock.
pub fn first_session_announcement_at<'a>(
    announcements: &'a [xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<&'a xai_grok_announcements::RemoteAnnouncement> {
    first_critical_session_announcement_at(announcements, hidden_ids, now)
}

/// Hide keys of every live (non-expired) critical notice. Promotional remote
/// state is not exposed through the passive announcement controls.
pub fn session_announcement_hide_keys(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
) -> Vec<String> {
    session_announcement_hide_keys_at(announcements, chrono::Utc::now())
}

/// [`session_announcement_hide_keys`] with an injectable clock.
pub fn session_announcement_hide_keys_at(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    visible_announcements(announcements)
        .into_iter()
        .filter(|a| is_live_critical(a, now))
        .map(xai_grok_announcements::announcement_hide_key)
        .collect()
}

/// Slash-gate predicate for live critical notices. This keeps operational
/// `/announcements hide|show` controls while preventing promo-only state from
/// creating a passive command/banner entry point.
pub fn has_session_announcements(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
) -> bool {
    let now = chrono::Utc::now();
    visible_announcements(announcements)
        .into_iter()
        .any(|a| is_live_critical(a, now))
}

/// Height for the session banner: 0 when no critical is selected, otherwise
/// two rows for the title and message.
pub fn session_banner_height(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
) -> u16 {
    match first_session_announcement(announcements, hidden_ids) {
        Some(_) => 2,
        None => 0,
    }
}

/// Clickable rects painted by [`render_banner`] (`None` = not painted).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BannerHits {
    /// The `[hide]` button.
    pub hide: Option<Rect>,
}

/// Shared dim style for the hide affordances (CTA text and resting button).
fn dim_hide_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.gray)
        .bg(theme.bg_base)
        .add_modifier(Modifier::DIM)
}

/// Right-aligned `[hide]` button on `row`, painted FIRST so its width is
/// reserved before any text budget is computed (text truncates, never the
/// button). Hover mirrors the turn-status [stop] affordance: error red on
/// hover, dim at rest. Returns the clickable rect (`None` when it cannot
/// fit) — the one paint/hover/reserve rule both banner painters share.
fn paint_hide_button(
    buf: &mut Buffer,
    area: Rect,
    row: u16,
    hovered: bool,
    theme: &Theme,
) -> Option<Rect> {
    use unicode_width::UnicodeWidthStr;

    let max_w = area.width as usize;
    let button_w = UnicodeWidthStr::width(HIDE_BUTTON);
    if max_w < button_w {
        return None;
    }
    let hide_x = area.x + (max_w - button_w) as u16;
    let button_style = if hovered {
        Style::default().fg(theme.accent_error).bg(theme.bg_base)
    } else {
        dim_hide_style(theme)
    };
    buf.set_span(
        hide_x,
        row,
        &Span::styled(HIDE_BUTTON, button_style),
        button_w as u16,
    );
    Some(Rect::new(hide_x, row, button_w as u16, 1))
}

/// Session top banner: paints the [`first_session_announcement`] selection
/// (slot precedence lives there alone) with the critical-notice painter.
///
/// Returns the painted clickable rects so the caller can hit-test mouse
/// clicks against them.
pub fn render_banner(
    area: Rect,
    buf: &mut Buffer,
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
    hidden_ids: &BTreeSet<String>,
    hide_hovered: bool,
) -> BannerHits {
    if area.height == 0 || area.width == 0 {
        return BannerHits::default();
    }
    match first_session_announcement(announcements, hidden_ids) {
        Some(a) => render_critical_rows(area, buf, a, hide_hovered),
        None => BannerHits::default(),
    }
}

/// The selected critical announcement, two lines.
///
/// Row 0: `! Title` (error red, title bold) with a right-aligned dim
/// `[hide]` button; row 1: the message (default fg) indented to the title
/// column, then the dim `hide: /announcements hide` CTA. The CTA width is
/// reserved up front so a long message truncates with `…` instead of pushing
/// the CTA off-screen. A non-dismissible announcement paints neither hide
/// affordance and the title/message reclaim the reserved widths.
fn render_critical_rows(
    area: Rect,
    buf: &mut Buffer,
    ann: &xai_grok_announcements::RemoteAnnouncement,
    hide_hovered: bool,
) -> BannerHits {
    use unicode_width::UnicodeWidthStr;

    let theme = Theme::current();
    buf.set_style(area, Style::default().bg(theme.bg_base));

    let title = ann
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let message = ann
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    if title.is_none() && message.is_none() {
        return BannerHits::default();
    }

    // Semantic error red (theme-aware) rather than a raw palette color.
    let alert_fg = theme.accent_error;
    // One visual unit: the `! ` prefix and the title share this style.
    let alert_style = Style::default()
        .fg(alert_fg)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);
    let dim_style = dim_hide_style(&theme);
    let max_w = area.width as usize;
    let prefix_w = UnicodeWidthStr::width(TITLE_PREFIX);
    let button_w = UnicodeWidthStr::width(HIDE_BUTTON);
    let row0 = area.y;
    let row1 = area.y.saturating_add(1);
    let max_y = area.y.saturating_add(area.height);
    let mut hide_rect = None;

    let dismissible = is_dismissible(ann);

    if row0 < max_y {
        // `! ` prefix anchors the alert even when the title is missing.
        let prefix_disp = truncate_str(TITLE_PREFIX, max_w);
        buf.set_span(
            area.x,
            row0,
            &Span::styled(prefix_disp, alert_style),
            area.width,
        );

        // Non-dismissible: no [hide] button — the no-button budget branch
        // below hands its columns back to the title.
        if dismissible {
            hide_rect = paint_hide_button(buf, area, row0, hide_hovered, &theme);
        }

        let title_budget = if hide_rect.is_some() {
            max_w.saturating_sub(prefix_w + button_w + GAP)
        } else {
            max_w.saturating_sub(prefix_w)
        };
        if let Some(t) = title
            && title_budget > 0
        {
            let t_disp = truncate_str(t, title_budget);
            buf.set_span(
                area.x + prefix_w as u16,
                row0,
                &Span::styled(t_disp, alert_style),
                title_budget as u16,
            );
        }
    }

    if row1 < max_y {
        // Message column == title column: indent past the `! ` prefix.
        let mut x = area.x.saturating_add(prefix_w as u16);
        let mut remaining = max_w.saturating_sub(prefix_w);
        let cta_w = UnicodeWidthStr::width(HIDE_CTA);

        // Reserve the CTA (plus gap) up front: the message truncates, never
        // the CTA. Non-dismissible reserves nothing — the message reclaims
        // the full row past the prefix (`W−2`).
        let msg_budget = if dismissible {
            remaining.saturating_sub(cta_w + GAP)
        } else {
            remaining
        };
        if let Some(m) = message
            && msg_budget > 0
        {
            let msg_style = Style::default().fg(theme.text_primary).bg(theme.bg_base);
            let m_disp = truncate_str(m, msg_budget);
            let m_w = UnicodeWidthStr::width(m_disp.as_str()).min(msg_budget);
            if m_w > 0 {
                buf.set_span(x, row1, &Span::styled(m_disp, msg_style), m_w as u16);
                // dismissible: m_w <= msg_budget keeps the gap + full CTA
                // fitting after it; non-dismissible paints no CTA below.
                x = x.saturating_add((m_w + GAP) as u16);
                remaining = remaining.saturating_sub(m_w + GAP);
            }
        }
        if dismissible && remaining > 0 {
            // Degenerate widths still truncate the CTA itself rather than panic.
            let cta_disp = truncate_str(HIDE_CTA, remaining);
            buf.set_span(
                x,
                row1,
                &Span::styled(cta_disp, dim_style),
                remaining as u16,
            );
        }
    }

    BannerHits { hide: hide_rect }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_announcements::RemoteAnnouncement;

    fn ann(severity: Option<&str>, message: Option<&str>) -> RemoteAnnouncement {
        RemoteAnnouncement {
            severity: severity.map(str::to_string),
            message: message.map(str::to_string),
            ..Default::default()
        }
    }

    fn no_hidden() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn promo(id: &str, message: &str, cta: Option<(&str, &str)>) -> RemoteAnnouncement {
        RemoteAnnouncement {
            id: Some(id.into()),
            severity: Some("promo".into()),
            message: Some(message.into()),
            cta: cta.map(|(label, url)| xai_grok_announcements::AnnouncementCta {
                label: Some(label.into()),
                url: Some(url.into()),
                caption: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn first_critical_session_announcement_skips_non_critical_and_empty() {
        let input = vec![
            ann(Some("info"), Some("info only")),
            ann(Some("warning"), Some("warn only")),
            ann(Some("critical"), None),
            ann(Some("critical"), Some("   ")),
            ann(Some("critical"), Some("crit one")),
            ann(Some("critical"), Some("crit two")),
            ann(None, Some("no severity")),
        ];
        let got =
            first_critical_session_announcement(&input, &no_hidden()).expect("critical present");
        assert_eq!(got.message.as_deref(), Some("crit one"));

        assert!(first_critical_session_announcement(&[], &no_hidden()).is_none());
        let no_critical = vec![
            ann(Some("info"), Some("hello")),
            ann(Some("critical"), Some("   ")),
        ];
        assert!(first_critical_session_announcement(&no_critical, &no_hidden()).is_none());
    }

    /// Hidden is a selection-level filter: hiding the first critical reveals
    /// the next unhidden one instead of closing the whole slot, while the
    /// slash-gate predicate keeps ignoring hidden so `show` stays reachable.
    #[test]
    fn first_critical_selection_skips_hidden_and_reveals_next() {
        let list = vec![
            RemoteAnnouncement {
                id: Some("a".into()),
                severity: Some("critical".into()),
                message: Some("A msg".into()),
                ..Default::default()
            },
            RemoteAnnouncement {
                id: Some("b".into()),
                severity: Some("critical".into()),
                message: Some("B msg".into()),
                ..Default::default()
            },
        ];

        let hide_a: BTreeSet<String> = ["a".to_string()].into_iter().collect();
        assert_eq!(
            first_critical_session_announcement(&list, &hide_a).and_then(|a| a.id.as_deref()),
            Some("b"),
            "hiding the first critical must reveal the next one"
        );
        assert_eq!(session_banner_height(&list, &hide_a), 2);

        let hide_both: BTreeSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        assert!(first_critical_session_announcement(&list, &hide_both).is_none());
        assert_eq!(session_banner_height(&list, &hide_both), 0);
        assert!(
            has_session_announcements(&list),
            "slash gate ignores hidden so /announcements show stays reachable"
        );
    }

    /// Draw-time expiry: the selection gate must skip a critical whose
    /// `expires_at` has passed even though it is still in the ingested list.
    #[test]
    fn first_critical_session_announcement_at_skips_expired() {
        let expiring = RemoteAnnouncement {
            severity: Some("critical".into()),
            message: Some("expiring".into()),
            expires_at: Some("2030-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        let evergreen = RemoteAnnouncement {
            severity: Some("critical".into()),
            message: Some("evergreen".into()),
            ..Default::default()
        };
        let list = vec![expiring, evergreen];
        let expiry = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let before = expiry - chrono::Duration::seconds(1);
        assert_eq!(
            first_critical_session_announcement_at(&list, &no_hidden(), before)
                .and_then(|a| a.message.as_deref()),
            Some("expiring")
        );
        assert_eq!(
            first_critical_session_announcement_at(&list, &no_hidden(), expiry)
                .and_then(|a| a.message.as_deref()),
            Some("evergreen"),
            "expired first critical must yield to the next live one"
        );

        let only_expired = vec![list[0].clone()];
        assert!(
            first_critical_session_announcement_at(&only_expired, &no_hidden(), expiry).is_none(),
            "all-expired list must close the banner slot"
        );
    }

    /// Show's clear set matches the selection's meaning of visible: live
    /// (non-expired) criticals only — expired keys and promos are not exposed.
    #[test]
    fn session_hide_keys_cover_live_criticals_and_promos_only() {
        let mut expired_promo = promo("promo-expired", "gone promo", None);
        expired_promo.expires_at = Some("2000-01-01T00:00:00Z".into());
        let list = vec![
            ann(Some("info"), Some("skip me")),
            RemoteAnnouncement {
                id: Some("crit-1".into()),
                severity: Some("critical".into()),
                message: Some("one".into()),
                ..Default::default()
            },
            RemoteAnnouncement {
                id: None,
                title: Some("T".into()),
                severity: Some("critical".into()),
                message: Some("two".into()),
                ..Default::default()
            },
            RemoteAnnouncement {
                id: Some("crit-expired".into()),
                severity: Some("critical".into()),
                message: Some("gone".into()),
                expires_at: Some("2000-01-01T00:00:00Z".into()),
                ..Default::default()
            },
            ann(Some("critical"), None), // no message → not visible
            promo("promo-1", "upsell", Some(("Go", "https://x.ai"))),
            expired_promo,
        ];
        let now = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let keys = session_announcement_hide_keys_at(&list, now);
        assert_eq!(
            keys,
            vec!["crit-1".to_string(), "content:T\u{1f}two".to_string(),],
            "expired keys and passive promo keys must not be cleared by show"
        );
    }

    #[test]
    fn session_banner_height_zero_for_unfiltered_info_only() {
        let info_only = vec![
            ann(Some("info"), Some("hello")),
            ann(Some("warning"), Some("careful")),
        ];
        assert_eq!(session_banner_height(&info_only, &no_hidden()), 0);
    }

    #[test]
    fn session_banner_height_is_two_for_critical() {
        let msg_only = vec![ann(Some("critical"), Some("outage"))];
        assert_eq!(session_banner_height(&msg_only, &no_hidden()), 2);
        let hide_it: BTreeSet<String> = msg_only
            .iter()
            .map(xai_grok_announcements::announcement_hide_key)
            .collect();
        assert_eq!(session_banner_height(&msg_only, &hide_it), 0);

        let with_title = vec![RemoteAnnouncement {
            severity: Some("critical".into()),
            title: Some("Outage".into()),
            message: Some("Do not deploy".into()),
            ..Default::default()
        }];
        assert_eq!(session_banner_height(&with_title, &no_hidden()), 2);
    }

    #[test]
    fn passive_promo_cta_is_not_selected_or_drawn() {
        let announcements = [promo(
            "promo",
            "Upgrade to SuperGrok",
            Some(("Upgrade", "https://grok.com/upgrade")),
        )];
        let area = Rect::new(0, 0, 60, 2);
        let mut buf = Buffer::empty(area);

        assert!(first_session_announcement(&announcements, &no_hidden()).is_none());
        assert_eq!(session_banner_height(&announcements, &no_hidden()), 0);
        assert!(!has_session_announcements(&announcements));

        // The selection-pool predicate (event_loop / settings-push random pick
        // and the welcome hero fallback all share it) must not pass a promo.
        assert!(
            !is_displayable_announcement(&announcements[0], &no_hidden()),
            "promo must not pass the selection predicate"
        );

        let hits = render_banner(area, &mut buf, &announcements, &no_hidden(), false);
        assert_eq!(hits, BannerHits::default());
        let rendered: String = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter_map(|pos| buf.cell(pos).map(|cell| cell.symbol().to_string()))
            .collect();
        assert!(
            !rendered.contains("Upgrade"),
            "passive promo leaked: {rendered:?}"
        );
    }

    /// The shared visibility predicate gates the random pick in `event_loop`,
    /// `pick_random_announcement` on a settings push, the session banner
    /// selection, and the welcome hero's `self.announcement` fallback. Any
    /// severity other than critical must be rejected here.
    #[test]
    fn is_displayable_announcement_permits_critical_only() {
        for severity in [Some("info"), Some("warning"), Some("promo"), None] {
            assert!(
                !is_displayable_announcement(&ann(severity, Some("msg")), &no_hidden()),
                "severity {severity:?} must not be displayable"
            );
        }
        assert!(is_displayable_announcement(
            &ann(Some("critical"), Some("outage")),
            &no_hidden()
        ));
        // Welcome-fallback shape: `.filter(is_displayable_announcement)` on a
        // stored announcement drops a promo and keeps a critical.
        let stored = Some(promo("p", "upsell", Some(("Go", "https://x.ai"))));
        assert!(
            stored
                .as_ref()
                .filter(|a| is_displayable_announcement(a, &no_hidden()))
                .is_none(),
            "welcome fallback must not render a non-critical announcement"
        );
        let stored = Some(ann(Some("critical"), Some("outage")));
        assert!(
            stored
                .as_ref()
                .filter(|a| is_displayable_announcement(a, &no_hidden()))
                .is_some()
        );
    }

    /// Hidden, expired, and empty-content criticals must fail the shared
    /// predicate too — the welcome hero fallback previously rechecked only
    /// severity, so a critical the primary selection correctly filtered
    /// could resurface through it.
    #[test]
    fn is_displayable_announcement_rejects_hidden_expired_and_empty() {
        // Hidden critical: filtered — the fallback cannot redisplay it.
        let crit = RemoteAnnouncement {
            id: Some("c".into()),
            severity: Some("critical".into()),
            message: Some("outage".into()),
            ..Default::default()
        };
        let hidden: BTreeSet<String> = ["c".to_string()].into_iter().collect();
        assert!(
            !is_displayable_announcement(&crit, &hidden),
            "hidden critical must not be displayable via any surface"
        );
        // ... except an explicit `dismissible: false`, matching the primary
        // selection's server-flag override of a stored hide key.
        let pinned = RemoteAnnouncement {
            dismissible: Some(false),
            ..crit.clone()
        };
        assert!(
            is_displayable_announcement(&pinned, &hidden),
            "non-dismissible critical ignores stored hide keys"
        );

        // Expired critical: filtered at check (draw) time even though it may
        // still sit in the stored pick or the ingested list.
        let expired = RemoteAnnouncement {
            severity: Some("critical".into()),
            message: Some("gone".into()),
            expires_at: Some("2000-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert!(
            !is_displayable_announcement(&expired, &no_hidden()),
            "expired critical must not be displayable"
        );

        // Empty / whitespace-only message: nothing to show.
        for message in [None, Some("   ")] {
            assert!(
                !is_displayable_announcement(&ann(Some("critical"), message), &no_hidden()),
                "empty-content critical must not be displayable (message={message:?})"
            );
        }
    }

    /// The banner selection and the fallback predicate are the SAME gate:
    /// whenever `first_session_announcement` skips an item (hidden, expired,
    /// promo, empty), the fallback predicate must skip it too — no
    /// announcement can be filtered by the primary path yet resurface
    /// through the welcome hero fallback.
    #[test]
    fn primary_selection_and_fallback_share_one_visibility_gate() {
        let now = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let hidden: BTreeSet<String> = ["hidden-crit".to_string()].into_iter().collect();
        let items = vec![
            RemoteAnnouncement {
                id: Some("hidden-crit".into()),
                severity: Some("critical".into()),
                message: Some("hidden".into()),
                ..Default::default()
            },
            RemoteAnnouncement {
                id: Some("expired-crit".into()),
                severity: Some("critical".into()),
                message: Some("expired".into()),
                expires_at: Some("2000-01-01T00:00:00Z".into()),
                ..Default::default()
            },
            promo("promo", "upsell", None),
            ann(Some("critical"), Some("   ")),
        ];
        for a in &items {
            assert!(
                !is_displayable_announcement_at(a, &hidden, now),
                "fallback predicate must reject {:?}",
                a.id
            );
        }
        assert!(
            first_session_announcement_at(&items, &hidden, now).is_none(),
            "primary selection rejects the same set"
        );

        let live = RemoteAnnouncement {
            id: Some("live".into()),
            severity: Some("critical".into()),
            message: Some("live outage".into()),
            ..Default::default()
        };
        assert!(is_displayable_announcement_at(&live, &hidden, now));
        let mut with_live = items.clone();
        with_live.push(live);
        assert_eq!(
            first_session_announcement_at(&with_live, &hidden, now).and_then(|a| a.id.as_deref()),
            Some("live"),
            "both gates admit the one live, unhidden critical"
        );
    }

    fn buf_row(buf: &Buffer, area: Rect, y: u16) -> String {
        (0..area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn render_banner_title_row_with_hide_button_message_row_with_cta() {
        let anns = [RemoteAnnouncement {
            severity: Some("critical".into()),
            title: Some("Outage".into()),
            message: Some("Do not deploy".into()),
            ..Default::default()
        }];
        let area = Rect::new(0, 0, 60, 2);
        let mut buf = Buffer::empty(area);
        let hits = render_banner(area, &mut buf, &anns, &no_hidden(), false);

        // Row 0: `! Title` left, `[hide]` right-aligned; row 1: message
        // indented to the title column, then the dim CTA after a gap.
        let row0 = buf_row(&buf, area, 0);
        assert!(row0.starts_with("! Outage"), "row0={row0:?}");
        assert!(row0.ends_with(HIDE_BUTTON), "row0={row0:?}");
        assert_eq!(
            buf_row(&buf, area, 1),
            "  Do not deploy  hide: /announcements hide"
        );
        assert_eq!(hits.hide, Some(Rect::new(54, 0, 6, 1)), "[hide] hit rect");
        for y in 0..2 {
            let r = buf_row(&buf, area, y);
            assert!(!r.contains('‼') && !r.contains('⚠') && !r.contains('ℹ'));
        }

        let theme = Theme::current();
        let prefix = buf.cell((0, 0)).unwrap();
        assert_eq!(prefix.fg, theme.accent_error, "! prefix uses error red");
        let title = buf.cell((2, 0)).unwrap();
        assert_eq!(title.fg, theme.accent_error, "title uses error red");
        assert!(title.modifier.contains(Modifier::BOLD));
        let button = buf.cell((54, 0)).unwrap();
        assert_eq!(button.fg, theme.gray);
        assert!(
            button.modifier.contains(Modifier::DIM),
            "[hide] dim at rest"
        );
        let msg = buf.cell((2, 1)).unwrap();
        assert_eq!(msg.fg, theme.text_primary, "message uses default fg");
        assert!(!msg.modifier.contains(Modifier::BOLD));
        let cta = buf.cell((17, 1)).unwrap();
        assert_eq!(cta.fg, theme.gray);
        assert!(cta.modifier.contains(Modifier::DIM), "CTA dim");
    }

    #[test]
    fn render_banner_hide_button_highlights_on_hover() {
        let anns = [ann(Some("critical"), Some("outage"))];
        let area = Rect::new(0, 0, 60, 2);
        let mut buf = Buffer::empty(area);
        let hits = render_banner(area, &mut buf, &anns, &no_hidden(), true);
        let rect = hits.hide.expect("hide button painted");
        let button = buf.cell((rect.x, rect.y)).unwrap();
        assert_eq!(button.fg, Theme::current().accent_error);
        assert!(!button.modifier.contains(Modifier::DIM));
    }

    /// Row-1 reservation: the CTA keeps its full width and the message is the
    /// part that truncates (with an ellipsis), never the other way around.
    #[test]
    fn render_banner_truncates_message_never_cta() {
        let anns = [RemoteAnnouncement {
            severity: Some("critical".into()),
            title: Some("Outage".into()),
            message: Some("0123456789ABCDEFGHIJ".into()),
            ..Default::default()
        }];
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        render_banner(area, &mut buf, &anns, &no_hidden(), false);
        // width 40 − 2 indent − (25 CTA + 2 gap) = 11 message columns.
        assert_eq!(
            buf_row(&buf, area, 1),
            "  0123456789…  hide: /announcements hide"
        );
    }

    #[test]
    fn render_banner_shows_first_critical_only() {
        let anns = [
            RemoteAnnouncement {
                severity: Some("info".into()),
                title: Some("Info".into()),
                message: Some("ignored".into()),
                ..Default::default()
            },
            RemoteAnnouncement {
                id: Some("first".into()),
                severity: Some("critical".into()),
                title: Some("First".into()),
                message: Some("one".into()),
                ..Default::default()
            },
            RemoteAnnouncement {
                severity: Some("critical".into()),
                title: Some("Second".into()),
                message: Some("two".into()),
                ..Default::default()
            },
        ];
        let area = Rect::new(0, 0, 40, 2);

        let mut buf = Buffer::empty(area);
        render_banner(area, &mut buf, &anns, &no_hidden(), false);
        let painted = buf_row(&buf, area, 0);
        assert!(painted.starts_with("! First"), "row0={painted:?}");
        assert!(!painted.contains("Second"));
        assert!(!painted.contains("Info"));

        // Hiding the painted critical must paint the next unhidden one.
        let hide_first: BTreeSet<String> = ["first".to_string()].into_iter().collect();
        let mut buf = Buffer::empty(area);
        render_banner(area, &mut buf, &anns, &hide_first, false);
        let painted = buf_row(&buf, area, 0);
        assert!(painted.starts_with("! Second"), "row0={painted:?}");
        assert!(!painted.contains("First"));
    }

    #[test]
    fn render_banner_ignores_info_only() {
        let anns = [ann(Some("info"), Some("hello"))];
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        let hits = render_banner(area, &mut buf, &anns, &no_hidden(), false);
        assert_eq!(hits, BannerHits::default(), "no banner, no hit rects");
        let any: String = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(!any.contains("hello"));
        assert!(!any.contains(HIDE_BUTTON));
    }

    /// The [hide] button width is reserved before the title budget, so a long
    /// title truncates with an ellipsis instead of overpainting the button.
    #[test]
    fn render_banner_long_title_truncates_before_hide_button() {
        let anns = [RemoteAnnouncement {
            severity: Some("critical".into()),
            title: Some("A".repeat(80)),
            message: Some("MSGBODY".into()),
            ..Default::default()
        }];
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        let hits = render_banner(area, &mut buf, &anns, &no_hidden(), false);
        let row0 = buf_row(&buf, area, 0);
        assert!(row0.starts_with("! AAA"), "row0={row0:?}");
        assert!(
            row0.contains('…'),
            "long title must show ellipsis; row0={row0:?}"
        );
        assert!(row0.ends_with(HIDE_BUTTON), "row0={row0:?}");
        assert_eq!(hits.hide, Some(Rect::new(34, 0, 6, 1)));
        assert!(
            buf_row(&buf, area, 1).contains("MSGBODY"),
            "long title must not drop the message row"
        );
    }

    /// The hidden-ids filter applies only to dismissible items: an explicit
    /// `dismissible: false` stays selectable with its hide key stored (a
    /// server-side flag flip resurrects a previously-hidden banner), while
    /// absent/`true` keep today's hidden behavior.
    #[test]
    fn non_dismissible_selected_despite_stored_hide_key() {
        let mut crit = RemoteAnnouncement {
            id: Some("c".into()),
            severity: Some("critical".into()),
            message: Some("pinned outage".into()),
            dismissible: Some(false),
            ..Default::default()
        };
        let hidden: BTreeSet<String> = ["c".to_string()].into_iter().collect();

        let list = vec![crit.clone()];
        assert_eq!(
            first_session_announcement(&list, &hidden).and_then(|a| a.id.as_deref()),
            Some("c"),
            "stored hide key must not filter a non-dismissible critical"
        );
        assert_eq!(session_banner_height(&list, &hidden), 2);

        // Back-compat: absent and explicit `true` still honor the hidden set.
        for dismissible in [None, Some(true)] {
            crit.dismissible = dismissible;
            assert!(
                first_session_announcement(&[crit.clone()], &hidden).is_none(),
                "dismissible={dismissible:?} must stay hidden"
            );
        }
    }

    /// Non-dismissible critical: neither hide affordance paints and the
    /// title/message reclaim the reserved widths (title `W−2`, message `W−2`
    /// vs the dismissible `W−2−6−2` / `W−2−27`).
    #[test]
    fn render_critical_rows_non_dismissible_reclaims_hide_columns() {
        let anns = [RemoteAnnouncement {
            severity: Some("critical".into()),
            title: Some("T".repeat(50)),
            message: Some("0123456789ABCDEFGHIJ".into()),
            dismissible: Some(false),
            ..Default::default()
        }];
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        let hits = render_banner(area, &mut buf, &anns, &no_hidden(), false);

        assert_eq!(hits.hide, None, "no [hide] target on a pinned banner");
        // Title budget 40−2=38: 37 chars + ellipsis fill to the right edge.
        let row0 = buf_row(&buf, area, 0);
        assert_eq!(row0, format!("! {}…", "T".repeat(37)));
        assert!(!row0.contains(HIDE_BUTTON), "row0={row0:?}");
        // Message budget 40−2=38: the 20-char message fits whole, no hide CTA
        // (the dismissible twin truncates it to 11 columns at this width).
        assert_eq!(buf_row(&buf, area, 1), "  0123456789ABCDEFGHIJ");
    }
}
