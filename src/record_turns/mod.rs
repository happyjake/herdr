//! Turns read whole out of a pane's session record — each one what started
//! it, the prompt as written and the final answer as written.
//!
//! A reader per harness turns one record into turns oldest first. Unlike the
//! turn index, which keeps one line of each, a reader here keeps the full
//! prompt and the full final answer, with their line structure, and nothing
//! else: no tool traffic, no thinking, no text the harness injected on the
//! person's behalf, and no compaction summary.
//!
//! The newest turn may still be running and is reported open; deciding what
//! of these turns to send is the caller's business.

pub(crate) mod codex;
pub(crate) mod pi;

use crate::api::schema::TurnEnd;
use crate::prompt_trail::clean;

/// What started a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordStart {
    /// Someone typed a prompt.
    Prompt,
    /// The harness woke the agent on a timer.
    Wake,
    /// Something other than a person or a wake did — a background task
    /// finishing, a sub-agent reporting back.
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordTurn {
    /// Stable for an append-only record: the harness's own turn or entry id.
    pub(crate) id: String,
    /// Start time in unix seconds.
    pub(crate) at: u64,
    pub(crate) start: RecordStart,
    /// Full prompt text; none for a wake or a task.
    pub(crate) prompt: Option<String>,
    /// The prompt's first non-empty line, whitespace-collapsed: what a
    /// screen shows of it, for placing it there.
    pub(crate) prompt_head: Option<String>,
    pub(crate) end: TurnEnd,
    /// Full final-answer text; none unless the turn was answered.
    pub(crate) answer: Option<String>,
}

impl RecordTurn {
    fn new(id: String, at: u64, start: RecordStart, prompt: Option<String>) -> Self {
        let prompt_head = prompt.as_deref().and_then(head_line);
        Self {
            id,
            at,
            start,
            prompt,
            prompt_head,
            end: TurnEnd::Open,
            answer: None,
        }
    }
}

/// A prompt or an answer as text to quote: line breaks kept, carriage
/// returns and other control characters dropped, outer blank space
/// trimmed. Nothing for text that is only blank.
fn quoted(text: &str) -> Option<String> {
    let kept: String = text
        .chars()
        .filter(|character| matches!(character, '\n' | '\t') || !character.is_control())
        .collect();
    let trimmed = kept.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn head_line(text: &str) -> Option<String> {
    text.lines().map(clean).find(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_turns_quote_keeps_lines_and_drops_control_characters() {
        assert_eq!(
            quoted("\r\n  First line\r\n\n| a | b |\u{7}\n\t- item  \n").as_deref(),
            Some("First line\n\n| a | b |\n\t- item")
        );
        assert_eq!(quoted(" \n\t\r "), None);
    }

    #[test]
    fn record_turns_head_is_the_first_non_empty_line_collapsed() {
        let turn = RecordTurn::new(
            "t".into(),
            0,
            RecordStart::Prompt,
            Some("\n   \n  Fix   the\tlayout\nthen ship".into()),
        );
        assert_eq!(turn.prompt_head.as_deref(), Some("Fix the layout"));
    }
}
