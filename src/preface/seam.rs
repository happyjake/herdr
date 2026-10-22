//! Where a client's screen meets its pane's record.
//!
//! A client pages a pane's history with `recent_unwrapped` offset reads and
//! holds every line from its oldest page down to the bottom. It names that
//! oldest line by its offset and its text. The screen is read here the same
//! way, so the offset means the same line — every page from one state of
//! the terminal, so nothing printed between two pages can shift one — and
//! the text re-anchors it when the tail has grown since the client looked.
//!
//! From the anchor down, the record's prompts are matched against the
//! screen's prompt rows, newest first, the way the two have to line up: the
//! newest prompt on screen is one of the record's newest. A row counts as a
//! prompt only when it carries the harness's prompt marker, so an answer or
//! tool output that repeats a prompt's words is never taken for it. The
//! oldest prompt that lines up is the first turn the client holds; every
//! turn older than it is the preface. A turn the anchor cuts through — its
//! prompt above the client's oldest line, some of its answer below — is
//! therefore quoted whole, and a few of its lines show twice.
//!
//! When the anchor or the prompts cannot be placed beyond doubt, there is
//! no preface: a preface placed by a guess would say the screen begins
//! somewhere it does not, and could leave out a turn without counting it.

use super::{ReadTurns, Turn};
use crate::api::schema::{Preface, PrefaceReason, PrefaceTurn, TurnStart};

/// Lines one screen page holds: the most an offset read serves.
pub(crate) const PAGE_LINES: u32 = 1000;

/// Screen lines one preface reads at most.
pub(crate) const MAX_SCREEN_LINES: usize = 12_000;

/// How far above its offset the client's oldest line is looked for: the
/// tail may have grown by this much while the client paged.
const ANCHOR_ABOVE: usize = 500;

/// How far below it.
const ANCHOR_BELOW: usize = 50;

/// Record prompts that may sit between two that line up with the screen.
const LOOKBACK: usize = 16;

/// A row this long or longer that a prompt's first line begins with is
/// that line wrapped.
const WRAPPED_PREFIX_CHARS: usize = 20;

/// Prompted turns one preface carries at most.
pub(crate) const MAX_TURNS: u32 = 200;

/// Wake and task turns one preface carries at most, among the prompted
/// turns it keeps.
pub(crate) const MAX_WAKE_TASK_TURNS: usize = 50;

/// Recent unwrapped history from the bottom up, read from one state of the
/// terminal: its lines top first, and whether history remains above them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub text: String,
    pub has_more: bool,
}

/// Where the client's held screen begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Top<'a> {
    pub offset: Option<u64>,
    /// The client's first held lines, oldest first, from the one at
    /// `offset` down.
    pub lines: Option<&'a [String]>,
}

/// What marks a prompt row on one harness's screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PromptEvidence<'a> {
    /// What the harness draws at column 0 ahead of a prompt (see
    /// [`super::Reader::prompt_markers`]).
    pub markers: &'a [&'a str],
    /// The background the harness lays under a prompt (see
    /// [`super::Reader::prompt_background`]); when set, the screen is read
    /// with its styling and the background is the evidence.
    pub background: Option<[u8; 3]>,
}

/// A screen as far up as it was read: `rows[i]` is the line `i` lines up
/// from the bottom.
#[derive(Debug, Default)]
struct Screen {
    rows: Vec<String>,
    /// Whether the read reached the screen's top.
    top: bool,
    /// Whether `rows[i]` is a prompt row by its background; empty unless
    /// the harness sets a prompt apart by one.
    prompts: Vec<bool>,
}

