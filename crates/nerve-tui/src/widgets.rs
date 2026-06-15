//! Reusable widgets for the three-pane TUI.
//!
//! Both panes are bordered blocks with a coloured title. The log panel
//! enforces a hard cap of 200 lines (oldest dropped first) so the UI never
//! grows unbounded — the source of truth for the full transcript stays in the
//! runtime, the TUI only mirrors a tail.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::theme::Theme;

/// Maximum number of lines retained in any log panel.
#[allow(dead_code)] // consumed by the render loop in the next phase
pub const LOG_PANEL_CAP: usize = 200;

/// Snapshot of status pane data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusState {
    /// Current round counter (1-based, 0 when idle).
    pub round: u32,
    /// Total rounds budgeted for the goal.
    pub max_rounds: u32,
    /// Tokens consumed so far.
    pub tokens_used: u64,
    /// Token budget ceiling.
    pub token_budget: u64,
    /// Most recent goal-check verdict (free-form, e.g. "PASS" / "FAIL").
    pub goal_check: Option<String>,
    /// Latest one-line status message (e.g. "applying patch").
    pub note: Option<String>,
}

/// Build a bordered log panel widget for `lines`.
///
/// If `lines` exceeds [`LOG_PANEL_CAP`], only the most recent
/// `LOG_PANEL_CAP` entries are rendered.
#[allow(dead_code)] // consumed by the render loop in the next phase
pub fn log_panel<'a>(title: &'a str, lines: &'a [String], theme: Theme) -> Paragraph<'a> {
    let start = lines.len().saturating_sub(LOG_PANEL_CAP);
    let tail = &lines[start..];

    let body: Vec<Line> = tail
        .iter()
        .map(|s| Line::from(Span::styled(s.as_str(), theme.text)))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(Span::styled(title, theme.title));

    Paragraph::new(body).block(block).wrap(Wrap { trim: false })
}

/// Build the status pane widget from a [`StatusState`] snapshot.
#[allow(dead_code)] // consumed by the render loop in the next phase
pub fn status_panel<'a>(state: &'a StatusState, theme: Theme) -> Paragraph<'a> {
    let rounds_line = Line::from(vec![
        Span::styled("round  ", theme.muted),
        Span::styled(format!("{}/{}", state.round, state.max_rounds), theme.text),
    ]);

    let budget_style = if state.token_budget > 0 && state.tokens_used * 10 >= state.token_budget * 9
    {
        theme.warn
    } else {
        theme.text
    };
    let budget_line = Line::from(vec![
        Span::styled("tokens ", theme.muted),
        Span::styled(
            format!("{}/{}", state.tokens_used, state.token_budget),
            budget_style,
        ),
    ]);

    let goal_line = match state.goal_check.as_deref() {
        Some(verdict) => {
            let style = match verdict {
                v if v.eq_ignore_ascii_case("pass") => theme.success,
                v if v.eq_ignore_ascii_case("fail") => theme.error,
                _ => theme.text,
            };
            Line::from(vec![
                Span::styled("goal   ", theme.muted),
                Span::styled(verdict, style),
            ])
        }
        None => Line::from(vec![
            Span::styled("goal   ", theme.muted),
            Span::styled("(pending)", theme.muted),
        ]),
    };

    let mut body = vec![rounds_line, budget_line, goal_line];
    if let Some(note) = state.note.as_deref() {
        body.push(Line::from(Span::styled(note, theme.muted)));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(Span::styled("status", theme.title));

    Paragraph::new(body).block(block).wrap(Wrap { trim: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_panel_cap_constant_is_200() {
        assert_eq!(LOG_PANEL_CAP, 200);
    }

    #[test]
    fn log_panel_accepts_empty_lines() {
        let theme = Theme::default();
        let _w = log_panel("lead", &[], theme);
    }

    #[test]
    fn status_panel_renders_default_state() {
        let theme = Theme::default();
        let state = StatusState::default();
        let _w = status_panel(&state, theme);
    }
}
