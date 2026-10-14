//! tokeman keeps a fleet of Anthropic subscription accounts and makes sure the
//! one credential slot Claude Code reads always holds an account with room.
//! See DESIGN.md for the shape; this crate is that document, in code.

// launchd and the Keychain are macOS-only; their helpers are unreachable
// elsewhere. Dead code is still reported on macOS, where everything is live.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

pub mod account;
pub mod anthropic;
pub mod claude;
pub mod cli;
pub mod codex;
pub mod daemon;
pub mod fs;
pub mod history;
pub mod launch;
pub mod observe;
pub mod paths;
pub mod policy;
pub mod quota;
pub mod settings;
pub mod store;
pub mod ui;
