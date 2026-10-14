//! Every surface: text output, charts, the TUI, the tray.

pub mod chart;
pub mod text;
#[cfg(feature = "tray")]
pub mod tray;
pub mod tui;
