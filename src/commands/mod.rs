//! What each command does, as functions that return data. Nothing here
//! prints or reads the terminal: `main` hands the result to `ui::text`.

pub mod claude;
pub mod codex;
pub mod doctor;
pub mod rotate;
pub mod services;
pub mod view;

/// Epoch seconds now.
pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
