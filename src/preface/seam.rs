//! Where a client's screen meets its pane's record.
//!
//! A client pages a pane's history with `recent_unwrapped` offset reads and
//! holds every line from its oldest page down to the bottom. It names that
//! oldest line by its offset and its text. The screen is read here the same
//! way, so the offset means the same line, and the text re-anchors it when
//! the tail has grown since the client looked.
//!
//! From the anchor down, the record's prompts are matched against the
//! screen's rows, newest first, the way the two have to line up: the
//! newest prompt on screen is one of the record's newest. The oldest prompt
//! that lines up is the first turn the client holds; every turn older than
//! it is the preface. A turn the anchor cuts through — its prompt above the
//! client's oldest line, some of its answer below — is therefore quoted
//! whole, and a few of its lines show twice.
//!
//! When the anchor or the prompts cannot be found, there is no preface: a
//! preface placed by a guess would say the screen begins somewhere it does
//! not.

use super::{ReadTurns, Turn};
use crate::api::schema::{Preface, PrefaceReason, PrefaceTurn, TurnStart};

/// Lines one screen page asks for: the most an offset read serves.
pub(crate) const PAGE_LINES: u32 = 1000;

/// Screen lines one preface reads at most.
const MAX_SCREEN_LINES: usize = 12_000;

/// How far above its offset the client's oldest line is looked for: the
/// tail may have grown by this much while the client paged.
const ANCHOR_ABOVE: usize = 2_000;

/// How far below it.
const ANCHOR_BELOW: usize = 200;

/// Times the screen is read again when it moved between two pages.
const SCREEN_ATTEMPTS: usize = 3;

/// Record prompts that may sit between two that line up with the screen
/// without showing on it.
const LOOKBACK: usize = 16;

/// A row this long or longer that a prompt's first line begins with is
/// that line wrapped.
const WRAPPED_PREFIX_CHARS: usize = 20;

/// Prompted turns one preface carries at most.
pub(crate) const MAX_TURNS: u32 = 200;

/// Wake and task turns one preface carries at most, among the prompted
/// turns it keeps.
pub(crate) const MAX_WAKE_TASK_TURNS: usize = 50;

/// One page of a `recent_unwrapped` offset read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub text: String,
    pub effective_offset: u64,
    pub has_more: bool,
}

/// Where the client's held screen begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Top<'a> {
    pub offset: Option<u64>,
    pub line: Option<&'a str>,
}

/// A screen as far up as it was read: `rows[i]` is the line `i` lines up
/// from the bottom.
#[derive(Debug, Default)]
struct Screen {
    rows: Vec<String>,
    /// Whether the read reached the screen's top.
    top: bool,
}

/// The preface one request asks for.
pub(crate) fn preface(
    agent: &str,
    fallback_session_id: &str,
    read: &ReadTurns,
    top: Top<'_>,
    max_turns: u32,
    page: impl FnMut(u64) -> Option<Page>,
) -> Result<Preface, PrefaceReason> {
    let turns = &read.turns.turns;
    let want = top.offset.map(|offset| {
        usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .saturating_add(ANCHOR_ABOVE)
    });
    let screen = read_screen(want, page).ok_or(PrefaceReason::SeamNotFound)?;
    let anchor = anchor(&screen, top).ok_or(PrefaceReason::SeamNotFound)?;
    let seam = place(turns, screen.rows[..=anchor].iter().map(String::as_str))
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

/// Read the screen from the bottom up, far enough to hold `want` lines or
/// to its top when no line count is known.
///
/// Pages overlap by one line, exactly as a client's do. When the line two
/// pages share differs, the tail moved between them and every offset read
/// so far is off by the same amount, so the screen is read again from the
/// bottom rather than stitched across the move.
fn read_screen(want: Option<usize>, mut page: impl FnMut(u64) -> Option<Page>) -> Option<Screen> {
    'attempt: for _ in 0..SCREEN_ATTEMPTS {
        let mut rows: Vec<String> = Vec::new();
        let mut offset = 0u64;
        loop {
            let read = page(offset)?;
            let lines = page_lines(&read.text);
            let landed = usize::try_from(read.effective_offset).ok()?;
            for (from_bottom, line) in lines.iter().rev().enumerate() {
                let index = landed + from_bottom;
                let line = row(line);
                match index.cmp(&rows.len()) {
                    std::cmp::Ordering::Less if rows[index] != line => continue 'attempt,
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => rows.push(line),
                    // The server stepped past lines this never saw.
                    std::cmp::Ordering::Greater => return None,
                }
            }
            if !read.has_more {
                return Some(Screen { rows, top: true });
            }
            if want.is_some_and(|want| rows.len() > want) || rows.len() >= MAX_SCREEN_LINES {
                return Some(Screen { rows, top: false });
            }
            let next = read.effective_offset + lines.len().saturating_sub(1).max(1) as u64;
            if next <= offset || lines.is_empty() {
                return None;
            }
            offset = next;
        }
    }
    None
}

