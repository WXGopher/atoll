//! Small helpers shared by every mode of the binary.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// The user's home directory, resolved at runtime. Never a compiled-in path.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Cut `text` to `limit` characters, marking the cut with an ellipsis.
///
/// Counts characters rather than bytes so a path with non-ASCII in it neither
/// panics nor gets cut mid-codepoint.
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// Collapse whitespace runs (newlines included) into single spaces, so a
/// multi-line tool input still renders as one line.
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(ch);
    }
    out
}

/// Turn a chunk of an agent's prose into something that reads as a label.
///
/// The last thing an assistant said is the best short description of what a
/// session is doing, and it is also Markdown written for a terminal: bold runs,
/// headings, back-ticked paths, numbered lists, em-dash asides. Rendered as a
/// one-line title those marks are noise — `安排好了： 1. **悬浮球 v3 暂停**——已做的进度`
/// is a real title this produced, and every character of syntax in it is
/// spending width the actual words needed.
///
/// So: strip the marks, keep the words, collapse to one line. Deliberately not
/// a Markdown parser — this runs over text that is only *mostly* Markdown, and
/// the failure mode of a parser on prose is worse than a few marks surviving.
pub fn clean_title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    // Leading list markers are dropped once, at the front, where they are
    // numbering rather than content. `1.` inside a sentence is a number.
    let mut at_start = true;

    while let Some(character) = chars.next() {
        match character {
            // Emphasis, headings, back-ticked code, block quotes, rules: the
            // marks themselves carry nothing once the line is one line.
            '*' | '_' | '`' | '#' | '~' | '>' => continue,
            // A bullet at the very front is a list marker; anywhere else it is
            // a hyphen doing its job.
            '-' if at_start => continue,
            // Link syntax: `[text](url)` keeps the text and drops the target.
            '[' | ']' => continue,
            '(' if out.ends_with(char::is_alphanumeric) && looks_like_url(&mut chars) => {
                for skipped in chars.by_ref() {
                    if skipped == ')' {
                        break;
                    }
                }
                continue;
            }
            // Digits at the very front, followed by a list marker's dot or
            // bracket, are numbering.
            digit if at_start && digit.is_ascii_digit() => {
                let mut lookahead = chars.clone();
                if matches!(lookahead.next(), Some('.') | Some(')'))
                    && lookahead.next().is_some_and(char::is_whitespace)
                {
                    chars.next();
                    continue;
                }
                at_start = false;
                out.push(digit);
            }
            other => {
                if !other.is_whitespace() {
                    at_start = false;
                }
                out.push(other);
            }
        }
    }

    one_line(&out)
        .trim_start_matches(is_dangling)
        .trim_end_matches(is_dangling_at_the_end)
        .to_string()
}

/// Whether what follows an opening bracket looks like a link target rather than
/// a parenthetical. Peeks without consuming.
fn looks_like_url(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let rest: String = chars.clone().take(8).collect();
    rest.starts_with("http") || rest.starts_with("www.") || rest.starts_with('/')
}

/// Punctuation a stripped title should not begin with: a full stop left by a
/// dropped list number, a colon, a dangling dash.
fn is_dangling(character: char) -> bool {
    matches!(
        character,
        ' ' | ':' | '：' | '-' | '–' | '—' | '·' | '.' | '。' | ',' | '，'
    )
}

/// And what it should not *end* with. A shorter list: a title that ends in a
/// full stop ends in a full stop because the sentence did, and taking it away
/// makes the label read as truncated rather than as tidy. A colon is different
/// — it was introducing a list that is no longer there.
fn is_dangling_at_the_end(character: char) -> bool {
    matches!(
        character,
        ' ' | ':' | '：' | '-' | '–' | '—' | '·' | ',' | '，'
    )
}

