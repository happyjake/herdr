//! What sits in an agent's composer, read from its styled screen.
//!
//! A Debrief types its invocation into the composer, so it must first know the
//! composer is empty: typing onto a person's unsent draft would change their words,
//! and typing onto an earlier, untaken invocation stacks a second one behind it.
//! Each supported agent draws its composer in a recognisable place. What counts
//! as typed text is what the screen draws in normal intensity there: placeholders
//! are drawn faint, and an empty editor's cursor is an inverse blank.

/// The composer's typed text with whitespace runs collapsed: `Some("")` when it
/// is empty, `None` when this screen shows no composer this reader recognises.
pub(super) fn draft(agent: &str, screen: &str) -> Option<String> {
    let lines: Vec<Vec<Cell>> = screen.split('\n').map(cells).collect();
    let text = match agent {
        "codex" => {
            let start = lines
                .iter()
                .rposition(|line| plain(line).trim_start().starts_with('›'))?;
            let mut body = vec![typed(&lines[start])];
            for line in &lines[start + 1..] {
                if plain(line).trim().is_empty() {
                    break;
                }
                body.push(typed(line));
            }
            strip_marker(&body.join(" "), &['›'])?
        }
        "claude" | "pi" => {
            let rules: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| is_rule(line))
                .map(|(index, _)| index)
                .collect();
            let [.., top, bottom] = rules[..] else {
                return None;
            };
            let body = lines[top + 1..bottom]
                .iter()
                .map(|line| typed(line))
                .collect::<Vec<_>>()
                .join(" ");
            if agent == "claude" {
                strip_marker(&body, &['❯', '>'])?
            } else {
                body
            }
        }
        _ => return None,
    };
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// True when the draft is nothing but the invocation typed one or more times:
/// text the desk itself typed in an earlier, untaken attempt. Agents join the
/// repeats with or without a space, so whitespace is ignored on both sides.
pub(super) fn is_own(draft: &str, invocation: &str) -> bool {
    let draft: String = draft.chars().filter(|c| !c.is_whitespace()).collect();
    let invocation: String = invocation.chars().filter(|c| !c.is_whitespace()).collect();
    !invocation.is_empty()
        && !draft.is_empty()
        && draft.len() % invocation.len() == 0
        && draft == invocation.repeat(draft.len() / invocation.len())
}

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    faint: bool,
    inverse: bool,
}

fn cells(line: &str) -> Vec<Cell> {
    let mut out = Vec::new();
    let (mut faint, mut inverse) = (false, false);
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(Cell { ch, faint, inverse });
            continue;
        }
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut params = String::new();
        let mut command = None;
        for next in chars.by_ref() {
            if next.is_ascii_alphabetic() || next == '~' {
                command = Some(next);
                break;
            }
            params.push(next);
        }
        if command != Some('m') {
            continue;
        }
        let params: Vec<&str> = params.split([';', ':']).collect();
        let mut index = 0;
        while index < params.len() {
            match params[index] {
                "" | "0" => (faint, inverse) = (false, false),
                "2" => faint = true,
                "22" => faint = false,
                "7" => inverse = true,
                "27" => inverse = false,
                "38" | "48" | "58" => {
                    index += match params.get(index + 1) {
                        Some(&"5") => 2,
                        Some(&"2") => 4,
                        _ => 0,
                    };
                }
                _ => {}
            }
            index += 1;
        }
    }
    out
}

fn plain(line: &[Cell]) -> String {
    line.iter().map(|cell| cell.ch).collect()
}

/// Normal-intensity text: drops faint placeholders and an inverse blank cursor.
fn typed(line: &[Cell]) -> String {
    line.iter()
        .filter(|cell| !cell.faint && !(cell.inverse && cell.ch == ' '))
        .map(|cell| if cell.ch == '\u{a0}' { ' ' } else { cell.ch })
        .collect()
}

/// A composer border: a line that opens with a long horizontal rule (Claude's
/// lower border can carry a scroll hint or usage after the rule begins).
fn is_rule(line: &[Cell]) -> bool {
    plain(line).trim_start().starts_with(&"─".repeat(10))
}

