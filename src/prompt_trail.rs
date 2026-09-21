//! Prompt trails — what a pane's session record says was asked of it.
//!
//! Every harness keeps its own file of one conversation on this desk. This
//! reads the few things in one that say what the work is: the first real
//! prompt, the last three, the harness's own title, how many prompts there
//! have been and when the newest arrived. Nothing else in a record is a
//! trail's business — not the agent's tool traffic, not its results, and
//! not what the harness injected on the person's behalf.
//!
//! The read is bounded at every step rather than best-effort. The file is
//! never loaded whole: it is walked a line at a time, a line past its own
//! cap is stepped over rather than parsed, and a file past the record cap
//! is not opened at all. A line that is no JSON is skipped the way the
//! harness's own half-written last line has to be; a record whose lines
//! all say nothing is an empty trail, which is a different answer from no
//! record.
//!
//! Where a record lives is the session identity's business, not a search:
//! a path the harness itself reported wins, and otherwise the file is
//! derived from the id in the one layout that harness writes.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{PromptTrail, PromptTrailAgent, PromptTrailReason, TrailPrompt};

/// Largest record this opens. A file past it is a record this server
/// declines to read rather than one it reads slowly.
pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// Longest single line this parses. A prompt bigger than this says nothing
/// a name could be made of, and holding one in memory to find that out is
/// the cost the bound exists to refuse.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Code points the first prompt is cut to.
const FIRST_PROMPT_CHARS: usize = 400;

/// Code points each recent prompt is cut to.
const RECENT_PROMPT_CHARS: usize = 240;

/// Recent prompts a trail carries.
const RECENT_PROMPTS: usize = 3;

/// Directories one search for a record walks before it gives up. The
/// layouts below are dated, so the bound cuts off old history rather than
/// the session anyone is asking about.
const MAX_RECORD_DIRS_SCANNED: usize = 2048;

/// Entries one search reads out of a single directory.
const MAX_RECORD_ENTRIES_SCANNED: usize = 4096;

/// A title the harness writes for every conversation, which therefore says
/// nothing about this one.
const CLAUDE_FALLBACK_TITLE: &str = "Claude Code";

/// Texts a claude record carries as a user turn that no person typed.
const CLAUDE_SKIPPED_PREFIXES: &[&str] = &[
    "<local-command-stdout>",
    "<system-reminder>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
    "<task-notification>",
    "[Request interrupted",
    "This session is being continued",
];

/// The same for a codex record: its preamble, its context, and the turns
/// it writes about itself.
const CODEX_SKIPPED_PREFIXES: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<user_instructions>",
    "<permissions",
    "<turn_aborted",
    "<collaboration_mode",
];

/// The harness a session belongs to, when its records are ones this reads.
pub(crate) fn trail_agent(agent: &str) -> Option<PromptTrailAgent> {
    match agent {
        "claude" => Some(PromptTrailAgent::Claude),
        "codex" => Some(PromptTrailAgent::Codex),
        "pi" => Some(PromptTrailAgent::Pi),
        _ => None,
    }
}

/// The record file one session names, or nothing when this desk holds no
/// such file.
///
/// A path the harness reported beats a derived one: the harness knows
/// where it is writing, and a derivation is only ever a good guess about a
/// layout.
pub(crate) fn record_path(
    home: Option<&Path>,
    agent: PromptTrailAgent,
    kind: AgentSessionRefKind,
    value: &str,
    reported: Option<&str>,
) -> Option<PathBuf> {
    if let Some(reported) = reported.map(Path::new).filter(|path| path.is_file()) {
        return Some(reported.to_path_buf());
    }

    match (agent, kind) {
        (PromptTrailAgent::Pi, AgentSessionRefKind::Path) => {
            let path = Path::new(value);
            path.is_file().then(|| path.to_path_buf())
        }
        (PromptTrailAgent::Claude, AgentSessionRefKind::Id) => claude_record(home?, value),
        (PromptTrailAgent::Codex, AgentSessionRefKind::Id) => codex_record(home?, value),
        _ => None,
    }
}

