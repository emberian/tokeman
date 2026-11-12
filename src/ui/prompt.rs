//! Everything that reads the terminal: confirmations, the hook's stdin, and
//! the browser login, which is a conversation.

use std::io::{BufRead, Read};

use anyhow::Result;

use crate::account::Fleet;
use crate::commands::claude;
use crate::ui::text;

/// Ask a yes/no question; anything but `y` is no.
pub fn confirm(question: &str) -> Result<bool> {
    text::print_question(&format!("{question} [y/N] "));
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

/// Everything on stdin (the SessionStart hook's JSON).
pub fn stdin_bytes() -> Result<Vec<u8>> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    Ok(input)
}

/// Walk the browser login for one account, or for every account that still
/// lacks a refreshable login. Each round: open a URL, paste what the page
/// shows, move on. `skip` passes, `q` stops. An unconfigured name enrolls a
/// login-only account.
pub async fn login(name: Option<&str>, more: bool) -> Result<()> {
    let fleet = Fleet::load()?;
    let targets = claude::login_targets(&fleet, name, more);
    if targets.is_empty() {
        text::print_login_none_needed();
        return Ok(());
    }
    let total = targets.len();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut done = 0usize;
    for (index, account) in targets.iter().enumerate() {
        let pkce = claude::begin_login(account)?;
        text::print_login_step(
            index,
            total,
            account,
            fleet.account(account).is_some(),
            &pkce.url,
        );
        let Some(line) = lines.next() else { break };
        let line = line?;
        let answer = line.trim();
        if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
            break;
        }
        if answer.is_empty() || answer.eq_ignore_ascii_case("skip") {
            text::print_login_skipped(account);
            continue;
        }
        let outcome = claude::finish_login(account, answer, &pkce).await;
        if matches!(outcome, Ok(claude::LoginOutcome::Stored { .. })) {
            done += 1;
        }
        text::print_login_outcome(account, &outcome);
    }
    text::print_login_summary(done, total);
    Ok(())
}
