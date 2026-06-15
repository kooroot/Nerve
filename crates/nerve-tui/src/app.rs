//! Top-level `TuiApp` driver.
//!
//! This module exposes the public entry point used by `nerve-cli` to attach a
//! ratatui front-end to the running supervisor. [`TuiApp::run`] takes ownership
//! of three broadcast receivers (lead stdout, reviewer stdout, status state)
//! plus a shutdown watch channel, enters raw mode + the alternate screen, and
//! drives a `tokio::select!` render loop until either:
//!
//!   * the shutdown watch flips to `true`, or
//!   * the user presses `q`, `Esc`, or `Ctrl-C`.
//!
//! Terminal teardown is handled by a RAII guard so the host shell is always
//! restored even on panic.

use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use thiserror::Error;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

use crate::layout;
use crate::theme::Theme;
use crate::widgets::{self, StatusState};

/// Public alias used by callers that prefer the v0.5.0 "TuiState" name. The
/// underlying carrier remains [`StatusState`] so on-wire compatibility with
/// existing producers is preserved.
pub type TuiState = StatusState;

/// Hard cap on the number of log lines retained per pane (lead / reviewer).
/// The widget further tails this buffer to its own render cap.
pub const LOG_BUFFER_CAP: usize = 500;

/// Errors surfaced by the TUI driver.
#[derive(Debug, Error)]
pub enum TuiError {
    /// Terminal initialization (raw mode, alt screen) failed.
    #[error("terminal init failed: {0}")]
    Terminal(String),
    /// I/O error while drawing or polling input.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Runtime-tunable options for [`TuiApp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiAppOptions {
    /// Render tick interval in milliseconds.
    pub refresh_ms: u64,
    /// Percentage of the right column allocated to the reviewer log
    /// (the remainder is given to the status pane).
    pub log_height_pct: u8,
}

impl Default for TuiAppOptions {
    fn default() -> Self {
        Self {
            refresh_ms: 100,
            log_height_pct: 70,
        }
    }
}

/// Three-pane TUI driver. Holds rendering configuration; the actual loop is
/// driven by [`TuiApp::run`].
#[derive(Debug, Clone)]
pub struct TuiApp {
    opts: TuiAppOptions,
    theme: Theme,
}

impl TuiApp {
    /// Create a new app with the given options and the default theme.
    pub fn new(opts: TuiAppOptions) -> Self {
        Self {
            opts,
            theme: Theme::default(),
        }
    }

    /// Override the visual theme.
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Read-only accessor used by tests.
    pub fn options(&self) -> TuiAppOptions {
        self.opts
    }

