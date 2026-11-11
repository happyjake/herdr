//! `pane.read` read continuity: a stateless token over a reply's covered
//! text, and the byte proof that lets a later read leave those bytes out.
//!
//! The covered part `C` of a text `T` is everything after `T`'s first line
//! feed. Skipping the first line keeps the token clear of a window's head,
//! which can be a soft-wrap fragment or start from different formatter state.
//! A later window `W` elides `C` only when, at a line start `i`, the bytes
//! `W[i..i + |C|]` match the token's length, head, tail and SHA-256 prefix.
//! Then `before + C + after == W` byte for byte, short of a 2^-128 digest
//! collision that also has to match length, head and tail. Anything short of
//! that proof answers the full window. Nothing is kept on the server between
//! reads, so a token survives reconnects and live handoffs.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use crate::api::schema::{PaneReadContinued, PaneReadContinuity, ReadFormat, ReadSource};

const TOKEN_VERSION: u8 = 1;
/// A covered part shorter than this is not worth a token: the token and the
/// `continued` envelope would cost about as much as the bytes they save.
const MIN_COVERED_BYTES: usize = 256;
const EDGE_BYTES: usize = 8;
const DIGEST_BYTES: usize = 16;
/// version ‖ len(C) u32 BE ‖ C[..8] ‖ C[len-8..] ‖ SHA-256(C)[..16]
const TOKEN_BYTES: usize = 1 + 4 + EDGE_BYTES + EDGE_BYTES + DIGEST_BYTES;
/// Unpadded base64url length of a v1 token.
const TOKEN_CHARS: usize = (TOKEN_BYTES * 4).div_ceil(3);
/// Upper bound on digests one continuation request may compute, so a window
/// of identical lines cannot turn one read into unbounded hashing.
const MAX_DIGEST_EVALUATIONS: usize = 16;
/// The aggregate info line is written at most this often.
const TALLY_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Continuity is offered only for ANSI reads of bottom-anchored sources.
/// Text-format reads are rewritten after the handler by the headless
/// alternate-screen harvest, so a byte proof made here would not describe the
/// text the client finally receives.
pub(super) fn honored_for(format: ReadFormat, source: ReadSource) -> bool {
    format == ReadFormat::Ansi && matches!(source, ReadSource::Recent | ReadSource::RecentUnwrapped)
}

/// What one `pane.read` reply carries after the continuity rules ran.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ContinuityReply {
    pub(super) text: String,
    pub(super) continuity: Option<PaneReadContinuity>,
    pub(super) continued: Option<PaneReadContinued>,
}

/// Apply the request's `continuity` and `continue_from` to the full window
/// text a read produced. Callers check [`honored_for`] first.
pub(super) fn apply(
    text: String,
    wants_token: bool,
    continue_from: Option<&str>,
) -> ContinuityReply {
    // The token always names the full window, also on a continued reply,
    // so a client can chain it into the next read.
    let continuity = if wants_token {
        issue(&text).map(|token| PaneReadContinuity { token })
    } else {
        None
    };
    let Some(token) = continue_from else {
        return ContinuityReply {
            text,
            continuity,
            continued: None,
        };
    };
    let (continuation, _) = continue_counting(&text, token);
    let (outcome, elided_bytes) = match &continuation {
        Continuation::Proved { covered_len, .. } => (Outcome::Hit, *covered_len),
        Continuation::Unproved => (Outcome::Miss, 0),
        Continuation::Invalid => (Outcome::Invalid, 0),
    };
    record(outcome, elided_bytes);
    match continuation {
        Continuation::Proved { continued, .. } => ContinuityReply {
            text: String::new(),
            continuity,
            continued: Some(continued),
        },
        Continuation::Unproved | Continuation::Invalid => ContinuityReply {
            text,
            continuity,
            continued: None,
        },
    }
}