/// The preface one request asks for.
///
/// `evidence` is what marks a prompt row on the harness's screen. `span`
/// reads the screen, styled when `evidence` names a background: asked for
/// more than a number of lines (or the whole screen, up to
/// [`MAX_SCREEN_LINES`]), it answers one span, or nothing when the screen
/// cannot be read that way.
pub(crate) fn preface(
    agent: &str,
    fallback_session_id: &str,
    read: &ReadTurns,
    top: Top<'_>,
    max_turns: u32,
    evidence: PromptEvidence<'_>,
    span: impl FnOnce(usize) -> Option<Span>,
) -> Result<Preface, PrefaceReason> {
    let turns = &read.turns.turns;
    if evidence.markers.is_empty() && evidence.background.is_none() {
        return Err(PrefaceReason::SeamNotFound);
    }
    let want = top.offset.map_or(MAX_SCREEN_LINES, |offset| {
        usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .saturating_add(ANCHOR_ABOVE)
            .min(MAX_SCREEN_LINES)
    });
    let screen = span(want)
        .map(|span| screen_of(span, evidence.background))
        .ok_or(PrefaceReason::SeamNotFound)?;
    let anchor = anchor(&screen, top).ok_or(PrefaceReason::SeamNotFound)?;
    let held = &screen.rows[..=anchor];
    let seam = match evidence.background {
        Some(_) => place_prompts(
            turns,
            held.iter()
                .zip(&screen.prompts)
                .map(|(row, prompt)| prompt.then(|| crate::prompt_trail::clean(row))),
        ),
        None => place(turns, evidence.markers, held.iter().map(String::as_str)),
    }
    .ok_or(PrefaceReason::SeamNotFound)?;
    let (kept, omitted) = select(turns, seam, max_turns.min(MAX_TURNS));
    if kept.is_empty() && omitted == 0 {
        return Err(PrefaceReason::NothingAbove);
    }
    Ok(Preface {
        agent: agent.to_string(),
        session_id: read
            .turns
            .session_id
            .clone()
            .unwrap_or_else(|| fallback_session_id.to_string()),
        revision: read.revision.clone(),
        turns: kept.into_iter().map(wire_turn).collect(),
        omitted: u32::try_from(omitted).unwrap_or(u32::MAX),
    })
}

fn wire_turn(turn: &Turn) -> PrefaceTurn {
    PrefaceTurn {
        id: turn.id.clone(),
        at: turn.at,
        start: turn.start,
        prompt: turn.prompt.clone(),
        end: turn.end,
        answer: turn.answer.clone(),
        prompt_cut: turn.prompt_cut,
        answer_cut: turn.answer_cut,
    }
}

/// A span's lines, bottom first, split the way a client splits them; with
/// a prompt background, which of them are prompt rows by it.
fn screen_of(span: Span, background: Option<[u8; 3]>) -> Screen {
    let text = span.text.strip_suffix('\n').unwrap_or(&span.text);
    let lines: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').collect()
    };
    let mut prompts = background.map_or_else(Vec::new, |background| {
        prompt_rows_by_background(&lines, background)
    });
    prompts.reverse();
    let mut rows: Vec<String> = lines.into_iter().map(row).collect();
    rows.reverse();
    Screen {
        rows,
        top: !span.has_more,
        prompts,
    }
}

/// Which of a screen's styled lines, top first, are prompt rows by their
/// background: a row whose every visible cell is under `background`, and
/// the first such row of its block. A block is a run of rows in that
/// colour — text rows and the padding rows the harness draws around them —
/// so a prompt's later lines are no prompt rows of their own.
fn prompt_rows_by_background(lines: &[&str], background: [u8; 3]) -> Vec<bool> {
    let mut block_has_text = false;
    lines
        .iter()
        .map(|line| match shade(line, background) {
            Shade::Text { under: true } => !std::mem::replace(&mut block_has_text, true),
            Shade::Blank { coloured: true } => false,
            Shade::Text { under: false } | Shade::Blank { coloured: false } => {
                block_has_text = false;
                false
            }
        })
        .collect()
}

/// How one styled line sits against a background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shade {
    /// It shows text; `under` when every visible cell is in the background.
    Text { under: bool },
    /// It shows none; `coloured` when any of its cells is in the background.
    Blank { coloured: bool },
}

fn shade(line: &str, background: [u8; 3]) -> Shade {
    let mut current: Option<[u8; 3]> = None;
    let mut visible = false;
    let mut under = true;
    let mut coloured = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            if next == 'm' {
                                apply_sgr_background(&params, &mut current);
                            }
                            break;
                        }
                        params.push(next);
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if character.is_control() {
            continue;
        }
        let in_background = current == Some(background);
        if character.is_whitespace() {
            coloured |= in_background;
        } else {
            visible = true;
            under &= in_background;
        }
    }
    if visible {
        Shade::Text { under }
    } else {
        Shade::Blank { coloured }
    }
}