    /// Render-loop entry point.
    ///
    /// Consumes its receivers so the caller cannot accidentally double-drive
    /// the loop. The shutdown watch channel allows the supervisor to ask the
    /// TUI to exit cleanly (e.g. when an RPC `session.ended` is observed).
    pub async fn run(
        self,
        mut state_rx: broadcast::Receiver<TuiState>,
        mut lead_rx: broadcast::Receiver<String>,
        mut reviewer_rx: broadcast::Receiver<String>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<(), TuiError> {
        // Fast-path: if shutdown is already signalled (e.g. caller flipped it
        // before invoking `run`), don't bother touching the terminal at all.
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        let mut guard = TerminalGuard::enter()?;
        let refresh = Duration::from_millis(self.opts.refresh_ms.max(16));

        let mut state: TuiState = TuiState::default();
        let mut lead_lines: VecDeque<String> = VecDeque::with_capacity(LOG_BUFFER_CAP);
        let mut reviewer_lines: VecDeque<String> = VecDeque::with_capacity(LOG_BUFFER_CAP);
        let mut lead_dropped: u64 = 0;
        let mut reviewer_dropped: u64 = 0;

        // Draw once immediately so the UI is visible before any events arrive.
        Self::draw(
            guard.terminal_mut(),
            &state,
            &lead_lines,
            &reviewer_lines,
            lead_dropped,
            reviewer_dropped,
            self.opts.log_height_pct,
            self.theme,
        )?;

        loop {
            tokio::select! {
                biased;

                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }

                msg = state_rx.recv() => match msg {
                    Ok(next) => state = next,
                    Err(RecvError::Lagged(_)) => {
                        // Skip ahead; the next successful recv carries the
                        // freshest state, which is all that matters here.
                    }
                    Err(RecvError::Closed) => break,
                },

                msg = lead_rx.recv() => match msg {
                    Ok(line) => push_capped(&mut lead_lines, line, LOG_BUFFER_CAP),
                    Err(RecvError::Lagged(n)) => lead_dropped = lead_dropped.saturating_add(n),
                    Err(RecvError::Closed) => break,
                },

                msg = reviewer_rx.recv() => match msg {
                    Ok(line) => push_capped(&mut reviewer_lines, line, LOG_BUFFER_CAP),
                    Err(RecvError::Lagged(n)) => {
                        reviewer_dropped = reviewer_dropped.saturating_add(n);
                    }
                    Err(RecvError::Closed) => break,
                },

                _ = tokio::time::sleep(refresh) => {
                    // Tick: drain any pending keyboard events and redraw.
                    if poll_for_quit()? {
                        break;
                    }
                    Self::draw(
                        guard.terminal_mut(),
                        &state,
                        &lead_lines,
                        &reviewer_lines,
                        lead_dropped,
                        reviewer_dropped,
                        self.opts.log_height_pct,
                        self.theme,
                    )?;
                }
            }
        }

        // Final paint so the user sees the terminal "settle" before we tear
        // down. Errors here are non-fatal — the guard still restores the host
        // shell on drop.
        let _ = Self::draw(
            guard.terminal_mut(),
            &state,
            &lead_lines,
            &reviewer_lines,
            lead_dropped,
            reviewer_dropped,
            self.opts.log_height_pct,
            self.theme,
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        state: &TuiState,
        lead_lines: &VecDeque<String>,
        reviewer_lines: &VecDeque<String>,
        lead_dropped: u64,
        reviewer_dropped: u64,
        log_height_pct: u8,
        theme: Theme,
    ) -> Result<(), TuiError> {
        let lead_snapshot = snapshot_lines(lead_lines, lead_dropped, "lead");
        let reviewer_snapshot = snapshot_lines(reviewer_lines, reviewer_dropped, "reviewer");

        terminal.draw(|frame| {
            let panes = layout::split(frame.area(), log_height_pct);

            let lead = widgets::log_panel(
                lead_snapshot.title.as_str(),
                lead_snapshot.lines.as_slice(),
                theme,
            );
            let reviewer = widgets::log_panel(
                reviewer_snapshot.title.as_str(),
                reviewer_snapshot.lines.as_slice(),
                theme,
            );
            let status = widgets::status_panel(state, theme);

            frame.render_widget(lead, panes.lead);
            frame.render_widget(reviewer, panes.reviewer);
            frame.render_widget(status, panes.status);
        })?;

        Ok(())
    }
}

/// Push a line into a bounded ring buffer, dropping the oldest entry when at
/// capacity. Exposed at module scope so the cap-behaviour can be unit-tested
/// without spinning up a terminal.
pub(crate) fn push_capped(buf: &mut VecDeque<String>, line: String, cap: usize) {
    if cap == 0 {
        return;
    }
    if buf.len() == cap {
        buf.pop_front();
    }
    buf.push_back(line);
}

/// Bundle of strings handed to a `Paragraph` widget. We materialise the title
/// (which embeds the dropped-line counter) and copy the buffered lines into a
/// contiguous `Vec` because `widgets::log_panel` takes a `&[String]`.
struct LogSnapshot {
    title: String,
    lines: Vec<String>,
}

fn snapshot_lines(buf: &VecDeque<String>, dropped: u64, base: &str) -> LogSnapshot {
    let title = if dropped == 0 {
        base.to_string()
    } else {
        format!("{base} (-{dropped})")
    };
    LogSnapshot {
        title,
        lines: buf.iter().cloned().collect(),
    }
}

/// Non-blocking peek for a quit keystroke. Returns `Ok(true)` if the user
/// asked to exit; never blocks waiting for input.
fn poll_for_quit() -> Result<bool, TuiError> {
    while event::poll(Duration::ZERO).map_err(io_to_tui)? {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read().map_err(io_to_tui)?
            && is_quit_key(code, modifiers)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_quit_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c'))
}

fn io_to_tui(err: io::Error) -> TuiError {
    TuiError::Io(err)
}

/// RAII guard that owns the configured terminal handle and restores cooked
/// mode + the host screen buffer on drop, even when the render loop panics.
struct TerminalGuard {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    raw_mode_active: bool,
    alt_screen_active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        enable_raw_mode().map_err(|e| TuiError::Terminal(e.to_string()))?;

        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            // Roll back the raw-mode toggle so the host shell is left as found.
            let _ = disable_raw_mode();
            return Err(TuiError::Terminal(e.to_string()));
        }
        stdout.flush().ok();

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).map_err(|e| TuiError::Terminal(e.to_string()))?;

