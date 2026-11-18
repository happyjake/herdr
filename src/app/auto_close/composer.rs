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
                strip_marker(&body, &['❯'])?
            } else {
                body
            }
        }
        "agy" => agy_draft(&lines)?,
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
    muted_fg: bool,
}

fn cells(line: &str) -> Vec<Cell> {
    let mut out = Vec::new();
    let (mut faint, mut inverse, mut muted_fg) = (false, false, false);
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(Cell {
                ch,
                faint,
                inverse,
                muted_fg,
            });
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
        let params: Vec<&str> = params.split(';').collect();
        let mut index = 0;
        while index < params.len() {
            let param = params[index];
            if param.contains(':') {
                let parts: Vec<&str> = param.split(':').collect();
                if parts.first() == Some(&"38") {
                    muted_fg = match parts.get(1) {
                        Some(&"5") => parts
                            .get(2)
                            .and_then(|code| code.parse::<u8>().ok())
                            .is_some_and(is_muted_grey_256),
                        Some(&"2") if parts.len() >= 5 => {
                            rgb(&parts[parts.len() - 3..]).is_some_and(is_muted_grey_rgb)
                        }
                        _ => false,
                    };
                }
                index += 1;
                continue;
            }
            match param {
                "" | "0" => (faint, inverse, muted_fg) = (false, false, false),
                "2" => faint = true,
                "22" => faint = false,
                "7" => inverse = true,
                "27" => inverse = false,
                "39" => muted_fg = false,
                "90" => muted_fg = true,
                "30" | "31" | "32" | "33" | "34" | "35" | "36" | "37" | "91" | "92" | "93"
                | "94" | "95" | "96" | "97" => muted_fg = false,
                "38" => {
                    match params.get(index + 1) {
                        Some(&"5") => {
                            muted_fg = params
                                .get(index + 2)
                                .and_then(|code| code.parse::<u8>().ok())
                                .is_some_and(is_muted_grey_256);
                            index += 2;
                        }
                        Some(&"2") => {
                            muted_fg = params
                                .get(index + 2..index + 5)
                                .and_then(rgb)
                                .is_some_and(is_muted_grey_rgb);
                            index += 4;
                        }
                        _ => muted_fg = false,
                    };
                }
                "48" | "58" => {
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

fn rgb(parts: &[&str]) -> Option<[u8; 3]> {
    match parts {
        [r, g, b] => Some([r.parse().ok()?, g.parse().ok()?, b.parse().ok()?]),
        _ => None,
    }
}

fn is_muted_grey_256(code: u8) -> bool {
    code == 8 || (232..=252).contains(&code)
}

fn is_muted_grey_rgb([r, g, b]: [u8; 3]) -> bool {
    let min = r.min(g).min(b);
    let max = r.max(g).max(b);
    max - min <= 24 && min >= 40 && max <= 210
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

/// Typed draft text for agy: drops faint, muted-grey placeholders/scroll
/// indicators, and inverse blank cursors.
fn typed_agy(line: &[Cell]) -> String {
    line.iter()
        .filter(|cell| !cell.faint && !cell.muted_fg && !(cell.inverse && cell.ch == ' '))
        .map(|cell| if cell.ch == '\u{a0}' { ' ' } else { cell.ch })
        .collect()
}

fn agy_draft(lines: &[Vec<Cell>]) -> Option<String> {
    let end = lines
        .iter()
        .rposition(|line| !plain(line).trim().is_empty())
        .map_or(0, |index| index + 1);
    let bottom = lines[..end].iter().rposition(|line| is_rule(line))?;
    let footer = &lines[bottom + 1..end];
    if footer.is_empty() || footer.len() > 6 || !footer.iter().all(|line| is_agy_footer_line(line))
    {
        return None;
    }
    let bottom_shape = plain(&lines[bottom]);
    let bottom_trimmed = bottom_shape.trim_end();
    let top = (0..bottom).rev().find(|&index| {
        is_rule(&lines[index]) && plain(&lines[index]).trim_end() == bottom_trimmed
    })?;
    let first = (top + 1..bottom).find(|&index| !plain(&lines[index]).trim().is_empty())?;
    if !is_agy_prompt_start(&lines[first]) {
        return None;
    }
    let mut body = vec![typed_agy(&lines[first][1..])];
    for line in &lines[first + 1..bottom] {
        body.push(typed_agy(line));
    }
    Some(body.join(" "))
}

fn is_agy_prompt_start(line: &[Cell]) -> bool {
    matches!(
        (
            line.first().map(|cell| cell.ch),
            line.get(1).map(|cell| cell.ch),
        ),
        (Some('>'), None | Some(' ' | '\u{a0}'))
    )
}

fn is_agy_footer_line(line: &[Cell]) -> bool {
    let text = plain(line);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.starts_with('>')
        || trimmed.starts_with('❯')
        || trimmed.starts_with('›')
        || trimmed.starts_with('●')
        || trimmed.starts_with('•')
        || trimmed.starts_with('⏺')
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("navigate")
        || lower.contains("enter select")
        || lower.contains("enter confirm")
        || lower.contains("esc skip")
        || lower.contains("tab amend")
        || lower.starts_with("1.")
        || lower.starts_with("2.")
        || lower.starts_with("3.")
    {
        return false;
    }
    lower.contains("? for shortcuts")
        || lower.contains("esc to cancel")
        || lower.contains("gemini")
        || lower.contains("claude")
        || lower.contains("orchestrator")
        || lower.contains("best-of-n")
        || lower.contains("accept-edits")
        || lower.contains("accept edits")
        || lower.contains("plan mode")
        || lower.contains("auto mode")
        || lower.ends_with(" task")
        || lower.ends_with(" tasks")
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

    const RULE: &str =
        "\x1b[0m\x1b[38;2;136;136;136m────────────────────────────────────────\x1b[0m";

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

    fn agy(composer: &str) -> String {
        format!("• Done.\n\n{RULE}\n{composer}\n{RULE}\n  ? for shortcuts    Gemini")
    }

    #[test]
    fn empty_composers_read_empty_through_their_placeholders_and_cursors() {
        assert_eq!(draft("claude", &claude("❯\u{a0}")).as_deref(), Some(""));
        assert_eq!(
            draft(
                "claude",
                &claude("❯ \x1b[0m\x1b[2mTry \"fix lint errors\"\x1b[0m")
            )
            .as_deref(),
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
        assert_eq!(draft("agy", &agy("> ")).as_deref(), Some(""));
        assert_eq!(
            draft("agy", &agy("> \x1b[0m\x1b[2mAsk anything\x1b[0m")).as_deref(),
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
        assert_eq!(
            draft("agy", &agy("> ship them all")).as_deref(),
            Some("ship them all")
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
    fn agy_filters_non_faint_grey_placeholders_and_scroll_indicators() {
        for placeholder in [
            "> \x1b[38;5;8mAsk anything\x1b[0m",
            "> \x1b[90mPlan mode: Ask anything\x1b[0m",
            "> \x1b[38;2;128;128;128mAuto mode: Ask anything\x1b[0m",
            "\x1b[90m> Accept-edits mode: Ask anything\x1b[0m",
            "> \x1b[38;2;128;128;128mBest-of-N mode: Ask anything\x1b[0m\n  \x1b[90m↓ 2 more lines\x1b[0m",
        ] {
            assert_eq!(draft("agy", &agy(placeholder)).as_deref(), Some(""));
        }
        assert_eq!(
            draft("agy", &agy("> ship it\n  \x1b[90m↓ 2 more lines\x1b[0m")).as_deref(),
            Some("ship it")
        );
    }

    #[test]
    fn agy_reads_multiline_drafts_including_interior_rules() {
        assert_eq!(
            draft("agy", &agy("> line 1\n  line 2")).as_deref(),
            Some("line 1 line 2")
        );
        assert_eq!(
            draft("agy", &agy("> line 1\n  ──────────\n  line 2")).as_deref(),
            Some("line 1 ────────── line 2")
        );
    }

    #[test]
    fn agy_rejects_markerless_boxes_and_past_turn_or_dialog_screens() {
        // Markerless two-rule box, even with an earlier turn rule above it.
        let markerless = format!(
            "{RULE}\n> earlier prompt\n• Done.\n\n{RULE}\n  no chevron\n{RULE}\n  ? for shortcuts    Gemini"
        );
        assert_eq!(draft("agy", &markerless), None);

        // Single-rule dialog / past-turn overlay screens without a valid live composer box.
        let single_rule_dialog = format!(
            "{RULE}\n> past prompt\n• Done.\n\n{RULE}\n  1. Allow once\n  2. Deny\n  ↑/↓ Navigate · enter Select"
        );
        assert_eq!(draft("agy", &single_rule_dialog), None);

        let past_turn_overlay =
            format!("{RULE}\n> past prompt\n{RULE}\n• Past answer\n  More prose below");
        assert_eq!(draft("agy", &past_turn_overlay), None);

        let dialog_above_cancel_footer =
            format!("{RULE}\n> past prompt\n{RULE}\n  1. Allow once\n  esc to cancel");
        assert_eq!(draft("agy", &dialog_above_cancel_footer), None);
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
                assert!(
                    !read.is_empty() && !is_own(&read, invocation),
                    "{name}: {read:?}"
                );
            } else {
                assert_eq!(read, "", "{name}");
            }
            checked += 1;
        }
        assert!(checked > 0);
        eprintln!("classified {checked} captured screens");
    }
}