/// A page's lines, split the way a client splits them.
fn page_lines(text: &str) -> Vec<&str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').collect()
}

/// One screen line as the two sides compare it: no styling, no carriage
/// returns, no trailing blanks.
fn row(line: &str) -> String {
    strip_ansi(line).replace('\r', "").trim_end().to_string()
}

/// Where in the screen the client's oldest held line is now.
///
/// Its offset is tried first, then the lines around it nearest first, so
/// an unmoved screen anchors exactly even on a line that repeats. A line
/// that repeats nearby — a blank one, most often — on a screen that moved
/// anchors on its nearest copy, which is off by no more than the distance
/// between the two.
fn anchor(screen: &Screen, top: Top<'_>) -> Option<usize> {
    let Some(offset) = top.offset else {
        return (screen.top && !screen.rows.is_empty()).then(|| screen.rows.len() - 1);
    };
    let wanted = row(top.line?);
    let offset = usize::try_from(offset).ok()?;
    let at = |index: usize| screen.rows.get(index).is_some_and(|line| *line == wanted);
    (0..=ANCHOR_ABOVE.max(ANCHOR_BELOW)).find_map(|distance| {
        let above = offset.checked_add(distance);
        let below = offset.checked_sub(distance);
        if distance <= ANCHOR_ABOVE && above.is_some_and(at) {
            above
        } else if distance > 0 && distance <= ANCHOR_BELOW && below.is_some_and(at) {
            below
        } else {
            None
        }
    })
}

/// The index of the oldest turn whose prompt the held rows show, when the
/// record's prompts line up with them at all.
///
/// Rows are walked from the bottom up and prompts from the newest back, so
/// a prompt that repeats is matched where its neighbours say it is. A
/// record prompt the screen does not show (an image paste, a prompt drawn
/// some other way) is stepped over within a few prompts of the last one
/// that lined up; a screen row that matches no prompt is just a row.
fn place<'a>(turns: &[Turn], held_bottom_first: impl Iterator<Item = &'a str>) -> Option<usize> {
    let prompted: Vec<usize> = turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| turn.start == TurnStart::Prompt && turn.head.is_some())
        .map(|(index, _)| index)
        .collect();
    let mut remaining = prompted.len();
    let mut seam = None;
    for line in held_bottom_first {
        if remaining == 0 {
            break;
        }
        let line = prompt_row(line);
        if line.is_empty() {
            continue;
        }
        for back in 0..LOOKBACK.min(remaining) {
            let candidate = prompted[remaining - 1 - back];
            if turns[candidate]
                .head
                .as_deref()
                .is_some_and(|head| shows(&line, head))
            {
                seam = Some(candidate);
                remaining -= back + 1;
                break;
            }
        }
    }
    seam
}

/// A row as a prompt would read in it: one row of text, without the glyph
/// a harness draws ahead of the person's words.
fn prompt_row(line: &str) -> String {
    let line = crate::prompt_trail::clean(line);
    for glyph in ["❯", "›", ">"] {
        if let Some(rest) = line.strip_prefix(glyph) {
            if rest.is_empty() || rest.starts_with(' ') {
                return rest.trim_start().to_string();
            }
        }
    }
    line
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

/// A line without its escape sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.next() {
            // A control sequence ends at its final byte.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // An operating-system command ends at a bell or a string
            // terminator.
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
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(prompt_row(">not a glyph"), ">not a glyph");
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
        assert_eq!(place(&turns, held.iter().rev().copied()), Some(2));
        // And all four.
        let held = ["❯ continue", "❯ map the piers", "❯ continue", "❯ continue"];
        assert_eq!(place(&turns, held.iter().rev().copied()), Some(0));
        // None of them.
        assert_eq!(place(&turns, ["❯ something else"].into_iter()), None);
    }
}