/// The last path component of `cwd` — the short name a human calls the project.
pub fn project_name(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// `HH:MM:SS`, UTC.
///
/// TODO(M4): show local time. Doing that needs either a date-time dependency or
/// a `GetLocalTime` call, and neither is worth it for a bring-up log.
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        day / 3_600,
        (day % 3_600) / 60,
        day % 60
    )
}

/// Append one line to `debug.log` in the config directory, silently.
///
/// For faults that happen where no console exists to print to — a window that
/// failed to show, a renderer that failed to come up. Best-effort by design:
/// logging must never add a failure of its own to the one it is recording.
pub fn debug_log(message: &str) {
    let Ok(dir) = crate::app::config::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("debug.log"))
    {
        use std::io::Write;
        let _ = writeln!(file, "{} {}", timestamp(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 10), "abcdefghij");
        assert_eq!(truncate("abcdefghijk", 10), "abcdefghi…");
        // A multi-byte string must not be cut mid-codepoint.
        assert_eq!(truncate("项目目录名称很长啊", 4), "项目目…");
    }

    #[test]
    fn one_line_flattens_whitespace() {
        assert_eq!(one_line("git status\n  --short"), "git status --short");
        assert_eq!(one_line("  padded  "), "padded");
        assert_eq!(one_line(""), "");
    }

    /// The one that prompted this: a real title, with every Markdown mark the
    /// terminal renders and the panel cannot.
    #[test]
    fn a_title_keeps_the_words_and_loses_the_markup() {
        assert_eq!(
            clean_title("安排好了： 1. **悬浮球 v3 暂停**——已做的进度"),
            "安排好了： 1. 悬浮球 v3 暂停——已做的进度"
        );
        assert_eq!(
            clean_title("**Done.** Wired up the parser."),
            "Done. Wired up the parser."
        );
        assert_eq!(clean_title("## Heading"), "Heading");
        assert_eq!(clean_title("`cargo test` is green"), "cargo test is green");
        assert_eq!(clean_title("> quoted advice"), "quoted advice");
        assert_eq!(clean_title("~~struck~~ through"), "struck through");
    }

    /// A leading list marker is numbering; the same characters mid-sentence are
    /// content and have to survive.
    #[test]
    fn only_a_leading_list_marker_is_numbering() {
        assert_eq!(clean_title("1. First step"), "First step");
        assert_eq!(clean_title("2) Second step"), "Second step");
        assert_eq!(clean_title("- a bullet"), "a bullet");
        assert_eq!(clean_title("  - indented bullet"), "indented bullet");
        // Not numbering: a version, a count, a range.
        assert_eq!(clean_title("3 tests failed"), "3 tests failed");
        assert_eq!(clean_title("v1.2 shipped"), "v1.2 shipped");
        assert_eq!(clean_title("well-formed input"), "well-formed input");
    }

    #[test]
    fn a_title_comes_back_as_one_line_without_dangling_punctuation() {
        assert_eq!(
            clean_title("Fixed it.\n\n  - one\n  - two"),
            "Fixed it. - one - two"
        );
        // A colon that introduced a list nobody can see any more.
        assert_eq!(clean_title("**Next:**"), "Next");
        assert_eq!(clean_title("看这里：\n"), "看这里");
        assert_eq!(clean_title(""), "");
        assert_eq!(clean_title("***"), "");
    }

    #[test]
    fn a_link_keeps_its_words_and_drops_its_target() {
        assert_eq!(
            clean_title("see [the docs](https://example.invalid/x) for more"),
            "see the docs for more"
        );
        // A parenthetical is not a link and must survive intact.
        assert_eq!(
            clean_title("done (finally) after three tries"),
            "done (finally) after three tries"
        );
    }

    #[test]
    fn the_project_name_is_the_last_path_component() {
        assert_eq!(project_name(r"C:\synthetic\project"), "project");
        assert_eq!(project_name(r"C:\synthetic\project\"), "project");
        assert_eq!(project_name("/home/synthetic/work"), "work");
        assert_eq!(project_name("bare"), "bare");
        assert_eq!(project_name(""), "");
    }
}