/// A v1 token over `text`'s covered part, or `None` when the text has no
/// line feed or its covered part is shorter than [`MIN_COVERED_BYTES`].
pub(super) fn issue(text: &str) -> Option<String> {
    let covered = covered_of(text)?.as_bytes();
    if covered.len() < MIN_COVERED_BYTES {
        return None;
    }
    let len = u32::try_from(covered.len()).ok()?;
    let mut token = Vec::with_capacity(TOKEN_BYTES);
    token.push(TOKEN_VERSION);
    token.extend_from_slice(&len.to_be_bytes());
    token.extend_from_slice(&covered[..EDGE_BYTES]);
    token.extend_from_slice(&covered[covered.len() - EDGE_BYTES..]);
    token.extend_from_slice(&digest(covered));
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token))
}

/// Everything after the first line feed. A line feed is never part of a
/// UTF-8 multibyte sequence, so the split is always on a char boundary.
fn covered_of(text: &str) -> Option<&str> {
    let line_feed = text.as_bytes().iter().position(|&byte| byte == b'\n')?;
    text.get(line_feed + 1..)
}

fn digest(bytes: &[u8]) -> [u8; DIGEST_BYTES] {
    let full = Sha256::digest(bytes);
    let mut prefix = [0; DIGEST_BYTES];
    prefix.copy_from_slice(&full[..DIGEST_BYTES]);
    prefix
}

struct Token {
    covered_len: usize,
    head: [u8; EDGE_BYTES],
    tail: [u8; EDGE_BYTES],
    digest: [u8; DIGEST_BYTES],
}