        Ok(Self {
            terminal: Some(terminal),
            raw_mode_active: true,
            alt_screen_active: true,
        })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        // Safe: `terminal` is `Some` from construction until `drop`.
        self.terminal.as_mut().expect("terminal taken before drop")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Try to show the cursor again on the way out; ignore failures.
        if let Some(mut terminal) = self.terminal.take() {
            let _ = terminal.show_cursor();
        }

        if self.alt_screen_active {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            self.alt_screen_active = false;
        }
        if self.raw_mode_active {
            let _ = disable_raw_mode();
            self.raw_mode_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_sane() {
        let opts = TuiAppOptions::default();
        assert!(opts.refresh_ms > 0);
        assert!(opts.log_height_pct >= 10 && opts.log_height_pct <= 90);
    }

    #[test]
    fn new_preserves_options() {
        let opts = TuiAppOptions {
            refresh_ms: 250,
            log_height_pct: 60,
        };
        let app = TuiApp::new(opts);
        assert_eq!(app.options(), opts);
    }

    #[test]
    fn log_buffer_caps_at_500() {
        let mut buf: VecDeque<String> = VecDeque::with_capacity(LOG_BUFFER_CAP);
        for i in 0..1000u32 {
            push_capped(&mut buf, format!("line {i}"), LOG_BUFFER_CAP);
        }
        assert_eq!(buf.len(), LOG_BUFFER_CAP);
        // Oldest retained line is the (1000 - 500)th one we pushed.
        assert_eq!(buf.front().map(String::as_str), Some("line 500"));
        assert_eq!(buf.back().map(String::as_str), Some("line 999"));
    }

    #[test]
    fn push_capped_handles_zero_cap() {
        let mut buf: VecDeque<String> = VecDeque::new();
        push_capped(&mut buf, "ignored".into(), 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn snapshot_lines_appends_drop_counter() {
        let mut buf: VecDeque<String> = VecDeque::new();
        push_capped(&mut buf, "a".into(), 4);
        let snap = snapshot_lines(&buf, 0, "lead");
        assert_eq!(snap.title, "lead");
        assert_eq!(snap.lines, vec!["a".to_string()]);

        let lagged = snapshot_lines(&buf, 7, "lead");
        assert_eq!(lagged.title, "lead (-7)");
    }

    #[test]
    fn quit_key_detection_matches_expected_combos() {
        assert!(is_quit_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(is_quit_key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(is_quit_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!is_quit_key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!is_quit_key(KeyCode::Char('Q'), KeyModifiers::SHIFT));
    }

    #[test]
    fn terminal_guard_drops_safely_when_empty() {
        // Construct a guard-shaped value WITHOUT touching the real terminal
        // (the test harness has no TTY). Dropping it must not panic and must
        // not attempt to restore raw mode it never enabled.
        let guard = TerminalGuard {
            terminal: None,
            raw_mode_active: false,
            alt_screen_active: false,
        };
        drop(guard);
    }

    #[tokio::test]
    async fn run_returns_immediately_when_shutdown_preset() {
        let (_state_tx, state_rx) = broadcast::channel::<TuiState>(8);
        let (_lead_tx, lead_rx) = broadcast::channel::<String>(8);
        let (_reviewer_tx, reviewer_rx) = broadcast::channel::<String>(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(true);

        let app = TuiApp::new(TuiAppOptions::default());
        // Pre-set shutdown means `run` skips terminal init entirely, so this
        // is safe to call without a TTY attached.
        let result = app.run(state_rx, lead_rx, reviewer_rx, shutdown_rx).await;
        assert!(result.is_ok(), "run() returned {result:?}");
    }
}