/// Follow one SGR sequence's effect on the background: a truecolour one is
/// kept, and anything else that sets or resets it is no colour this
/// compares against. Colour arguments of the other attributes are stepped
/// over so they are never read as codes of their own.
fn apply_sgr_background(params: &str, current: &mut Option<[u8; 3]>) {
    let codes: Vec<&str> = params.split(';').collect();
    let mut index = 0;
    while index < codes.len() {
        let code = codes[index];
        index += 1;
        if code.contains(':') {
            let parts: Vec<&str> = code.split(':').collect();
            if parts[0] == "48" {
                *current = (parts.get(1) == Some(&"2") && parts.len() >= 5)
                    .then(|| rgb(&parts[parts.len() - 3..]))
                    .flatten();
            }
            continue;
        }
        match code {
            "" | "0" | "49" => *current = None,
            "38" | "48" | "58" => {
                let set = match codes.get(index) {
                    Some(&"2") => {
                        let colour = codes.get(index + 1..index + 4).and_then(rgb);
                        index += 4;
                        colour
                    }
                    Some(&"5") => {
                        index += 2;
                        None
                    }
                    _ => None,
                };
                if code == "48" {
                    *current = set;
                }
            }
            _ => {
                if matches!(code.parse::<u16>(), Ok(40..=47 | 100..=107)) {
                    *current = None;
                }
            }
        }
    }
}

fn rgb(parts: &[&str]) -> Option<[u8; 3]> {
    match parts {
        [r, g, b] => Some([r.parse().ok()?, g.parse().ok()?, b.parse().ok()?]),
        _ => None,
    }
}

/// One screen line as the two sides compare it: no styling, no carriage
/// returns, no trailing blanks.
fn row(line: &str) -> String {
    crate::pane::strip_ansi(line)
        .replace('\r', "")
        .trim_end()
        .to_string()
}

/// Where in the screen the client's oldest held line is now.
///
/// The client's first held lines are looked for as one run, top line
/// within a margin around its offset and each next line one row below, and
/// the run must be there exactly once. A run that repeats nearby — blank
/// lines, most often — names no one place: taking its nearest copy on a
/// screen that moved would hand the client turns it already holds, or take
/// from it turns it does not. The anchor is the run's first line.
fn anchor(screen: &Screen, top: Top<'_>) -> Option<usize> {
    let Some(offset) = top.offset else {
        return (screen.top && !screen.rows.is_empty()).then(|| screen.rows.len() - 1);
    };
    let wanted: Vec<String> = top.lines?.iter().map(|line| row(line)).collect();
    let below = wanted.len().checked_sub(1)?;
    let offset = usize::try_from(offset).ok()?;
    let low = offset.saturating_sub(ANCHOR_BELOW).max(below);
    let high = offset
        .saturating_add(ANCHOR_ABOVE)
        .min(screen.rows.len().checked_sub(1)?);
    let mut found = (low..=high).filter(|first| {
        wanted
            .iter()
            .enumerate()
            .all(|(step, line)| screen.rows[first - step] == *line)
    });
    let only = found.next()?;
    found.next().is_none().then_some(only)
}

/// The index of the oldest turn whose prompt the held rows show, when the
/// record's prompts line up with them beyond doubt.
///
/// Rows are walked from the bottom up and prompts from the newest back, so
/// a prompt that repeats is matched where its neighbours say it is. Only a
/// row carrying a prompt marker can stand for a prompt. A prompt row that
/// matches no record prompt is a prompt drawn some other way (a paste
/// shown as a placeholder, say), and each such row may account for one
/// record prompt that lines up with nothing; a record prompt skipped with
/// no row to account for it would be a turn left out unseen, so the seam
/// is refused instead.
fn place<'a>(
    turns: &[Turn],
    markers: &[&str],
    held_bottom_first: impl Iterator<Item = &'a str>,
) -> Option<usize> {
    place_prompts(
        turns,
        held_bottom_first.map(|line| prompt_row(line, markers)),
    )
}

/// [`place`] over rows already judged: each held row, bottom first, is its
/// words as a prompt when it is a prompt row and nothing otherwise.
fn place_prompts(
    turns: &[Turn],
    held_bottom_first: impl Iterator<Item = Option<String>>,
) -> Option<usize> {
    let prompted: Vec<usize> = turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| turn.start == TurnStart::Prompt && turn.head.is_some())
        .map(|(index, _)| index)
        .collect();
    let mut remaining = prompted.len();
    let mut unaccounted = 0usize;
    let mut seam = None;
    for line in held_bottom_first {
        if remaining == 0 {
            break;
        }
        let Some(line) = line else {
            continue;
        };
        if line.is_empty() {
            continue;
        }
        let matched = (0..LOOKBACK.min(remaining)).find(|back| {
            turns[prompted[remaining - 1 - back]]
                .head
                .as_deref()
                .is_some_and(|head| shows(&line, head))
        });
        match matched {
            Some(back) if back > unaccounted => return None,
            Some(back) => {
                seam = Some(prompted[remaining - 1 - back]);
                remaining -= back + 1;
                unaccounted = 0;
            }
            None => unaccounted += 1,
        }
    }
    seam
}

