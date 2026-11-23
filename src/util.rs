//! Small formatting helpers.

/// `1234567` -> `1,234,567`.
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        out.insert(0, '-');
    }
    out
}

/// Collapse whitespace and cut to at most `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Cut to at most `max` characters by dropping the middle, so that both the
/// start (what it is) and the end (often a file name) stay readable.
pub fn squeeze(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let kept = max.saturating_sub(1);
    let head = kept.div_ceil(2);
    let tail = &chars[chars.len() - (kept - head)..];
    chars[..head].iter().chain(['…'].iter()).chain(tail).collect()
}

/// Cut an item description to `max` characters. One that ends in a path
/// keeps its end, where the file name is; anything else keeps its start.
pub fn brief(text: &str, max: usize) -> String {
    let ends_in_path = text.split_whitespace().next_back().is_some_and(|word| word.contains('/'));
    if ends_in_path {
        squeeze(text, max)
    } else {
        one_line(text, max)
    }
}

/// Break text into lines of at most `width` characters, at a space where
/// the line has one in its second half. Tabs become spaces and other control
/// characters are dropped, so that the result is safe to put on a terminal.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let mut chars: Vec<char> = Vec::with_capacity(line.len());
        for c in line.chars() {
            match c {
                '\t' => chars.extend([' '; 4]),
                c if c.is_control() => {}
                c => chars.push(c),
            }
        }
        let mut rest = &chars[..];
        while rest.len() > width {
            let space = rest[..width].iter().rposition(|&c| c == ' ').filter(|&at| at >= width / 2);
            let cut = space.map_or(width, |at| at + 1);
            lines.push(rest[..cut].iter().collect::<String>().trim_end().to_string());
            rest = &rest[cut..];
        }
        lines.push(rest.iter().collect());
    }
    lines
}

/// A duration in milliseconds as `42s`, `7m`, `3h` or `2d`.
pub fn age(millis: u64) -> String {
    match millis / 1000 {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squeeze_keeps_both_ends() {
        assert_eq!(squeeze("short", 10), "short");
        assert_eq!(squeeze("call to Write: ~/project/src/context.rs", 24), "call to Writ…/context.rs");
        assert_eq!(squeeze("abcdef", 1), "…");
        assert_eq!(squeeze("abcdef", 0), "…");
        assert_eq!(squeeze("éèàùç", 4), "éè…ç");
    }

    #[test]
    fn brief_cuts_where_the_least_is_lost() {
        assert_eq!(brief("call to Write: ~/project/src/context.rs", 24), "call to Writ…/context.rs");
        assert_eq!(brief("call to Bash: cd ~/project && cargo test --all", 24), "call to Bash: cd ~/proj…");
        assert_eq!(brief("user prompt: hi", 24), "user prompt: hi");
    }

    #[test]
    fn wrap_breaks_at_spaces_and_cleans_control_characters() {
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(wrap("a\n\nb", 9), ["a", "", "b"]);
        // No usable space: cut hard.
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("x verylongword", 8), ["x verylo", "ngword"]);
        assert_eq!(wrap("\tred\u{1b}[0m\r", 20), ["    red[0m"]);
        assert_eq!(wrap("", 5), [""]);
        assert_eq!(wrap("abc", 0), ["a", "b", "c"]);
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        assert_eq!(age(999), "0s");
        assert_eq!(age(59_000), "59s");
        assert_eq!(age(61_000), "1m");
        assert_eq!(age(2 * 3_600_000), "2h");
        assert_eq!(age(3 * 86_400_000), "3d");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(-45678), "-45,678");
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("abcdefgh", 5), "abcd…");
    }
}
