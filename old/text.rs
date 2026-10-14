//! Small string shaping shared by logs and terminal output.

/// Collapse whitespace (newlines included) to single spaces and cap the result
/// at `max_chars` characters, marking a cut with `…`. For error bodies that
/// would otherwise sprawl across a log or a status line.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= max_chars {
        single_line
    } else {
        let cut: String = single_line.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::one_line;

    #[test]
    fn flattens_and_caps_on_characters() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("💥💥💥", 2), "💥💥…");
    }
}
