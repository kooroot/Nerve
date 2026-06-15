//! Three-pane layout helpers.
//!
//! Splits the terminal viewport into left (lead log), top-right (reviewer
//! log) and bottom-right (status) regions. The vertical split between the
//! reviewer log and status is parameterised by `log_height_pct`, which is the
//! percentage of the right column reserved for the reviewer log.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Rectangles produced by [`split`].
#[allow(dead_code)] // wired up by the render loop in the next phase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRects {
    /// Left pane: lead agent log.
    pub lead: Rect,
    /// Top-right pane: reviewer agent log.
    pub reviewer: Rect,
    /// Bottom-right pane: status panel.
    pub status: Rect,
}

/// Split `area` into the three logical panes.
///
/// `log_height_pct` is clamped to `[10, 90]` to avoid degenerate splits.
#[allow(dead_code)] // wired up by the render loop in the next phase
pub fn split(area: Rect, log_height_pct: u8) -> PaneRects {
    let clamped = log_height_pct.clamp(10, 90);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(clamped as u16),
            Constraint::Percentage((100 - clamped) as u16),
        ])
        .split(columns[1]);

    PaneRects {
        lead: columns[0],
        reviewer: right_rows[0],
        status: right_rows[1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_produces_three_areas() {
        let area = Rect::new(0, 0, 120, 40);
        let panes = split(area, 70);

        // Left half spans the full height.
        assert_eq!(panes.lead.x, 0);
        assert_eq!(panes.lead.y, 0);
        assert_eq!(panes.lead.height, area.height);
        assert!(panes.lead.width >= 59 && panes.lead.width <= 61);

        // Reviewer + status stack on the right and together cover full height.
        assert_eq!(panes.reviewer.x, panes.lead.x + panes.lead.width);
        assert_eq!(panes.status.x, panes.reviewer.x);
        assert_eq!(panes.reviewer.y, 0);
        assert_eq!(panes.status.y, panes.reviewer.y + panes.reviewer.height);
        assert_eq!(panes.reviewer.height + panes.status.height, area.height);

        // All three rectangles are non-empty.
        assert!(panes.lead.width > 0 && panes.lead.height > 0);
        assert!(panes.reviewer.width > 0 && panes.reviewer.height > 0);
        assert!(panes.status.width > 0 && panes.status.height > 0);
    }

    #[test]
    fn split_clamps_extreme_percentages() {
        let area = Rect::new(0, 0, 80, 20);

        let low = split(area, 0);
        assert!(low.reviewer.height > 0);
        assert!(low.status.height > 0);

        let high = split(area, 100);
        assert!(high.reviewer.height > 0);
        assert!(high.status.height > 0);
    }
}