/// A row's words as a prompt, when the row is one: it begins at column 0
/// with one of the harness's prompt markers, followed by a space or
/// nothing.
fn prompt_row(line: &str, markers: &[&str]) -> Option<String> {
    markers.iter().find_map(|marker| {
        let rest = line.strip_prefix(marker)?;
        (rest.is_empty() || rest.starts_with(' ')).then(|| crate::prompt_trail::clean(rest))
    })
}

/// Whether a row shows a prompt's first line: all of it, or the start of it
/// wrapped where the harness wrapped it.
fn shows(row: &str, head: &str) -> bool {
    row == head
        || (head.starts_with(row) && row.chars().count() >= WRAPPED_PREFIX_CHARS)
        || (row.starts_with(head) && head.chars().count() >= WRAPPED_PREFIX_CHARS)
}

/// The turns older than the seam a preface carries, oldest first, and how
/// many of them it leaves out.
///
/// The newest `max_turns` prompted turns are kept, with the wake and task
/// turns among them up to their own cap, newest first; everything older
/// is counted rather than sent. The seam is itself one of the record's
/// turns, so the newest turn — the one that may still be running — is
/// never older than it and never sent; an older turn the record left open
/// is kept as it is.
fn select(turns: &[Turn], seam: usize, max_turns: u32) -> (Vec<&Turn>, usize) {
    let span = &turns[..seam.min(turns.len())];
    let prompted: Vec<usize> = span
        .iter()
        .enumerate()
        .filter(|(_, turn)| turn.start == TurnStart::Prompt)
        .map(|(index, _)| index)
        .collect();
    let max_turns = usize::try_from(max_turns).unwrap_or(usize::MAX);
    let first = if max_turns == 0 {
        span.len()
    } else if prompted.is_empty() {
        0
    } else {
        prompted[prompted.len().saturating_sub(max_turns)]
    };
    let mut wake_or_task = 0;
    let mut kept: Vec<&Turn> = span[first..]
        .iter()
        .rev()
        .filter(|turn| {
            if turn.start == TurnStart::Prompt {
                return true;
            }
            wake_or_task += 1;
            wake_or_task <= MAX_WAKE_TASK_TURNS
        })
        .collect();
    kept.reverse();
    let omitted = span.len() - kept.len();
    (kept, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::strip_ansi;

    #[test]
    fn styling_is_no_part_of_a_line() {
        assert_eq!(
            strip_ansi("\u{1b}[38;2;153;153;153m❯ \u{1b}[0mhello\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{1b}\\"),
            "❯ hellolink"
        );
        assert_eq!(row("  text \u{1b}[0m  \r"), "  text");
    }

    #[test]
    fn a_wrapped_prompt_row_shows_its_prompt_but_a_short_one_must_be_whole() {
        let markers = ["\u{276f}", ">"];
        let prompt_row = |line: &str| prompt_row(line, &markers).expect("a prompt row");
        let head = "trace why the ferry timetable importer drops the last sailing of the day";
        assert!(shows(&prompt_row("❯ trace why the ferry timetable"), head));
        assert!(shows(
            &prompt_row(
                "> trace why the ferry timetable importer drops the last sailing of the day"
            ),
            head
        ));
        assert!(
            !shows(&prompt_row("❯ trace why"), head),
            "too short to be a wrapped row"
        );
        assert!(shows(&prompt_row("❯ yes"), "yes"));
        assert!(!shows(&prompt_row("❯ yes, and more"), "yes"));
    }

    #[test]
    fn only_a_row_with_the_marker_at_column_zero_is_a_prompt_row() {
        let markers = ["\u{276f}"];
        assert_eq!(
            prompt_row("❯ map  the piers", &markers).as_deref(),
            Some("map the piers")
        );
        assert_eq!(prompt_row("❯ ", &markers).as_deref(), Some(""));
        // A marker run into a word, an indented one, and none at all.
        assert_eq!(prompt_row("❯not a marker", &markers), None);
        assert_eq!(prompt_row("  ❯ map the piers", &markers), None);
        assert_eq!(prompt_row("map the piers", &markers), None);
        assert_eq!(prompt_row("⏺ map the piers", &markers), None);
    }

    fn turn(index: usize, start: TurnStart) -> Turn {
        let prompt = (start == TurnStart::Prompt).then(|| format!("prompt number {index}"));
        let mut turn = Turn::started(format!("t{index}"), index as u64, start, prompt);
        turn.answer("an answer");
        turn
    }

    #[test]
    fn wake_and_task_turns_ride_along_under_their_own_cap() {
        // Prompted turns at either end, and more wake and task turns than
        // the cap between them.
        let mut turns = vec![turn(0, TurnStart::Prompt)];
        turns.extend((1..=MAX_WAKE_TASK_TURNS + 10).map(|index| {
            turn(
                index,
                if index % 2 == 0 {
                    TurnStart::Wake
                } else {
                    TurnStart::Task
                },
            )
        }));
        let last_prompt = turns.len();
        turns.push(turn(last_prompt, TurnStart::Prompt));
        turns.push(turn(last_prompt + 1, TurnStart::Prompt));

        let (kept, omitted) = select(&turns, last_prompt + 1, MAX_TURNS);
        assert_eq!(omitted, 10);
        assert_eq!(kept.len(), MAX_WAKE_TASK_TURNS + 2);
        assert_eq!(
            kept[0].id, "t0",
            "a prompted turn is never dropped for the cap"
        );
        // The newest wake and task turns are the ones kept.
        assert_eq!(kept[1].id, "t11");
        assert_eq!(
            kept.last().map(|turn| turn.id.as_str()),
            Some(format!("t{last_prompt}").as_str())
        );

        // With no room for prompted turns, nothing is sent and everything is
        // counted.
        let (kept, omitted) = select(&turns, last_prompt + 1, 0);
        assert!(kept.is_empty());
        assert_eq!(omitted, last_prompt + 1);
    }

    #[test]
    fn a_repeated_prompt_is_placed_where_its_neighbours_say() {
        let turns: Vec<Turn> = ["continue", "map the piers", "continue", "continue"]
            .iter()
            .enumerate()
            .map(|(index, text)| {
                Turn::started(
                    format!("t{index}"),
                    index as u64,
                    TurnStart::Prompt,
                    Some(text.to_string()),
                )
            })
            .collect();
        // The screen shows the last two prompts only.
        let held = ["❯ continue", "  output", "❯ continue", "  output", "❯ "];
        assert_eq!(place(&turns, &MARKERS, held.iter().rev().copied()), Some(2));
        // And all four.
        let held = ["❯ continue", "❯ map the piers", "❯ continue", "❯ continue"];
        assert_eq!(place(&turns, &MARKERS, held.iter().rev().copied()), Some(0));
        // None of them.
        assert_eq!(
            place(&turns, &MARKERS, ["❯ something else"].into_iter()),
            None
        );
    }

    const MARKERS: [&str; 1] = ["\u{276f}"];

    fn prompted(texts: &[&str]) -> ReadTurns {
        let turns = texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let mut turn = Turn::started(
                    format!("t{index}"),
                    index as u64,
                    TurnStart::Prompt,
                    Some(text.to_string()),
                );
                turn.answer(&format!("answer to {text}"));
                turn
            })
            .collect();
        ReadTurns {
            revision: "r".into(),
            turns: std::sync::Arc::new(super::super::RecordTurns {
                session_id: Some("s".into()),
                turns,
            }),
        }
    }

    fn preface_of(
        read: &ReadTurns,
        rows: &[&str],
        top: Top<'_>,
    ) -> Result<Vec<String>, PrefaceReason> {
        let text = rows.join("\n");
        preface(
            "claude",
            "s",
            read,
            top,
            MAX_TURNS,
            PromptEvidence {
                markers: &MARKERS,
                background: None,
            },
            |_| {
                Some(Span {
                    text,
                    has_more: false,
                })
            },
        )
        .map(|preface| {
            preface
                .turns
                .into_iter()
                .filter_map(|turn| turn.prompt)
                .collect()
        })
    }

    fn run(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// The client's oldest line repeats near its offset, and the screen has
    /// grown since: the copy now at the offset is not the client's. Alone it
    /// names no place; with the lines the client holds below it, it does.
    #[test]
    fn an_anchor_run_must_occur_once_near_its_offset() {
        let read = prompted(&["prompt A", "prompt B", "prompt C"]);
        let before = [
            "❯ prompt A",
            "⏺ answer A",
            "",
            "❯ prompt B",
            "⏺ answer B",
            "",
            "❯ prompt C",
            "⏺ answer C",
            "❯ ",
        ];
        // The client holds from the first blank row, offset 6, so it holds
        // prompt B and prompt C.
        assert_eq!(before.len() - 1 - 2, 6);
        // Three rows printed above the footer move the second blank row to
        // offset 6.
        let mut after = before[..8].to_vec();
        after.extend(["⏺ more", "⏺ more still", "⏺ and more"]);
        after.push("❯ ");
        assert_eq!(after[after.len() - 1 - 6], "");

        let blank = run(&[""]);
        let top = Top {
            offset: Some(6),
            lines: Some(&blank),
        };
        assert_eq!(
            preface_of(&read, &after, top),
            Err(PrefaceReason::SeamNotFound)
        );

        // The blank row and the prompt below it occur once.
        let held = run(&["", "❯ prompt B"]);
        let top = Top {
            offset: Some(6),
            lines: Some(&held),
        };
        assert_eq!(
            preface_of(&read, &after, top),
            Ok(vec!["prompt A".to_string()])
        );
        // Styling and trailing blanks are no part of either side.
        let styled = run(&["  \u{1b}[0m", "\u{1b}[1m❯ prompt B\u{1b}[0m  "]);
        let top = Top {
            offset: Some(6),
            lines: Some(&styled),
        };
        assert_eq!(
            preface_of(&read, &after, top),
            Ok(vec!["prompt A".to_string()])
        );

        // A run of nothing but blank rows that repeats still names no place.
        let mut blanks = after[..3].to_vec();
        blanks.extend([
            "",
            "❯ prompt B",
            "⏺ answer B",
            "",
            "",
            "❯ prompt C",
            "⏺ answer C",
            "❯ ",
        ]);
        let two_blank = run(&["", ""]);
        let top = Top {
            offset: Some(8),
            lines: Some(&two_blank),
        };
        // The client's pair sits at offset 8; the same pair sits at 4.
        assert_eq!(blanks.len() - 1 - 8, 2);
        assert_eq!(&blanks[2..4], &["", ""]);
        assert_eq!(&blanks[6..8], &["", ""]);
        assert_eq!(
            preface_of(&read, &blanks, top),
            Err(PrefaceReason::SeamNotFound)
        );

        // A line that appears once near its offset still anchors after the
        // same growth.
        let unique = run(&["⏺ answer A"]);
        let top = Top {
            offset: Some(7),
            lines: Some(&unique),
        };
        assert_eq!(
            preface_of(&read, &after, top),
            Ok(vec!["prompt A".to_string()])
        );
    }

    /// An answer that repeats a prompt's words is not the prompt.
    #[test]
    fn a_row_without_the_prompt_marker_never_stands_for_a_prompt() {
        let read = prompted(&["old work", "continue", "continue"]);
        let held = ["❯ continue", "⏺ Summary", "  continue", "❯ "];
        let top = Top {
            offset: None,
            lines: None,
        };
        // The client holds only the newest turn; both turns before it are
        // its preface, none left out.
        assert_eq!(
            preface_of(&read, &held, top),
            Ok(vec!["old work".to_string(), "continue".to_string()])
        );
    }

    /// A record prompt that lines up with no row, and no unmatched prompt
    /// row to account for it, would be a turn left out unseen.
    #[test]
    fn a_record_prompt_skipped_with_no_row_to_account_for_it_places_no_seam() {
        let read = prompted(&["map the piers", "chart the tides", "print the map"]);
        let top = Top {
            offset: None,
            lines: None,
        };
        // "chart the tides" is not on the screen at all.
        assert_eq!(
            preface_of(
                &read,
                &["❯ map the piers", "⏺ done", "❯ print the map", "❯ "],
                top
            ),
            Err(PrefaceReason::SeamNotFound)
        );
        // Drawn some other way, it still has a prompt row standing for it.
        assert_eq!(
            preface_of(
                &read,
                &[
                    "❯ map the piers",
                    "❯ [Pasted text #1 +4 lines]",
                    "❯ print the map",
                    "❯ "
                ],
                top
            ),
            Err(PrefaceReason::NothingAbove)
        );
    }

    #[test]
    fn a_harness_that_declares_no_prompt_marker_places_no_seam() {
        let read = prompted(&["map the piers"]);
        assert_eq!(
            preface(
                "pi",
                "s",
                &read,
                Top {
                    offset: None,
                    lines: None
                },
                MAX_TURNS,
                PromptEvidence::default(),
                |_| { panic!("the screen is not read when it cannot be lined up") }
            ),
            Err(PrefaceReason::SeamNotFound)
        );
    }
}
