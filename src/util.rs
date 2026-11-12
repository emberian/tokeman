//! Small pure helpers that more than one domain needs. Nothing here does IO.

/// Collapse whitespace and cap at `max_chars`, marking a cut with `…`.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max_chars {
        single
    } else {
        let cut: String = single.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

/// `value` as one word of a POSIX shell command line.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// An RFC 3339 reset time as epoch seconds; 0 (no reset pending) when the
/// server sent none or one that does not parse.
pub fn parse_reset(resets_at: Option<&str>) -> i64 {
    resets_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map_or(0, |time| time.timestamp())
}

/// `first` seconds, doubling with each consecutive failure after the first,
/// capped at `cap`.
pub fn backoff_secs(first: i64, cap: i64, consecutive: u32) -> i64 {
    let doublings = consecutive.saturating_sub(1).min(16);
    first.saturating_mul(1 << doublings).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_flattens_and_caps_on_characters() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("  a\n  b\t c ", 10), "a b c");
        assert_eq!(one_line("ééééé", 3), "ééé…");
        assert_eq!(one_line("💥💥💥", 2), "💥💥…");
    }

    #[test]
    fn a_quoted_word_survives_an_apostrophe() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn a_missing_or_unreadable_reset_is_none_pending() {
        assert_eq!(parse_reset(Some("1970-01-01T00:01:00Z")), 60);
        assert_eq!(parse_reset(Some("soon")), 0);
        assert_eq!(parse_reset(None), 0);
    }

    #[test]
    fn backoff_doubles_from_the_first_failure_to_the_cap() {
        let steps: Vec<i64> = (0..6).map(|n| backoff_secs(300, 1800, n)).collect();
        assert_eq!(steps, [300, 300, 600, 1200, 1800, 1800]);
        assert_eq!(backoff_secs(600, 3600, 40), 3600);
    }
}