fn decode(token: &str) -> Option<Token> {
    // Checked before decoding so an oversized token costs nothing.
    if token.len() != TOKEN_CHARS {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()?;
    if bytes.len() != TOKEN_BYTES || bytes[0] != TOKEN_VERSION {
        return None;
    }
    let covered_len = usize::try_from(u32::from_be_bytes(bytes[1..5].try_into().ok()?)).ok()?;
    if covered_len < MIN_COVERED_BYTES {
        return None;
    }
    let head_at = 5;
    let tail_at = head_at + EDGE_BYTES;
    let digest_at = tail_at + EDGE_BYTES;
    Some(Token {
        covered_len,
        head: bytes[head_at..tail_at].try_into().ok()?,
        tail: bytes[tail_at..digest_at].try_into().ok()?,
        digest: bytes[digest_at..].try_into().ok()?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Hit,
    Miss,
    Invalid,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Continuation {
    Proved {
        continued: PaneReadContinued,
        covered_len: usize,
    },
    Unproved,
    Invalid,
}

/// Look for the token's covered bytes inside `window`, from the bottom up,
/// and return the outcome plus how many digests it took.
fn continue_counting(window: &str, token: &str) -> (Continuation, usize) {
    let Some(token) = decode(token) else {
        return (Continuation::Invalid, 0);
    };
    let bytes = window.as_bytes();
    let Some(last_start) = bytes.len().checked_sub(token.covered_len) else {
        return (Continuation::Unproved, 0);
    };
    let mut evaluations = 0;
    // `upper` bounds the next candidate start, inclusive. Candidates are
    // line starts: 0 and every index right after a line feed. Bottom-up
    // reaches the unchanged-pane position, |W| - |C|, first.
    let mut upper = last_start;
    loop {
        let start = bytes[..upper]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |line_feed| line_feed + 1);
        let end = start + token.covered_len;
        if bytes[start..start + EDGE_BYTES] == token.head
            && bytes[end - EDGE_BYTES..end] == token.tail
        {
            // `get` refuses a range that splits a char, so the slices
            // below cannot panic and `before`/`after` stay valid UTF-8.
            if let (Some(candidate), Some(before), Some(after)) = (
                window.get(start..end),
                window.get(..start),
                window.get(end..),
            ) {
                evaluations += 1;
                if digest(candidate.as_bytes()) == token.digest {
                    let continued = PaneReadContinued {
                        before: before.to_owned(),
                        after: after.to_owned(),
                        utf16_len: window.encode_utf16().count() as u64,
                    };
                    return (
                        Continuation::Proved {
                            continued,
                            covered_len: token.covered_len,
                        },
                        evaluations,
                    );
                }
                if evaluations >= MAX_DIGEST_EVALUATIONS {
                    return (Continuation::Unproved, evaluations);
                }
            }
        }
        if start == 0 {
            return (Continuation::Unproved, evaluations);
        }
        upper = start - 1;
    }
}

/// Process-wide continuation counters, reported as one info line at most
/// once per [`TALLY_REPORT_INTERVAL`]. Totals are cumulative since the
/// server started, so two lines bracket a block of reads by subtraction.
#[derive(Debug)]
struct ContinuityTally {
    hits: u64,
    misses: u64,
    invalid: u64,
    elided_bytes: u64,
    last_report: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TallyReport {
    hits: u64,
    misses: u64,
    invalid: u64,
    elided_bytes: u64,
}

impl ContinuityTally {
    const fn new() -> Self {
        Self {
            hits: 0,
            misses: 0,
            invalid: 0,
            elided_bytes: 0,
            last_report: None,
        }
    }

    /// Count one continuation and return the totals when a report is due:
    /// on the first continuation, then on the first one at least an interval
    /// after the last report.
    fn record(
        &mut self,
        outcome: Outcome,
        elided_bytes: usize,
        now: Instant,
    ) -> Option<TallyReport> {
        match outcome {
            Outcome::Hit => self.hits += 1,
            Outcome::Miss => self.misses += 1,
            Outcome::Invalid => self.invalid += 1,
        }
        self.elided_bytes = self.elided_bytes.saturating_add(elided_bytes as u64);
        let due = self
            .last_report
            .is_none_or(|last| now.saturating_duration_since(last) >= TALLY_REPORT_INTERVAL);
        if !due {
            return None;
        }
        self.last_report = Some(now);
        Some(TallyReport {
            hits: self.hits,
            misses: self.misses,
            invalid: self.invalid,
            elided_bytes: self.elided_bytes,
        })
    }
}

static TALLY: Mutex<ContinuityTally> = Mutex::new(ContinuityTally::new());

fn record(outcome: Outcome, elided_bytes: usize) {
    crate::logging::api_read_continued(outcome.as_str(), elided_bytes);
    let report = TALLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(outcome, elided_bytes, Instant::now());
    if let Some(report) = report {
        crate::logging::api_read_continuity_totals(
            report.hits,
            report.misses,
            report.invalid,
            report.elided_bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(lines: std::ops::Range<usize>) -> String {
        lines
            .map(|line| format!("line {line:04} of the fixture\n"))
            .collect()
    }

    fn reassemble(continued: &PaneReadContinued, covered: &str) -> String {
        format!("{}{covered}{}", continued.before, continued.after)
    }

    fn proved(window: &str, token: &str) -> PaneReadContinued {
        match continue_counting(window, token).0 {
            Continuation::Proved { continued, .. } => continued,
            other => panic!("expected a proved continuation, got {other:?}"),
        }
    }

    #[test]
    fn a_token_needs_a_line_feed_and_256_covered_bytes() {
        assert_eq!(issue(&"x".repeat(4096)), None, "no line feed");
        let short = format!("head\n{}", "y".repeat(MIN_COVERED_BYTES - 1));
        assert_eq!(issue(&short), None);
        let enough = format!("head\n{}", "y".repeat(MIN_COVERED_BYTES));
        let token = issue(&enough).expect("covered part is long enough");
        assert_eq!(token.len(), TOKEN_CHARS);
        assert_eq!(TOKEN_CHARS, 50);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "base64url without padding: {token}"
        );
    }

    #[test]
    fn the_covered_part_skips_the_first_line_only() {
        let text = numbered(0..40);
        let token = issue(&text).unwrap();
        let covered = covered_of(&text).unwrap();
        assert!(covered.starts_with("line 0001"));
        let decoded = decode(&token).unwrap();
        assert_eq!(decoded.covered_len, covered.len());
        assert_eq!(&decoded.head, &covered.as_bytes()[..8]);
        assert_eq!(&decoded.tail, &covered.as_bytes()[covered.len() - 8..]);
    }

    #[test]
    fn an_unchanged_window_elides_everything_but_the_first_line() {
        let text = numbered(0..40);
        let token = issue(&text).unwrap();
        let continued = proved(&text, &token);
        assert_eq!(continued.before, "line 0000 of the fixture\n");
        assert_eq!(continued.after, "");
        assert_eq!(reassemble(&continued, covered_of(&text).unwrap()), text);
    }

    #[test]
    fn a_wider_window_with_appended_lines_proves_before_and_after() {
        let base = numbered(880..1000);
        let token = issue(&base).unwrap();
        let wide = numbered(0..1010);
        let continued = proved(&wide, &token);
        assert_eq!(continued.after, numbered(1000..1010));
        assert_eq!(continued.before, numbered(0..881));
        assert_eq!(reassemble(&continued, covered_of(&base).unwrap()), wide);
        assert_eq!(continued.utf16_len, wide.encode_utf16().count() as u64);
    }

    #[test]
    fn a_rewritten_last_line_misses() {
        let base = numbered(0..40);
        let token = issue(&base).unwrap();
        let rewritten = base.replace("line 0039 of the fixture", "line 0039 of the rewrite");
        assert_eq!(
            continue_counting(&rewritten, &token).0,
            Continuation::Unproved
        );
    }

    #[test]
    fn a_window_that_slid_past_the_covered_stretch_misses() {
        let base = numbered(0..40);
        let token = issue(&base).unwrap();
        assert_eq!(
            continue_counting(&numbered(2..60), &token).0,
            Continuation::Unproved
        );
        assert_eq!(
            continue_counting(&numbered(0..5), &token).0,
            Continuation::Unproved,
            "a window shorter than the covered part"
        );
    }

    #[test]
    fn malformed_tokens_are_invalid_and_never_panic() {
        let window = numbered(0..40);
        let valid = issue(&window).unwrap();
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&valid)
            .unwrap();
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut wrong_version = raw.clone();
        wrong_version[0] = 2;
        let mut short_len = raw.clone();
        short_len[1..5].copy_from_slice(&(MIN_COVERED_BYTES as u32 - 1).to_be_bytes());
        let mut huge_len = raw.clone();
        huge_len[1..5].copy_from_slice(&u32::MAX.to_be_bytes());
        let cases = [
            String::new(),
            "garbage".to_owned(),
            "!".repeat(TOKEN_CHARS),
            encode(&wrong_version),
            valid[..TOKEN_CHARS - 1].to_owned(),
            format!("{valid}A"),
            encode(&raw[..TOKEN_BYTES - 1]),
            encode(&[raw.clone(), vec![0]].concat()),
            encode(&short_len),
            format!("{valid}="),
            format!("+{}", &valid[1..]),
            "é".repeat(TOKEN_CHARS / 2),
        ];
        for token in &cases {
            let outcome = continue_counting(&window, token).0;
            assert_eq!(outcome, Continuation::Invalid, "{token:?}");
        }
        assert_eq!(
            continue_counting(&window, &encode(&huge_len)).0,
            Continuation::Unproved,
            "a well-formed token longer than the window proves nothing"
        );
    }

    #[test]
    fn a_foreign_token_proves_only_equal_bytes() {
        let window = numbered(0..40);
        let other_pane = numbered(500..540);
        let foreign = issue(&other_pane).unwrap();
        assert_eq!(
            continue_counting(&window, &foreign).0,
            Continuation::Unproved
        );
        let same_bytes_elsewhere =
            issue(&format!("another head\n{}", covered_of(&window).unwrap())).unwrap();
        let continued = proved(&window, &same_bytes_elsewhere);
        assert_eq!(reassemble(&continued, covered_of(&window).unwrap()), window);
    }

    #[test]
    fn cjk_and_emoji_reassemble_in_utf16() {
        let lines: String = (0..40)
            .map(|line| format!("第{line}行 日本語のテキスト 🦀🎉 é\n"))
            .collect();
        let token = issue(&lines).unwrap();
        let wide = format!("前の行 🚀\n{lines}最後の行 👋🏽\n");
        let continued = proved(&wide, &token);
        let covered = covered_of(&lines).unwrap();
        let rebuilt = reassemble(&continued, covered);
        assert_eq!(rebuilt, wide);
        let utf16: Vec<u16> = continued
            .before
            .encode_utf16()
            .chain(covered.encode_utf16())
            .chain(continued.after.encode_utf16())
            .collect();
        assert_eq!(utf16.len() as u64, continued.utf16_len);
        assert_eq!(String::from_utf16(&utf16).unwrap(), wide);
        assert_ne!(continued.utf16_len, wide.len() as u64);
    }

    #[test]
    fn a_window_of_identical_lines_hashes_at_most_sixteen_times() {
        let line = "the same line, again and again, with no change at all\n";
        let base = line.repeat(120);
        let token = issue(&base).unwrap();
        // A changed final line defeats every candidate while each of them
        // still passes the head and tail prefilter.
        let mut decoded = decode(&token).unwrap();
        decoded.digest = [0; DIGEST_BYTES];
        let mut raw = vec![TOKEN_VERSION];
        raw.extend_from_slice(&(decoded.covered_len as u32).to_be_bytes());
        raw.extend_from_slice(&decoded.head);
        raw.extend_from_slice(&decoded.tail);
        raw.extend_from_slice(&decoded.digest);
        let unprovable = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let window = line.repeat(3000);
        let (outcome, evaluations) = continue_counting(&window, &unprovable);
        assert_eq!(outcome, Continuation::Unproved);
        assert_eq!(evaluations, MAX_DIGEST_EVALUATIONS);

        let (outcome, evaluations) = continue_counting(&window, &token);
        let Continuation::Proved { continued, .. } = outcome else {
            panic!("identical lines still prove");
        };
        assert_eq!(
            evaluations, 1,
            "bottom-up hits the unchanged position first"
        );
        assert_eq!(continued.after, "");
        assert_eq!(reassemble(&continued, covered_of(&base).unwrap()), window);
    }

    #[test]
    fn only_ansi_reads_of_recent_sources_are_honored() {
        assert!(honored_for(ReadFormat::Ansi, ReadSource::Recent));
        assert!(honored_for(ReadFormat::Ansi, ReadSource::RecentUnwrapped));
        assert!(!honored_for(ReadFormat::Ansi, ReadSource::Visible));
        assert!(!honored_for(ReadFormat::Ansi, ReadSource::Detection));
        assert!(!honored_for(ReadFormat::Text, ReadSource::Recent));
        assert!(!honored_for(ReadFormat::Text, ReadSource::RecentUnwrapped));
    }

    #[test]
    fn the_tally_reports_first_then_at_most_once_a_minute_with_cumulative_totals() {
        let start = Instant::now();
        let mut tally = ContinuityTally::new();
        assert_eq!(
            tally.record(Outcome::Hit, 100, start),
            Some(TallyReport {
                hits: 1,
                misses: 0,
                invalid: 0,
                elided_bytes: 100
            })
        );
        assert_eq!(
            tally.record(Outcome::Miss, 0, start + Duration::from_secs(10)),
            None
        );
        assert_eq!(
            tally.record(Outcome::Invalid, 0, start + Duration::from_secs(59)),
            None
        );
        assert_eq!(
            tally.record(Outcome::Hit, 50, start + Duration::from_secs(60)),
            Some(TallyReport {
                hits: 2,
                misses: 1,
                invalid: 1,
                elided_bytes: 150
            })
        );
        assert_eq!(
            tally.record(Outcome::Hit, 1, start + Duration::from_secs(61)),
            None
        );
    }

    #[test]
    fn apply_issues_over_the_full_window_also_when_it_continues() {
        let base = numbered(0..40);
        let token = issue(&base).unwrap();
        let wide = numbered(0..50);
        let reply = apply(wide.clone(), true, Some(&token));
        assert_eq!(reply.text, "");
        assert_eq!(
            reply.continuity,
            Some(PaneReadContinuity {
                token: issue(&wide).unwrap()
            })
        );
        let continued = reply.continued.unwrap();
        assert_eq!(reassemble(&continued, covered_of(&base).unwrap()), wide);

        let untouched = apply(wide.clone(), false, None);
        assert_eq!(
            untouched,
            ContinuityReply {
                text: wide.clone(),
                continuity: None,
                continued: None
            }
        );
        let miss = apply(wide.clone(), false, Some("garbage"));
        assert_eq!(miss.text, wide);
        assert_eq!(miss.continued, None);
    }
}