/// Read one record into the trail it holds.
pub(crate) fn read(path: &Path, agent: PromptTrailAgent) -> Result<PromptTrail, PromptTrailReason> {
    let metadata = std::fs::metadata(path).map_err(|_| PromptTrailReason::NoRecord)?;
    if !metadata.is_file() {
        return Err(PromptTrailReason::NoRecord);
    }
    if metadata.len() > MAX_RECORD_BYTES {
        return Err(PromptTrailReason::Unreadable);
    }
    let file = std::fs::File::open(path).map_err(|_| PromptTrailReason::Unreadable)?;
    read_lines(std::io::BufReader::new(file), agent)
}

fn read_lines(
    mut reader: impl BufRead,
    agent: PromptTrailAgent,
) -> Result<PromptTrail, PromptTrailReason> {
    let mut first: Option<TrailPrompt> = None;
    // The first prompt whole, only so a title can be judged against what it
    // would be repeating rather than against a cut of it.
    let mut first_whole = String::new();
    let mut recent: Vec<TrailPrompt> = Vec::with_capacity(RECENT_PROMPTS);
    let mut title: Option<String> = None;
    let mut count: u32 = 0;
    let mut newest_at: Option<u64> = None;

    let mut line = Vec::new();
    loop {
        match read_line_bounded(&mut reader, &mut line) {
            Ok(LineRead::End) => break,
            // A line past the cap is stepped over whole: it is neither a
            // prompt this counts nor a reason to abandon the record.
            Ok(LineRead::TooLong) => continue,
            Ok(LineRead::Line) => {}
            Err(_) => return Err(PromptTrailReason::Unreadable),
        }
        // A record's last line is often half written, and a harness may log
        // anything at all; neither is a record this cannot read.
        let Ok(parsed) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };

        if let Some(found) = claude_title(agent, &parsed) {
            title = Some(found);
        }

        let Some(text) = prompt_text(agent, &parsed) else {
            continue;
        };
        let at = parsed
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(unix_seconds_from_rfc3339);

        count = count.saturating_add(1);
        newest_at = at;
        if first.is_none() {
            first = Some(TrailPrompt {
                text: cut(&text, FIRST_PROMPT_CHARS),
                at,
            });
            first_whole = text.clone();
        }
        if recent.len() == RECENT_PROMPTS {
            recent.remove(0);
        }
        recent.push(TrailPrompt {
            text: cut(&text, RECENT_PROMPT_CHARS),
            at,
        });
    }

    // A title that is the harness's own name, or the first prompt said
    // again, tells a reader nothing the trail does not already carry.
    let title = title.filter(|title| {
        !title.eq_ignore_ascii_case(CLAUDE_FALLBACK_TITLE)
            && !first_whole.eq_ignore_ascii_case(title)
    });

    Ok(PromptTrail {
        agent,
        first,
        recent,
        title,
        count,
        newest_at,
    })
}

/// What one line of a record says was asked, when it says anything.
fn prompt_text(agent: PromptTrailAgent, line: &Value) -> Option<String> {
    match agent {
        PromptTrailAgent::Claude => claude_prompt(line),
        PromptTrailAgent::Codex => codex_prompt(line),
        PromptTrailAgent::Pi => pi_prompt(line),
    }
}

fn claude_prompt(line: &Value) -> Option<String> {
    if line.get("type")?.as_str()? != "user" {
        return None;
    }
    // What the harness put in the person's mouth: a reminder it attached, a
    // summary it wrote when the conversation was compacted.
    if flag(line, "isMeta") || flag(line, "isCompactSummary") {
        return None;
    }
    let message = line.get("message")?;
    if message.get("role")?.as_str()? != "user" {
        return None;
    }

    let text = match message.get("content")? {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            // A turn carrying a tool's answer is the agent's traffic wearing
            // the person's role, whatever text rides beside it.
            if blocks
                .iter()
                .any(|block| block_kind(block) == Some("tool_result"))
            {
                return None;
            }
            join_text_blocks(blocks, "text")
        }
        _ => return None,
    };
    let text = clean(&text);

    if text.starts_with("<command-message>") {
        return claude_command_prompt(&text);
    }
    if CLAUDE_SKIPPED_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return None;
    }
    (!text.is_empty()).then_some(text)
}

