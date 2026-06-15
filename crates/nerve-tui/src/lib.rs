//! Terminal UI (ratatui) front-end for the Nerve runtime.
//!
//! This crate provides the v0.5.0 Tier 3g three-pane TUI:
//!  - left  : lead agent stdout log
//!  - top-right : reviewer agent stdout log
//!  - bottom-right : status panel (rounds, budget, goal-check state)
//!
//! The TUI subscribes to broadcast channels emitted by the RPC event bus and
//! renders incremental updates without blocking the runtime.

pub mod app;
mod layout;
mod theme;
mod widgets;

pub use app::{LOG_BUFFER_CAP, TuiApp, TuiAppOptions, TuiError, TuiState};
pub use theme::Theme;
pub use widgets::StatusState;
