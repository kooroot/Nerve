//! Color palette for the three-pane TUI.
//!
//! Themes are plain `ratatui::style::Style` containers so widgets can apply
//! them with a single call. All defaults aim for a dark terminal background.

use ratatui::style::{Color, Modifier, Style};

/// Visual style bundle shared by every pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Border lines around panes.
    pub border: Style,
    /// Pane titles (top-of-block text).
    pub title: Style,
    /// Default text inside log/status panes.
    pub text: Style,
    /// De-emphasised metadata (timestamps, counters).
    pub muted: Style,
    /// Success markers (goal met, patch applied).
    pub success: Style,
    /// Warning markers (budget threshold, retries).
    pub warn: Style,
    /// Error markers (RPC failure, refusal).
    pub error: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border: Style::default().fg(Color::DarkGray),
            title: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            text: Style::default().fg(Color::Gray),
            muted: Style::default().fg(Color::DarkGray),
            success: Style::default().fg(Color::Green),
            warn: Style::default().fg(Color::Yellow),
            error: Style::default().fg(Color::Red),
        }
    }
}