/// A slash command, read back as it was typed.
///
/// A command with no words is the person working the harness rather than
/// asking it for anything, so it is no prompt at all.
fn claude_command_prompt(text: &str) -> Option<String> {
    let name = tag_contents(text, "command-name")?
        .trim()
        .trim_start_matches('/')
        .trim();
    let args = tag_contents(text, "command-args")
        .unwrap_or_default()
        .trim();
    if name.is_empty() || args.is_empty() {
        return None;
    }
    Some(format!("/{name} {args}"))
}

fn codex_prompt(line: &Value) -> Option<String> {
    if line.get("type")?.as_str()? != "response_item" {
        return None;
    }
    let payload = line.get("payload")?;
    if payload.get("type")?.as_str()? != "message" || payload.get("role")?.as_str()? != "user" {
        return None;
    }
    let text = clean(&join_text_blocks(
        payload.get("content")?.as_array()?,
        "input_text",
    ));
    if CODEX_SKIPPED_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return None;
    }
    let text = clean(&replace_image_tags(&text));
    (!text.is_empty()).then_some(text)
}

fn pi_prompt(line: &Value) -> Option<String> {
    if line.get("type")?.as_str()? != "message" {
        return None;
    }
    let message = line.get("message")?;
    if message.get("role")?.as_str()? != "user" {
        return None;
    }
    let text = match message.get("content")? {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => join_text_blocks(blocks, "text"),
        _ => return None,
    };
    let text = clean(&text);
    (!text.is_empty()).then_some(text)
}

/// The title a claude record carries, before it is judged.
fn claude_title(agent: PromptTrailAgent, line: &Value) -> Option<String> {
    if agent != PromptTrailAgent::Claude || line.get("type")?.as_str()? != "ai-title" {
        return None;
    }
    let title = clean(line.get("aiTitle")?.as_str()?);
    (!title.is_empty()).then_some(title)
}

fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn block_kind(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn join_text_blocks(blocks: &[Value], kind: &str) -> String {
    blocks
        .iter()
        .filter(|block| block_kind(block) == Some(kind))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tag_contents<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let start = text.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = text[start..].find(&format!("</{tag}>"))? + start;
    Some(&text[start..end])
}

/// An attached image reads as one, wherever in the text it was written.
fn replace_image_tags(text: &str) -> String {
    const OPEN: &str = "<image";
    let mut replaced = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        // A word that merely begins the same way is not a tag.
        if !matches!(after.chars().next(), Some('>') | Some(' ')) {
            let (head, tail) = rest.split_at(start + OPEN.len());
            replaced.push_str(head);
            rest = tail;
            continue;
        }
        let Some(end) = after.find('>') else {
            break;
        };
        replaced.push_str(&rest[..start]);
        replaced.push_str("[image]");
        rest = &after[end + 1..];
    }
    replaced.push_str(rest);
    replaced
}

/// One prompt as a row of text: whitespace runs become one space, control
/// characters go, and what is left is trimmed.
fn clean(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !cleaned.is_empty();
            continue;
        }
        if character.is_control() {
            continue;
        }
        if pending_space {
            cleaned.push(' ');
            pending_space = false;
        }
        cleaned.push(character);
    }
    cleaned
}

/// Cut to a length, at a word boundary when one is near enough the end
/// that keeping it still leaves most of the cut standing.
fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    let kept = match head.rfind(' ') {
        Some(index) if head[..index].chars().count() * 2 >= limit => &head[..index],
        _ => head.as_str(),
    };
    format!("{}…", kept.trim_end())
}

enum LineRead {
    Line,
    TooLong,
    End,
}

/// Read one line, holding what is kept of it to [`MAX_LINE_BYTES`].
///
/// A line past the cap is still walked to its end — the next line has to
/// start where it really starts — but nothing of it is kept, so a record
/// holding one enormous line costs the cap rather than the line.
fn read_line_bounded(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<LineRead> {
    line.clear();
    let mut over_cap = false;
    let mut read_anything = false;

    loop {
        let (ended, consumed) = {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            if available.is_empty() {
                break;
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    keep_bounded(line, &available[..index], &mut over_cap);
                    (true, index + 1)
                }
                None => {
                    keep_bounded(line, available, &mut over_cap);
                    (false, available.len())
                }
            }
        };
        reader.consume(consumed);
        read_anything = true;
        if ended {
            return Ok(line_read(over_cap));
        }
    }

    if !read_anything {
        return Ok(LineRead::End);
    }
    Ok(line_read(over_cap))
}

