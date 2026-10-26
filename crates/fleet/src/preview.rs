//! A one-line preview of an agent's reply, as rows and notifications show
//! it: the first line with real words, outside code blocks, with the
//! Markdown taken out.

use serde_json::Value;

/// Characters a preview keeps at most.
const MAX: usize = 200;

/// The newest assistant reply in a timeline page's entries, with its time,
/// as a tail read ([`crate::requests::last_reply`]) returns them.
pub fn newest_reply(page: &Value) -> Option<(String, String)> {
    page["entries"]
        .as_array()?
        .iter()
        .rev()
        .find(|entry| {
            entry["item"]["type"] == "assistant_message"
                && entry["item"]["text"]
                    .as_str()
                    .is_some_and(|t| !t.trim().is_empty())
        })
        .map(|entry| {
            (
                entry["item"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                entry["timestamp"].as_str().unwrap_or_default().to_string(),
            )
        })
}

/// The first line of `text` worth reading, as plain text.
pub fn first_line(text: &str) -> String {
    let mut in_fence = false;
    let mut code: Option<&str> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            // A reply that is only code previews its first code line.
            if code.is_none() && !line.is_empty() {
                code = Some(line);
            }
            continue;
        }
        if is_furniture(line) {
            continue;
        }
        let plain = plain(line);
        if plain.chars().any(char::is_alphanumeric) {
            return clip(&plain);
        }
    }
    code.map(clip).unwrap_or_default()
}

/// Lines that are only layout: empty, rules, bare bullets, table rules.
fn is_furniture(line: &str) -> bool {
    if line.is_empty() {
        return true;
    }
    let marks = |set: &[char]| line.chars().all(|c| set.contains(&c) || c == ' ');
    // `---`, `***`, `___`, `===` (a setext underline), and the like.
    if line.len() >= 3 && (marks(&['-']) || marks(&['*']) || marks(&['_']) || marks(&['='])) {
        return true;
    }
    // `|---|:---:|`
    if line.starts_with('|') && marks(&['|', '-', ':']) {
        return true;
    }
    matches!(line, "-" | "*" | "+" | ">" | "#")
}

/// One line without its Markdown: heading marks, quote marks, bullets
/// and numbers, task boxes, emphasis, code ticks, and link syntax.
fn plain(line: &str) -> String {
    let mut line = line.trim_start_matches('#').trim_start();
    while let Some(rest) = line.strip_prefix('>') {
        line = rest.trim_start();
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            line = rest.trim_start();
            break;
        }
    }
    // "1. " or "1) "
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits < 4 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            line = rest.trim_start();
        }
    }
    for task in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = line.strip_prefix(task) {
            line = rest;
        }
    }
    let unlinked = links(line);
    let mut out = String::with_capacity(unlinked.len());
    let chars: Vec<char> = unlinked.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        match c {
            '*' | '`' | '~' => {}
            // An underscore marks emphasis only next to a word's edge;
            // snake_case keeps its own.
            '_' => {
                let before = i.checked_sub(1).map(|j| chars[j]);
                let after = chars.get(i + 1).copied();
                let inside = before.is_some_and(char::is_alphanumeric)
                    && after.is_some_and(char::is_alphanumeric);
                if inside {
                    out.push('_');
                }
            }
            c => out.push(*c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `[text](url)` and `![alt](url)` become their text.
fn links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else {
            break;
        };
        let before = rest[..open].strip_suffix('!').unwrap_or(&rest[..open]);
        out.push_str(before);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn clip(line: &str) -> String {
    let mut out: String = line.chars().take(MAX).collect();
    if line.chars().count() > MAX {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_newest_reply_with_words_is_the_one() {
        let page = json!({"entries": [
            {"item": {"type": "assistant_message", "text": "First."}, "timestamp": "t1"},
            {"item": {"type": "assistant_message", "text": "Second."}, "timestamp": "t2"},
            {"item": {"type": "assistant_message", "text": "  "}, "timestamp": "t3"},
            {"item": {"type": "tool_call"}, "timestamp": "t4"},
        ]});
        assert_eq!(
            newest_reply(&page),
            Some(("Second.".to_string(), "t2".to_string()))
        );
        assert_eq!(newest_reply(&json!({"entries": []})), None);
    }

    #[test]
    fn markdown_is_taken_out() {
        assert_eq!(
            first_line("**candidate #5** looks `best`, see [the notes](https://example.com/n)."),
            "candidate #5 looks best, see the notes."
        );
        assert_eq!(first_line("## Done\n\nAll tests pass."), "Done");
        assert_eq!(first_line("- [x] _shipped_ the fix"), "shipped the fix");
        assert_eq!(
            first_line("1. Read the snake_case code"),
            "Read the snake_case code"
        );
        assert_eq!(first_line("> quoted ~~old~~ text"), "quoted old text");
        assert_eq!(first_line("![diagram](a.png) below"), "diagram below");
    }

    #[test]
    fn furniture_is_skipped_for_the_first_line_with_words() {
        assert_eq!(first_line("---\n\nThe real summary."), "The real summary.");
        assert_eq!(
            first_line("***\n-\n|---|---|\n# \nWords here"),
            "Words here"
        );
        assert_eq!(
            first_line("```rust\nfn main() {}\n```\nIt compiles now."),
            "It compiles now."
        );
        assert_eq!(first_line("```\nonly_code();\n```"), "only_code();");
        assert_eq!(first_line("\n\n"), "");
        assert_eq!(first_line(&"x".repeat(300)).chars().count(), 201);
    }
}