fn strip_marker(text: &str, markers: &[char]) -> Option<String> {
    let text = text.trim_start();
    let first = text.chars().next()?;
    markers
        .contains(&first)
        .then(|| text[first.len_utf8()..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::{draft, is_own};

    const RULE: &str = "\x1b[0m\x1b[38;2;136;136;136m────────────────────────────────────────\x1b[0m";

    fn claude(composer: &str) -> String {
        format!(
            "⏺ Done.\n\n{RULE}\n{composer}\n{RULE}\n  \x1b[0m\x1b[1m\x1b[38;5;14m~/code/app\x1b[0m · main\n  ⏵⏵ auto mode on"
        )
    }

    fn codex(composer: &str) -> String {
        format!(
            "• Finished.\n\n\x1b[0m\x1b[2m  Worked for 2m 28s\x1b[0m\n \n{composer}\n \n  \x1b[0m\x1b[38;2;171;223;167m~/code/app\x1b[0m · main · Context 10% used"
        )
    }

    fn pi(composer: &str) -> String {
        let rule = "\x1b[0m\x1b[38;2;151;118;229m────────────────────────────────────────\x1b[0m";
        format!(" \x1b[0m\x1b[38;2;157;165;169mDone.\x1b[0m\n\n{rule}\n{composer}\n{rule}\n~/code/app · 9% · worker · $0.00")
    }

    #[test]
    fn empty_composers_read_empty_through_their_placeholders_and_cursors() {
        assert_eq!(draft("claude", &claude("❯\u{a0}")).as_deref(), Some(""));
        assert_eq!(
            draft("claude", &claude("❯ \x1b[0m\x1b[2mTry \"fix lint errors\"\x1b[0m")).as_deref(),
            Some("")
        );
        assert_eq!(
            draft(
                "codex",
                &codex("\x1b[0m\x1b[1m›\x1b[0m \x1b[0m\x1b[2mAsk Codex to do anything\x1b[0m")
            )
            .as_deref(),
            Some("")
        );
        assert_eq!(
            draft("pi", &pi("\x1b[0m\x1b[7m \x1b[0m                    ")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn a_persons_draft_reads_as_its_text_on_one_or_more_lines() {
        assert_eq!(
            draft("claude", &claude("❯ please check the build")).as_deref(),
            Some("please check the build")
        );
        assert_eq!(
            draft(
                "codex",
                &codex("\x1b[0m\x1b[1m›\x1b[0m first line of a note\n  second line here")
            )
            .as_deref(),
            Some("first line of a note second line here")
        );
        assert_eq!(
            draft("pi", &pi("please check the build\x1b[7m \x1b[0m")).as_deref(),
            Some("please check the build")
        );
    }

    #[test]
    fn claudes_lower_border_may_carry_text_after_the_rule() {
        let screen = format!(
            "{RULE}\n❯\u{a0}\n──────────────────── ↓157───7% · Opus · $21.36──────\n  ~/code/app · main"
        );
        assert_eq!(draft("claude", &screen).as_deref(), Some(""));
    }

    #[test]
    fn a_screen_without_a_recognisable_composer_is_unreadable() {
        assert_eq!(draft("claude", "⏺ Working…\n"), None);
        assert_eq!(draft("codex", "loading…\n"), None);
        assert_eq!(draft("pi", "just text\n"), None);
        assert_eq!(draft("agy", &codex("› hi")), None);
    }

    #[test]
    fn own_text_is_the_invocation_repeated_with_or_without_spaces() {
        assert!(is_own("$debrief", "$debrief"));
        assert!(is_own("$debrief $debrief $debrief", "$debrief"));
        assert!(is_own("/debrief/debrief/debrief", "/debrief"));
        assert!(is_own("/skill:debrief", "/skill:debrief"));
        assert!(!is_own("", "$debrief"));
        assert!(!is_own("$debrief please", "$debrief"));
        assert!(!is_own("$debrie", "$debrief"));
        assert!(!is_own("/debrief", ""));
    }

    /// Classify captured screens: `HERDR_COMPOSER_FIXTURES=<dir>` holding
    /// `<agent>__<label>.ansi` files, where a label starting `draft` expects a
    /// person's draft, `own` the invocation, and anything else an empty composer.
    #[test]
    #[ignore = "needs captured screens"]
    fn captured_screens_classify_by_their_labels() {
        let dir = std::env::var("HERDR_COMPOSER_FIXTURES").expect("fixture dir");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let Some((agent, label)) = name.trim_end_matches(".ansi").split_once("__") else {
                continue;
            };
            let screen = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
            let read = draft(agent, &screen).unwrap_or_else(|| panic!("{name}: unreadable"));
            let invocation = match agent {
                "claude" => "/debrief",
                "codex" => "$debrief",
                _ => "/skill:debrief",
            };
            if label.starts_with("own") {
                assert!(is_own(&read, invocation), "{name}: {read:?}");
            } else if label.starts_with("draft") {
                assert!(!read.is_empty() && !is_own(&read, invocation), "{name}: {read:?}");
            } else {
                assert_eq!(read, "", "{name}");
            }
            checked += 1;
        }
        assert!(checked > 0);
        eprintln!("classified {checked} captured screens");
    }
}