fn line_read(over_cap: bool) -> LineRead {
    if over_cap {
        LineRead::TooLong
    } else {
        LineRead::Line
    }
}

fn keep_bounded(line: &mut Vec<u8>, chunk: &[u8], over_cap: &mut bool) {
    if *over_cap {
        return;
    }
    if line.len() + chunk.len() > MAX_LINE_BYTES {
        *over_cap = true;
        line.clear();
        return;
    }
    line.extend_from_slice(chunk);
}

/// The claude record for one session id: a file of that name filed under
/// one of the project directories.
fn claude_record(home: &Path, id: &str) -> Option<PathBuf> {
    if !safe_file_stem(id) {
        return None;
    }
    let file = format!("{id}.jsonl");
    let projects = std::fs::read_dir(home.join(".claude").join("projects")).ok()?;
    for entry in projects.take(MAX_RECORD_ENTRIES_SCANNED).flatten() {
        let candidate = entry.path().join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The codex record for one thread id: a rollout filed under the date it
/// was started, its name carrying the id.
fn codex_record(home: &Path, id: &str) -> Option<PathBuf> {
    if !safe_file_stem(id) {
        return None;
    }
    let mut scanned = 0;
    rollout_under(
        &home.join(".codex").join("sessions"),
        3,
        &format!("-{id}.jsonl"),
        &mut scanned,
    )
}

fn rollout_under(dir: &Path, depth: usize, suffix: &str, scanned: &mut usize) -> Option<PathBuf> {
    if *scanned >= MAX_RECORD_DIRS_SCANNED {
        return None;
    }
    *scanned += 1;
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .take(MAX_RECORD_ENTRIES_SCANNED)
        .flatten()
        .map(|entry| entry.path())
        .collect();

    if depth == 0 {
        return children.into_iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(suffix))
        });
    }

    // The layout is dated, so the name sorts the way the clock does and the
    // newest buckets are the ones the bound above spends its walk on.
    children.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    children
        .into_iter()
        .filter(|path| path.is_dir())
        .find_map(|path| rollout_under(&path, depth - 1, suffix, scanned))
}

