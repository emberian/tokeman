//! Every surface: text output, charts, the TUI, the tray.

pub mod chart;
pub mod overview;
pub mod prompt;
pub mod text;
#[cfg(feature = "tray")]
pub mod tray;
pub mod tui;

/// The colour a level is drawn in, from a surface's own `[ok, low, critical]`
/// palette: the one place that says which level is which colour.
pub fn level_color<C: Copy>(level: crate::quota::Level, palette: [C; 3]) -> C {
    match level {
        crate::quota::Level::Ok => palette[0],
        crate::quota::Level::Low => palette[1],
        crate::quota::Level::Critical => palette[2],
    }
}
