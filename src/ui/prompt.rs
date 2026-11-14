//! Everything that reads the terminal: confirmations, the hook's stdin, and
//! the browser login, which is a conversation.

use std::io::{BufRead, IsTerminal, Read};

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

/// Interactive prompts need both a keyboard and a terminal for their output.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[derive(Debug)]
pub struct ResetOption {
    pub account: String,
    pub grant: Option<String>,
    label: String,
    details: String,
}

fn reset_options(listings: &[claude::ResetListing], now: i64) -> Vec<ResetOption> {
    use crate::anthropic::resets;
    let mut options = Vec::new();
    for listing in listings {
        let Some(Ok(status)) = &listing.status else {
            continue;
        };
        if let Some(program) = &status.cedar_ember {
            if program.standing() == resets::Standing::Eligible {
                let mut grants = program.grants();
                grants.sort_by_key(|grant| program.next_grant_id.as_deref() != Some(&grant.id));
                for grant in grants {
                    if !grant.is_usable(now) || !grant.usable_now {
                        continue;
                    }
                    if listing
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.item.as_deref() != Some(&grant.id))
                    {
                        continue;
                    }
                    let label = if grant.label.is_empty() {
                        &grant.id
                    } else {
                        &grant.label
                    };
                    let mut details = vec![label.clone()];
                    details.extend(resets::grant_effects(&grant));
                    details.push(format!("Resets left: {}", grant.resets_left));
                    if let Some(end) = &grant.ends_at {
                        details.push(format!("Expires: {}", end.get(..10).unwrap_or(end)));
                    }
                    if listing.pending.is_some() {
                        details
                            .push("Resends the previous request whose outcome is unknown.".into());
                    }
                    options.push(ResetOption {
                        account: listing.account.clone(),
                        label: format!("{} · {label}", listing.account),
                        details: details.join("\n"),
                        grant: Some(grant.id),
                    });
                }
            }
        }
        if listing.pending.is_none()
            && status
                .juniper_tide
                .as_ref()
                .is_some_and(|session| session.available)
        {
            options.push(ResetOption {
                account: listing.account.clone(),
                grant: None,
                label: format!("{} · Session reset", listing.account),
                details:
                    "Refills the 5-hour allowance.\nUsage still counts toward the weekly limit."
                        .into(),
            });
        }
    }
    options
}

/// Pick an available reset. Enter only selects; the caller confirms and rechecks it.
pub fn choose_reset(listings: &[claude::ResetListing]) -> Result<Option<ResetOption>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::prelude::*;
    use ratatui::widgets::*;
    let mut options = reset_options(listings, chrono::Utc::now().timestamp());
    if options.is_empty() {
        crate::ui::text::print_reset_offers(listings);
        println!("No resets available to use now.");
        return Ok(None);
    }
    let _session = super::tui::TerminalSession::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default().with_selected(Some(0));
    loop {
        terminal.draw(|frame| {
            let areas = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(10),
                Constraint::Length(1),
            ])
            .split(frame.area());
            let items = options
                .iter()
                .map(|option| ListItem::new(option.label.as_str()));
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::bordered().title("Choose a Claude reset"))
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                areas[0],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new(options[state.selected().unwrap_or(0)].details.as_str())
                    .block(Block::bordered().title("Effect"))
                    .wrap(Wrap { trim: false }),
                areas[1],
            );
            frame.render_widget(
                Paragraph::new("↑/↓ choose · Enter review · Esc cancel"),
                areas[2],
            );
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let selected = state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Down | KeyCode::Char('j') => {
                state.select(Some((selected + 1) % options.len()))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.select(Some((selected + options.len() - 1) % options.len()))
            }
            KeyCode::Home => state.select(Some(0)),
            KeyCode::End => state.select(Some(options.len() - 1)),
            KeyCode::Enter => return Ok(Some(options.remove(selected))),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picker_filters_unavailable_offers_and_respects_pending_claims() {
        let grant = |id: &str, left: u32, usable: bool| {
            json!({
                "id": id, "resets_left": left, "usable_now": usable,
                "clears": ["seven_day"], "percent_used": {"seven_day": 60}
            })
        };
        let mut listing = claude::ResetListing {
            account: "work".into(),
            pending: None,
            status: Some(Ok(serde_json::from_value(json!({
                "cedar_ember": {"eligible": true, "next_grant_id": "recommended",
                    "grants": [grant("other", 1, true), grant("spent", 0, true),
                        grant("blocked", 1, false), grant("recommended", 1, true)]},
                "juniper_tide": {"eligible": true, "available": true}
            }))
            .unwrap())),
        };
        let options = reset_options(&[listing.clone()], 0);
        assert_eq!(
            options
                .iter()
                .map(|option| option.grant.as_deref())
                .collect::<Vec<_>>(),
            [Some("recommended"), Some("other"), None]
        );
        assert!(options[0].details.contains("60% → 0%"));
        listing.pending = Some(crate::claims::Pending {
            service: crate::quota::Service::Claude,
            account: "work".into(),
            item: Some("other".into()),
            request_id: "request".into(),
            sent_at: 0,
        });
        let options = reset_options(&[listing], 0);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].grant.as_deref(), Some("other"));
        assert!(options[0].details.contains("previous request"));
    }
}