/// Whether an id may be spelled into a path. A session id is a name, never
/// a route to somewhere else on the disk.
fn safe_file_stem(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// Unix seconds for the times a harness writes, which are RFC 3339.
///
/// Written out rather than borrowed: the one shape that matters here is a
/// date, a time and an offset, and a time before the epoch is no time a
/// prompt was asked at.
fn unix_seconds_from_rfc3339(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // The fraction of a second is stepped over: a trail dates prompts to
    // the second.
    let rest = text
        .get(19..)?
        .trim_start_matches(|character: char| character == '.' || character.is_ascii_digit());
    let offset_seconds = match rest.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let digits: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            if digits.len() < 4 {
                return None;
            }
            let hours: i64 = digits.get(0..2)?.parse().ok()?;
            let minutes: i64 = digits.get(2..4)?.parse().ok()?;
            let magnitude = hours * 3600 + minutes * 60;
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
        - offset_seconds;
    u64::try_from(seconds).ok()
}

/// Days from the unix epoch to a civil date, by the usual shifted-era
/// arithmetic: March-first years, so a leap day falls at the end of one.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway home holding real records in the layouts the harnesses
    /// write, so what is found is answered by a filesystem rather than by a
    /// stub. Nothing here names anything on the machine running the test.
    pub(crate) struct FixtureHome {
        root: PathBuf,
    }

    impl FixtureHome {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "herdr-prompt-trail-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("fixture home");
            Self { root }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.root
        }

        pub(crate) fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture parent");
            }
            std::fs::write(&path, contents).expect("fixture record");
            path
        }
    }

    impl Drop for FixtureHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn lines(lines: &[serde_json::Value]) -> String {
        lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn claude_user(text: &str, at: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
            "timestamp": at,
        })
    }

    fn claude_record_lines() -> String {
        lines(&[
            claude_user(
                "why does the beacon relay drop every third frame",
                "2026-03-04T09:15:00.120Z",
            ),
            serde_json::json!({
                "type": "user",
                "isMeta": true,
                "message": { "role": "user", "content": "the messages below were generated by a hook" },
                "timestamp": "2026-03-04T09:15:01.000Z",
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "looking" }] },
                "timestamp": "2026-03-04T09:15:02.000Z",
            }),
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "content": "frames: 3" },
                    { "type": "text", "text": "still dropping" },
                ]},
                "timestamp": "2026-03-04T09:16:00.000Z",
            }),
            claude_user(
                "<local-command-stdout>relay frames: 3</local-command-stdout>",
                "2026-03-04T09:17:00.000Z",
            ),
            claude_user(
                "<command-message>model is running…</command-message>\n<command-name>/model</command-name>\n<command-args></command-args>",
                "2026-03-04T09:18:00.000Z",
            ),
            claude_user(
                "<command-message>deploy is running…</command-message>\n<command-name>/deploy</command-name>\n<command-args>beacon-relay staging</command-args>",
                "2026-03-04T09:19:00.000Z",
            ),
            serde_json::json!({
                "type": "ai-title",
                "aiTitle": "Claude Code",
                "timestamp": "2026-03-04T09:19:30.000Z",
            }),
            serde_json::json!({
                "type": "ai-title",
                "aiTitle": "Beacon relay frame drops",
                "timestamp": "2026-03-04T09:20:00.000Z",
            }),
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "text", "text": "pin it to  the encoder\nthen" }] },
                "timestamp": "2026-03-04T09:40:12Z",
            }),
            // A half-written last line, which is what a live record looks
            // like while the harness is still writing it.
            serde_json::json!({ "type": "user", "message": { "role": "user" } }),
        ]) + "{\"type\":\"user\",\"mess"
    }

    fn codex_record_lines() -> String {
        lines(&[
            serde_json::json!({
                "timestamp": "2026-03-05T08:00:00.000Z",
                "type": "session_meta",
                "payload": { "id": "7c0de401", "cwd": "/invented/lantern" },
            }),
            serde_json::json!({
                "timestamp": "2026-03-05T08:00:01.000Z",
                "type": "response_item",
                "payload": { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "# AGENTS.md instructions\n\nkeep answers short" },
                ]},
            }),
            serde_json::json!({
                "timestamp": "2026-03-05T08:00:02.000Z",
                "type": "response_item",
                "payload": { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "<environment_context>cwd /invented/lantern</environment_context>" },
                ]},
            }),
            serde_json::json!({
                "timestamp": "2026-03-05T08:01:00.000Z",
                "type": "response_item",
                "payload": { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "rewrite the lantern parser so it streams" },
                ]},
            }),
            serde_json::json!({
                "timestamp": "2026-03-05T08:02:00.000Z",
                "type": "response_item",
                "payload": { "type": "message", "role": "assistant", "content": [
                    { "type": "output_text", "text": "on it" },
                ]},
            }),
            serde_json::json!({
                "timestamp": "2026-03-05T08:09:00.000Z",
                "type": "response_item",
                "payload": { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "here is the trace <image 1> and the log" },
                ]},
            }),
        ])
    }

    fn pi_record_lines() -> String {
        lines(&[
            serde_json::json!({
                "type": "message",
                "timestamp": "2026-03-06T11:00:00Z",
                "message": { "role": "user", "content": "start the tide gauge importer" },
            }),
            serde_json::json!({
                "type": "message",
                "timestamp": "2026-03-06T11:01:00Z",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "started" }] },
            }),
            serde_json::json!({
                "type": "message",
                "timestamp": "2026-03-06T11:05:00Z",
                "message": { "role": "user", "content": [
                    { "type": "thinking", "text": "not what was asked" },
                    { "type": "text", "text": "now chunk it by station" },
                ]},
            }),
        ])
    }

    fn texts(prompts: &[TrailPrompt]) -> Vec<&str> {
        prompts.iter().map(|prompt| prompt.text.as_str()).collect()
    }

    #[test]
    fn a_claude_record_reads_what_was_asked_and_nothing_else() {
        let home = FixtureHome::new();
        let record = home.write("records/claude.jsonl", &claude_record_lines());

        let trail = read(&record, PromptTrailAgent::Claude).expect("a readable record");

        assert_eq!(trail.agent, PromptTrailAgent::Claude);
        assert_eq!(trail.count, 3);
        assert_eq!(
            trail.first.as_ref().map(|first| first.text.as_str()),
            Some("why does the beacon relay drop every third frame")
        );
        // The tool result, the hook's own turn, the command output and the
        // bare slash command are all the harness talking, not the person.
        assert_eq!(
            texts(&trail.recent),
            vec![
                "why does the beacon relay drop every third frame",
                "/deploy beacon-relay staging",
                "pin it to the encoder then",
            ]
        );
        assert_eq!(trail.title.as_deref(), Some("Beacon relay frame drops"));
        assert_eq!(trail.newest_at, Some(1772617212));
        assert_eq!(
            trail.first.as_ref().and_then(|first| first.at),
            Some(1772615700)
        );
    }

    #[test]
    fn a_claude_title_that_says_nothing_new_is_no_title() {
        let home = FixtureHome::new();

        let only_the_harness = home.write(
            "records/harness.jsonl",
            &lines(&[
                claude_user("trace the ferry timetable import", "2026-03-04T09:15:00Z"),
                serde_json::json!({ "type": "ai-title", "aiTitle": "Claude Code" }),
            ]),
        );
        assert_eq!(
            read(&only_the_harness, PromptTrailAgent::Claude)
                .expect("a readable record")
                .title,
            None
        );

        let echoing_the_prompt = home.write(
            "records/echo.jsonl",
            &lines(&[
                claude_user("trace the ferry timetable import", "2026-03-04T09:15:00Z"),
                serde_json::json!({ "type": "ai-title", "aiTitle": "Trace The Ferry Timetable Import" }),
            ]),
        );
        assert_eq!(
            read(&echoing_the_prompt, PromptTrailAgent::Claude)
                .expect("a readable record")
                .title,
            None
        );

        let its_own = home.write(
            "records/own.jsonl",
            &lines(&[
                claude_user("trace the ferry timetable import", "2026-03-04T09:15:00Z"),
                serde_json::json!({ "type": "ai-title", "aiTitle": "Ferry timetable import" }),
            ]),
        );
        assert_eq!(
            read(&its_own, PromptTrailAgent::Claude)
                .expect("a readable record")
                .title
                .as_deref(),
            Some("Ferry timetable import")
        );
    }

    #[test]
    fn a_codex_record_skips_its_preamble_and_reads_an_image_as_one() {
        let home = FixtureHome::new();
        let record = home.write("records/rollout.jsonl", &codex_record_lines());

        let trail = read(&record, PromptTrailAgent::Codex).expect("a readable record");

        assert_eq!(trail.agent, PromptTrailAgent::Codex);
        assert_eq!(trail.count, 2);
        assert_eq!(
            trail.first.as_ref().map(|first| first.text.as_str()),
            Some("rewrite the lantern parser so it streams")
        );
        assert_eq!(
            texts(&trail.recent),
            vec![
                "rewrite the lantern parser so it streams",
                "here is the trace [image] and the log",
            ]
        );
        // The web already holds codex's own terminal title; the record's is
        // none of a trail's business.
        assert_eq!(trail.title, None);
        assert_eq!(trail.newest_at, Some(1772698140));
    }

    #[test]
    fn a_pi_record_reads_a_string_turn_and_a_block_turn_alike() {
        let home = FixtureHome::new();
        let record = home.write("records/pi.jsonl", &pi_record_lines());

        let trail = read(&record, PromptTrailAgent::Pi).expect("a readable record");

        assert_eq!(trail.agent, PromptTrailAgent::Pi);
        assert_eq!(trail.count, 2);
        assert_eq!(
            texts(&trail.recent),
            vec!["start the tide gauge importer", "now chunk it by station"]
        );
        assert_eq!(trail.title, None);
        assert_eq!(trail.newest_at, Some(1772795100));
    }

    #[test]
    fn a_trail_keeps_only_the_last_three_prompts() {
        let home = FixtureHome::new();
        let record = home.write(
            "records/many.jsonl",
            &lines(
                &(1..=5)
                    .map(|turn| {
                        claude_user(
                            &format!("turn {turn}"),
                            &format!("2026-03-04T09:0{turn}:00Z"),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
        );

        let trail = read(&record, PromptTrailAgent::Claude).expect("a readable record");

        assert_eq!(trail.count, 5);
        assert_eq!(
            trail.first.as_ref().map(|first| first.text.as_str()),
            Some("turn 1")
        );
        assert_eq!(texts(&trail.recent), vec!["turn 3", "turn 4", "turn 5"]);
    }

    #[test]
    fn a_record_of_nothing_asked_is_an_empty_trail_and_not_a_missing_one() {
        let home = FixtureHome::new();
        let record = home.write(
            "records/quiet.jsonl",
            &lines(&[serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
            })]),
        );

        let trail = read(&record, PromptTrailAgent::Claude).expect("a readable record");

        assert_eq!(trail.count, 0);
        assert_eq!(trail.first, None);
        assert!(trail.recent.is_empty());
        assert_eq!(trail.newest_at, None);
    }

    #[test]
    fn a_record_past_the_size_bound_is_unreadable_and_a_missing_one_is_absent() {
        let home = FixtureHome::new();

        let oversize = home.write("records/huge.jsonl", "");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&oversize)
            .expect("fixture record")
            .set_len(MAX_RECORD_BYTES + 1)
            .expect("a record past the bound");
        assert_eq!(
            read(&oversize, PromptTrailAgent::Claude),
            Err(PromptTrailReason::Unreadable)
        );

        assert_eq!(
            read(
                &home.path().join("records/never-written.jsonl"),
                PromptTrailAgent::Claude
            ),
            Err(PromptTrailReason::NoRecord)
        );
    }

    #[test]
    fn a_line_past_its_own_bound_is_stepped_over_whole() {
        let home = FixtureHome::new();
        let enormous = claude_user(&"x".repeat(MAX_LINE_BYTES + 16), "2026-03-04T09:15:00Z");
        let record = home.write(
            "records/long-line.jsonl",
            &lines(&[
                claude_user("first, the short one", "2026-03-04T09:14:00Z"),
                enormous,
                claude_user("and the short one after it", "2026-03-04T09:16:00Z"),
            ]),
        );

        let trail = read(&record, PromptTrailAgent::Claude).expect("a readable record");

        assert_eq!(trail.count, 2);
        assert_eq!(
            texts(&trail.recent),
            vec!["first, the short one", "and the short one after it"]
        );
    }

    #[test]
    fn a_claude_record_is_found_under_its_project_directory() {
        let home = FixtureHome::new();
        let id = "5f2a9c11-0b44-4d8e-9a10-6c3b7e5d1f22";
        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Claude,
                AgentSessionRefKind::Id,
                id,
                None
            ),
            None,
            "a record nobody has written is no record"
        );

        let written = home.write(&format!(".claude/projects/-invented-beacon/{id}.jsonl"), "");
        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Claude,
                AgentSessionRefKind::Id,
                id,
                None
            ),
            Some(written)
        );
    }

    #[test]
    fn a_path_the_harness_reported_beats_one_derived_from_the_id() {
        let home = FixtureHome::new();
        let id = "5f2a9c11-0b44-4d8e-9a10-6c3b7e5d1f22";
        home.write(&format!(".claude/projects/-invented-beacon/{id}.jsonl"), "");
        let reported = home.write("elsewhere/moved.jsonl", "");

        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Claude,
                AgentSessionRefKind::Id,
                id,
                reported.to_str(),
            ),
            Some(reported)
        );

        // A reported path that is not there falls back to the derivation
        // rather than answering nothing.
        assert!(record_path(
            Some(home.path()),
            PromptTrailAgent::Claude,
            AgentSessionRefKind::Id,
            id,
            home.path().join("elsewhere/gone.jsonl").to_str(),
        )
        .is_some_and(|path| path.ends_with(format!("{id}.jsonl"))));
    }

    #[test]
    fn an_id_that_is_a_route_somewhere_else_names_no_record() {
        let home = FixtureHome::new();
        home.write(".claude/projects/-invented-beacon/secret.jsonl", "");

        for id in ["../secret", "-invented-beacon/secret", "a/../../secret", ""] {
            assert_eq!(
                record_path(
                    Some(home.path()),
                    PromptTrailAgent::Claude,
                    AgentSessionRefKind::Id,
                    id,
                    None
                ),
                None,
                "id {id:?} was spelled into a path"
            );
        }
    }

    #[test]
    fn a_codex_record_is_found_by_the_id_in_its_rollout_name() {
        let home = FixtureHome::new();
        let id = "0199f3c2b7d84a1e9c05";
        let written = home.write(
            &format!(".codex/sessions/2026/03/05/rollout-2026-03-05T08-00-00-{id}.jsonl"),
            "",
        );

        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Codex,
                AgentSessionRefKind::Id,
                id,
                None
            ),
            Some(written)
        );
        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Codex,
                AgentSessionRefKind::Id,
                "0199f3c2b7d84a1e9c06",
                None
            ),
            None
        );
    }

    #[test]
    fn a_pi_record_is_the_file_the_session_names() {
        let home = FixtureHome::new();
        let written = home.write(".pi/agent/sessions/tide-gauge.jsonl", "");

        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Pi,
                AgentSessionRefKind::Path,
                written.to_str().expect("a utf-8 fixture path"),
                None,
            ),
            Some(written)
        );
        assert_eq!(
            record_path(
                Some(home.path()),
                PromptTrailAgent::Pi,
                AgentSessionRefKind::Path,
                home.path()
                    .join(".pi/agent/sessions/gone.jsonl")
                    .to_str()
                    .expect("a utf-8 fixture path"),
                None,
            ),
            None
        );
    }

    #[test]
    fn only_the_harnesses_whose_records_this_reads_are_named() {
        assert_eq!(trail_agent("claude"), Some(PromptTrailAgent::Claude));
        assert_eq!(trail_agent("codex"), Some(PromptTrailAgent::Codex));
        assert_eq!(trail_agent("pi"), Some(PromptTrailAgent::Pi));
        for other in ["omp", "copilot", "droid", "", "Claude"] {
            assert_eq!(trail_agent(other), None, "agent {other:?}");
        }
    }

    #[test]
    fn a_prompt_is_one_row_of_text_however_it_was_written() {
        assert_eq!(clean("  one\n\ttwo   three \n"), "one two three");
        assert_eq!(clean("\u{7}bell\u{1b}[0m"), "bell[0m");
        assert_eq!(clean("   \n  "), "");
    }

    #[test]
    fn a_long_prompt_is_cut_at_a_word_and_says_that_it_was() {
        assert_eq!(cut("short enough", 40), "short enough");
        assert_eq!(cut("one two three four", 11), "one two…");
        // A single word with no boundary worth keeping is cut where the
        // limit falls rather than back to almost nothing.
        assert_eq!(cut("a bbbbbbbbbbbbbbbb", 8), "a bbbbbb…");
        assert_eq!(cut("ありがとう ございます", 6), "ありがとう…");
    }

    #[test]
    fn the_times_a_harness_writes_read_as_unix_seconds() {
        assert_eq!(unix_seconds_from_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            unix_seconds_from_rfc3339("2026-03-04T09:15:00.120Z"),
            Some(1772615700)
        );
        assert_eq!(
            unix_seconds_from_rfc3339("2026-03-04T09:15:00.120456789Z"),
            Some(1772615700)
        );
        // An offset is what it says: the same instant, spelled locally.
        assert_eq!(
            unix_seconds_from_rfc3339("2026-03-04T10:15:00+01:00"),
            Some(1772615700)
        );
        assert_eq!(
            unix_seconds_from_rfc3339("2026-03-04T04:15:00-05:00"),
            Some(1772615700)
        );
        // A leap day, which the shifted-era arithmetic is there for.
        assert_eq!(
            unix_seconds_from_rfc3339("2024-02-29T00:00:00Z"),
            Some(1709164800)
        );
        for nonsense in [
            "",
            "yesterday",
            "2026-03-04",
            "2026-03-04 09:15",
            "2026-13-04T09:15:00Z",
            "2026-03-04T25:15:00Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(
                unix_seconds_from_rfc3339(nonsense),
                None,
                "time {nonsense:?}"
            );
        }
    }
}
