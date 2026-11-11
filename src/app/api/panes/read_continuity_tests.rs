//! `pane.read` read continuity (`continuity` / `continue_from`) and the
//! reply's `alternate_screen` fact, end to end through `handle_pane_read` on
//! real terminal state.
//!
//! The invariant every test leans on: when a reply elides, `before + C +
//! after` equals, byte for byte, the text the same request returns without
//! `continue_from` on the same state, where `C` is everything after the first
//! line feed of the reply that issued the token. When the server cannot
//! prove that, the reply is the full window.

use super::*;
use crate::api::schema::{PaneReadContinued, SuccessResponse};
use crate::{config::Config, workspace::Workspace};

const SCROLLBACK_BYTES: usize = 50_000_000;

fn app_with_bytes(cols: u16, rows: u16, bytes: &[u8]) -> (App, String, PaneId) {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![Workspace::test_new("continuity")];
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].tabs[0].root_pane;
    let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
        cols,
        rows,
        SCROLLBACK_BYTES,
        bytes,
    );
    app.state.insert_test_runtime(pane_id, runtime);
    (app, public_pane_id, pane_id)
}

fn numbered_lines(range: std::ops::Range<usize>) -> Vec<u8> {
    range
        .flat_map(|line| format!("line {line:04} of the opening fixture\r\n").into_bytes())
        .collect()
}

fn write(app: &App, pane_id: PaneId, bytes: &[u8]) {
    app.lookup_runtime(0, pane_id)
        .expect("pane runtime")
        .0
        .test_process_pty_bytes(bytes);
}

fn runtime(app: &App, pane_id: PaneId) -> &crate::terminal::TerminalRuntime {
    app.lookup_runtime(0, pane_id).expect("pane runtime").0
}

fn params(pane_id: &str, source: ReadSource, lines: u32, offset: Option<u64>) -> PaneReadParams {
    PaneReadParams {
        pane_id: pane_id.to_owned(),
        source,
        lines: Some(lines),
        format: ReadFormat::Ansi,
        strip_ansi: true,
        intent: Default::default(),
        span: None,
        offset_from_bottom: offset,
        continuity: false,
        continue_from: None,
    }
}

/// The client's opening screenful: no offset, asks for a token.
fn read120(pane_id: &str) -> PaneReadParams {
    PaneReadParams {
        continuity: true,
        ..params(pane_id, ReadSource::RecentUnwrapped, 120, None)
    }
}

/// The client's opening wide read: offset zero, optionally continuing.
fn read1000(pane_id: &str, token: Option<&str>) -> PaneReadParams {
    PaneReadParams {
        continue_from: token.map(str::to_owned),
        ..params(pane_id, ReadSource::RecentUnwrapped, 1000, Some(0))
    }
}

fn read(app: &mut App, params: PaneReadParams) -> PaneReadResult {
    let response = app.handle_pane_read("read".into(), params);
    let success: SuccessResponse = serde_json::from_str(&response)
        .unwrap_or_else(|err| panic!("not a success ({err}): {response}"));
    let ResponseResult::PaneRead { read } = success.result else {
        panic!("expected a pane read reply: {response}");
    };
    read
}

fn token_of(read: &PaneReadResult) -> String {
    read.continuity
        .as_ref()
        .unwrap_or_else(|| panic!("expected a token on {:?}", read.text))
        .token
        .clone()
}

fn covered_of(text: &str) -> &str {
    &text[text.find('\n').expect("a covered part needs a line feed") + 1..]
}

fn reassemble(continued: &PaneReadContinued, covered: &str) -> String {
    format!("{}{covered}{}", continued.before, continued.after)
}

/// Assert the invariant for one continuation against the same request
/// without `continue_from`, read on the same state. Returns whether it hit.
fn assert_invariant(
    app: &mut App,
    request: PaneReadParams,
    token: &str,
    covered: &str,
    label: &str,
) -> bool {
    let full = read(app, request.clone());
    let continued = read(
        app,
        PaneReadParams {
            continue_from: Some(token.to_owned()),
            ..request
        },
    );
    // Every field but the text describes the full window, unchanged.
    assert_eq!(continued.cols, full.cols, "{label}");
    assert_eq!(continued.truncated, full.truncated, "{label}");
    assert_eq!(continued.effective_offset, full.effective_offset, "{label}");
    assert_eq!(continued.has_more, full.has_more, "{label}");
    assert_eq!(continued.alternate_screen, full.alternate_screen, "{label}");
    assert_eq!(continued.continuity, full.continuity, "{label}");
    match &continued.continued {
        Some(parts) => {
            assert_eq!(continued.text, "", "{label}: a hit carries no text");
            assert_eq!(reassemble(parts, covered), full.text, "{label}");
            assert_eq!(
                parts.utf16_len,
                full.text.encode_utf16().count() as u64,
                "{label}"
            );
            true
        }
        None => {
            assert_eq!(
                continued.text, full.text,
                "{label}: a miss is the full window"
            );
            false
        }
    }
}

/// T1: a token only when asked, on ANSI reads of recent sources whose text
/// has a line feed and at least 256 covered bytes. Without the flag the
/// reply has no continuity keys.
#[tokio::test]
async fn t1_a_token_is_issued_only_when_asked_on_ansi_recent_reads() {
    let mut bytes = numbered_lines(0..300);
    bytes.extend_from_slice(b"$ ");
    let (mut app, pane, _) = app_with_bytes(80, 24, &bytes);

    for source in [ReadSource::Recent, ReadSource::RecentUnwrapped] {
        for offset in [None, Some(0)] {
            let asked = read(
                &mut app,
                PaneReadParams {
                    continuity: true,
                    ..params(&pane, source, 120, offset)
                },
            );
            assert!(asked.continuity.is_some(), "{source:?} {offset:?}");
            assert_eq!(asked.continued, None);

            let raw = app.handle_pane_read("plain".into(), params(&pane, source, 120, offset));
            assert!(
                !raw.contains("\"continuity\"") && !raw.contains("\"continued\""),
                "an unasked reply keeps today's keys: {raw}"
            );
        }
    }

    for (source, format) in [
        (ReadSource::Visible, ReadFormat::Ansi),
        (ReadSource::Detection, ReadFormat::Ansi),
        (ReadSource::Recent, ReadFormat::Text),
        (ReadSource::RecentUnwrapped, ReadFormat::Text),
        (ReadSource::Visible, ReadFormat::Text),
    ] {
        let reply = read(
            &mut app,
            PaneReadParams {
                continuity: true,
                format,
                ..params(&pane, source, 120, None)
            },
        );
        assert_eq!(reply.continuity, None, "{source:?} {format:?}");
    }

    // Under 256 covered bytes there is nothing worth a token.
    let (mut short_app, short_pane, _) = app_with_bytes(80, 24, b"one\r\ntwo\r\nthree\r\n$ ");
    let short = read(&mut short_app, read120(&short_pane));
    assert!(covered_of(&short.text).len() < 256);
    assert_eq!(short.continuity, None);
    let raw = short_app.handle_pane_read("short".into(), read120(&short_pane));
    assert!(!raw.contains("\"continuity\""), "{raw}");
}

/// T2: the real opening pair on an unchanged pane hits, elides everything
/// after read120's first line, and reassembles to the full wide read.
#[tokio::test]
async fn t2_the_unchanged_opening_pair_hits() {
    let mut bytes = numbered_lines(0..600);
    bytes.extend_from_slice(b"$ ");
    let (mut app, pane, _) = app_with_bytes(80, 24, &bytes);

    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let covered = covered_of(&screenful.text).to_owned();

    let wide = read(&mut app, read1000(&pane, Some(&token)));
    let parts = wide.continued.clone().expect("an unchanged pane hits");
    assert_eq!(wide.text, "");
    assert_eq!(parts.after, "");
    let full = read(&mut app, read1000(&pane, None));
    assert_eq!(reassemble(&parts, &covered), full.text);
    assert!(assert_invariant(
        &mut app,
        read1000(&pane, None),
        &token,
        &covered,
        "unchanged"
    ));
}

/// T3 (R3 counterexample 1): ten lines appended between the reads still
/// hit, and `after` is exactly those lines.
#[tokio::test]
async fn t3_appended_lines_arrive_as_after() {
    let bytes = numbered_lines(0..600);
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);

    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let covered = covered_of(&screenful.text).to_owned();

    let appended: Vec<String> = (0..10)
        .map(|n| format!("appended {n:02} after the screenful"))
        .collect();
    for line in &appended {
        write(&app, pane_id, format!("{line}\r\n").as_bytes());
    }

    let wide = read(&mut app, read1000(&pane, Some(&token)));
    let parts = wide.continued.clone().expect("appended output still hits");
    let full = read(&mut app, read1000(&pane, None));
    assert_eq!(reassemble(&parts, &covered), full.text);
    assert!(full.text.ends_with(&parts.after));
    // The formatter separates rows with CRLF and trims the trailing blank
    // row, so the appended rows follow the covered text's last row.
    assert_eq!(parts.after, format!("\r\n{}", appended.join("\r\n")));
}

/// T4 (R3 counterexample 2): a rewritten last line misses, and the reply is
/// the full window with the new text.
#[tokio::test]
async fn t4_a_rewritten_last_line_misses() {
    let bytes = numbered_lines(0..600);
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);

    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let covered = covered_of(&screenful.text).to_owned();

    write(&app, pane_id, b"\x1b[1A\x1b[2Kline 0599 was rewritten\r\n");

    let wide = read(&mut app, read1000(&pane, Some(&token)));
    assert_eq!(wide.continued, None);
    assert!(wide.text.contains("line 0599 was rewritten"));
    assert!(!wide.text.contains("line 0599 of the opening fixture"));
    assert!(!assert_invariant(
        &mut app,
        read1000(&pane, None),
        &token,
        &covered,
        "rewrite"
    ));
}

/// T5: output that pushes the covered stretch out of the wide window
/// misses.
#[tokio::test]
async fn t5_a_window_that_slid_past_the_covered_stretch_misses() {
    let bytes = numbered_lines(0..600);
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);

    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let covered = covered_of(&screenful.text).to_owned();

    write(&app, pane_id, &numbered_lines(600..1700));

    let wide = read(&mut app, read1000(&pane, Some(&token)));
    assert_eq!(wide.continued, None);
    assert!(!assert_invariant(
        &mut app,
        read1000(&pane, None),
        &token,
        &covered,
        "slid"
    ));
}

/// T6: a resize between the reads. Shrinking below the line width rewraps
/// physical rows; the invariant holds whichever way each source goes.
#[tokio::test]
async fn t6_a_resize_between_the_reads_keeps_the_invariant() {
    let mut outcomes = Vec::new();
    for source in [ReadSource::Recent, ReadSource::RecentUnwrapped] {
        for (rows, cols) in [(24, 20), (40, 120), (10, 80)] {
            let bytes = numbered_lines(0..600);
            let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);
            let screenful = read(
                &mut app,
                PaneReadParams {
                    continuity: true,
                    ..params(&pane, source, 120, None)
                },
            );
            let token = token_of(&screenful);
            let covered = covered_of(&screenful.text).to_owned();

            runtime(&app, pane_id).resize(rows, cols, 0, 0);

            let hit = assert_invariant(
                &mut app,
                params(&pane, source, 1000, Some(0)),
                &token,
                &covered,
                &format!("{source:?} resize to {rows}x{cols}"),
            );
            outcomes.push((source, cols, hit));
        }
    }
    // Rewrapping to 20 columns changes every physical row of `recent`.
    assert!(
        outcomes.contains(&(ReadSource::Recent, 20, false)),
        "{outcomes:?}"
    );
}

/// T7: clearing the screen between the reads misses (or, if no covered
/// byte changed, proves); the invariant holds for both clear paths.
#[tokio::test]
async fn t7_a_clear_between_the_reads_keeps_the_invariant() {
    for clear in ["api", "ed2", "ed3"] {
        let bytes = numbered_lines(0..600);
        let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);
        let screenful = read(&mut app, read120(&pane));
        let token = token_of(&screenful);
        let covered = covered_of(&screenful.text).to_owned();

        match clear {
            "api" => runtime(&app, pane_id).clear_screen().expect("clear"),
            "ed2" => write(&app, pane_id, b"\x1b[2J\x1b[H$ "),
            _ => write(&app, pane_id, b"\x1b[3J\x1b[2J\x1b[H$ "),
        }

        let hit = assert_invariant(&mut app, read1000(&pane, None), &token, &covered, clear);
        if clear == "ed3" {
            assert!(!hit, "erased scrollback cannot prove the covered stretch");
        }
    }
}

/// T8: malformed tokens answer the full window, never an error.
#[tokio::test]
async fn t8_malformed_tokens_answer_the_full_window() {
    let bytes = numbered_lines(0..600);
    let (mut app, pane, _) = app_with_bytes(80, 24, &bytes);
    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let raw =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &token).unwrap();
    let encode = |bytes: &[u8]| {
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
    };
    let mut wrong_version = raw.clone();
    wrong_version[0] = 9;

    let full = read(&mut app, read1000(&pane, None));
    for bad in [
        "garbage".to_owned(),
        String::new(),
        encode(&wrong_version),
        token[..token.len() - 3].to_owned(),
        encode(&raw[..raw.len() - 1]),
        encode(&[raw.as_slice(), &[0, 0, 0]].concat()),
        "x".repeat(100_000),
    ] {
        let reply = read(&mut app, read1000(&pane, Some(&bad)));
        assert_eq!(reply.continued, None);
        assert_eq!(reply.text, full.text);
    }
}

/// T9: text-format reads ignore both fields, so headless's text rewrites
/// never meet an elided reply.
#[tokio::test]
async fn t9_text_reads_ignore_continuity() {
    let bytes = numbered_lines(0..600);
    let (mut app, pane, _) = app_with_bytes(80, 24, &bytes);
    let token = token_of(&read(&mut app, read120(&pane)));

    let text_request = PaneReadParams {
        format: ReadFormat::Text,
        continuity: true,
        ..read1000(&pane, Some(&token))
    };
    let reply = read(&mut app, text_request.clone());
    assert_eq!(reply.continued, None);
    assert_eq!(reply.continuity, None);
    let plain = read(
        &mut app,
        PaneReadParams {
            continuity: false,
            continue_from: None,
            ..text_request
        },
    );
    assert_eq!(reply.text, plain.text);
    assert!(!reply.text.is_empty());
}

/// T10: a token from another pane misses unless the bytes are equal, and
/// then proves exactly.
#[tokio::test]
async fn t10_a_foreign_token_proves_only_equal_bytes() {
    let bytes = numbered_lines(0..600);
    let (mut app, pane, _) = app_with_bytes(80, 24, &bytes);

    let (mut other, other_pane, _) = app_with_bytes(80, 24, &numbered_lines(1000..1600));
    let foreign = read(&mut other, read120(&other_pane));
    assert!(!assert_invariant(
        &mut app,
        read1000(&pane, None),
        &token_of(&foreign),
        covered_of(&foreign.text),
        "foreign"
    ));

    let (mut twin, twin_pane, _) = app_with_bytes(80, 24, &bytes);
    let twin_read = read(&mut twin, read120(&twin_pane));
    assert!(assert_invariant(
        &mut app,
        read1000(&pane, None),
        &token_of(&twin_read),
        covered_of(&twin_read.text),
        "equal bytes"
    ));
}

/// T11: styled output whose SGR runs span line boundaries, with every
/// logical line wider than the pane so read120's head row can be a
/// soft-wrap fragment. Answers spec B §6 #1: the covered lines (line 2 on)
/// are byte-identical in the wide read, so the unchanged pair hits for
/// both sources.
#[tokio::test]
async fn t11_styled_wrapped_output_still_hits_after_the_first_line() {
    let mut bytes = Vec::new();
    for line in 0..400 {
        // A colour opened on one line and closed several lines later, bold
        // toggled mid-line, and 100+ columns on a 40-column pane.
        if line % 7 == 0 {
            bytes.extend_from_slice(b"\x1b[38;5;208m");
        }
        if line % 7 == 4 {
            bytes.extend_from_slice(b"\x1b[0m");
        }
        bytes.extend_from_slice(
            format!(
                "styled {line:04} \x1b[1mbold\x1b[22m and a long tail that wraps past forty columns {}\r\n",
                "-".repeat(line % 37)
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(b"\x1b[44m$ ");

    for source in [ReadSource::Recent, ReadSource::RecentUnwrapped] {
        let (mut app, pane, _) = app_with_bytes(40, 24, &bytes);
        let screenful = read(
            &mut app,
            PaneReadParams {
                continuity: true,
                ..params(&pane, source, 120, None)
            },
        );
        assert!(screenful.text.contains("\x1b["), "the fixture is styled");
        let hit = assert_invariant(
            &mut app,
            params(&pane, source, 1000, Some(0)),
            &token_of(&screenful),
            covered_of(&screenful.text),
            &format!("{source:?} styled"),
        );
        assert!(hit, "{source:?}: the styled unchanged opening pair hits");
    }
}

/// T12: CJK and emoji content reassembles exactly, and `utf16_len` counts
/// UTF-16 code units, not bytes or chars.
#[tokio::test]
async fn t12_cjk_and_emoji_reassemble_and_count_utf16() {
    let mut bytes = Vec::new();
    for line in 0..300 {
        bytes.extend_from_slice(format!("第{line}行 日本語 한국어 🦀🎉👋🏽 é\r\n").as_bytes());
    }
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);
    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let covered = covered_of(&screenful.text).to_owned();
    write(&app, pane_id, "最後 🚀\r\n".as_bytes());

    let full = read(&mut app, read1000(&pane, None));
    let wide = read(&mut app, read1000(&pane, Some(&token)));
    let parts = wide.continued.expect("unchanged covered bytes hit");
    let utf16: Vec<u16> = parts
        .before
        .encode_utf16()
        .chain(covered.encode_utf16())
        .chain(parts.after.encode_utf16())
        .collect();
    assert_eq!(utf16.len() as u64, parts.utf16_len);
    assert_eq!(String::from_utf16(&utf16).unwrap(), full.text);
    assert!(parts.utf16_len < full.text.len() as u64);
    assert!(
        parts.utf16_len > full.text.chars().count() as u64,
        "emoji take two units"
    );
}

/// A small deterministic generator so T13 needs no new dependency.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// T13: seeded sequences of append / rewrite / resize / clear / screen
/// switches between a base read and a continuation. The invariant holds in
/// every case, hit or miss, and both outcomes occur.
#[tokio::test]
async fn t13_generated_sequences_keep_the_invariant() {
    let mut rng = XorShift(0x5eed_c0de_2026_1112);
    let mut bytes = numbered_lines(0..400);
    bytes.extend_from_slice(b"$ ");
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);
    let (mut hits, mut misses) = (0, 0);

    for case in 0..2000 {
        if hits + misses >= 240 {
            break;
        }
        let sources = [ReadSource::Recent, ReadSource::RecentUnwrapped];
        let base_source = sources[rng.below(2) as usize];
        let base_lines = [120, 120, 40, 300][rng.below(4) as usize];
        let base = read(
            &mut app,
            PaneReadParams {
                continuity: true,
                ..params(
                    &pane,
                    base_source,
                    base_lines,
                    [None, Some(0)][rng.below(2) as usize],
                )
            },
        );
        let Some(token) = base.continuity.as_ref().map(|c| c.token.clone()) else {
            // Too little text for a token (a fresh alternate screen, a
            // clear): leave the alternate screen and print more.
            write(&app, pane_id, b"\x1b[?1049l");
            write(&app, pane_id, &numbered_lines(case * 10..case * 10 + 50));
            continue;
        };
        let covered = covered_of(&base.text).to_owned();

        for _ in 0..rng.below(4) {
            match rng.below(10) {
                0 | 1 => {
                    let count = 1 + rng.below(40) as usize;
                    let start = 10_000 + case * 100;
                    write(&app, pane_id, &numbered_lines(start..start + count));
                }
                2 => write(
                    &app,
                    pane_id,
                    format!("\x1b[1A\x1b[2Krewritten in case {case}\r\n").as_bytes(),
                ),
                // Rewrite a row in the middle of the screen and come back:
                // head and tail stay, only the digest can tell.
                9 => {
                    let up = 2 + rng.below(3);
                    write(
                        &app,
                        pane_id,
                        format!("\x1b7\x1b[{up}A\r\x1b[2Kmid {case}\x1b8").as_bytes(),
                    );
                }
                3 => write(&app, pane_id, format!("partial {case} ").as_bytes()),
                4 => runtime(&app, pane_id).resize(
                    5 + rng.below(40) as u16,
                    20 + rng.below(120) as u16,
                    0,
                    0,
                ),
                5 => runtime(&app, pane_id).clear_screen().expect("clear"),
                6 => write(&app, pane_id, b"\x1b[2J\x1b[H"),
                7 => write(&app, pane_id, b"\x1b[?1049h alternate frame\r\n"),
                _ => write(&app, pane_id, b"\x1b[?1049l"),
            }
        }

        let source = sources[rng.below(2) as usize];
        let lines = [1000, 1000, 120, 500, 37][rng.below(5) as usize];
        let offset = [Some(0), Some(0), None, Some(3)][rng.below(4) as usize];
        let request = PaneReadParams {
            continuity: rng.below(2) == 0,
            ..params(&pane, source, lines, offset)
        };
        if assert_invariant(&mut app, request, &token, &covered, &format!("case {case}")) {
            hits += 1;
        } else {
            misses += 1;
        }
    }
    assert!(hits + misses >= 200, "{hits} hits, {misses} misses");
    assert!(hits >= 20 && misses >= 20, "{hits} hits, {misses} misses");
}

/// T15 and B2: on a 3000-line pane a hit saves the covered bytes up to the
/// envelope; the token costs at most 80 bytes; a miss is byte-identical to
/// the reply without `continue_from`.
#[tokio::test]
async fn t15_a_hit_saves_the_covered_bytes_and_a_miss_costs_nothing() {
    let mut bytes = numbered_lines(0..3000);
    bytes.extend_from_slice(b"$ ");
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);

    let screenful = read(&mut app, read120(&pane));
    let token = token_of(&screenful);
    let covered = covered_of(&screenful.text);
    let covered_json = serde_json::to_string(covered).unwrap().len();

    let full_raw = app.handle_pane_read("r".into(), read1000(&pane, None));
    let hit_raw = app.handle_pane_read("r".into(), read1000(&pane, Some(&token)));
    assert!(hit_raw.contains("\"continued\""));
    assert!(
        hit_raw.len() + covered_json <= full_raw.len() + 128,
        "hit {} + covered {covered_json} vs full {}",
        hit_raw.len(),
        full_raw.len()
    );

    let asked_raw = app.handle_pane_read(
        "r".into(),
        PaneReadParams {
            continuity: true,
            ..read1000(&pane, None)
        },
    );
    assert!(
        asked_raw.len() - full_raw.len() <= 80,
        "the continuity field costs {} bytes",
        asked_raw.len() - full_raw.len()
    );

    let bad_miss = app.handle_pane_read("r".into(), read1000(&pane, Some("garbage")));
    assert_eq!(
        bad_miss, full_raw,
        "an invalid token costs the client nothing"
    );
    let bad_asked = app.handle_pane_read(
        "r".into(),
        PaneReadParams {
            continuity: true,
            ..read1000(&pane, Some("garbage"))
        },
    );
    assert_eq!(
        bad_asked, asked_raw,
        "only the requested continuity field differs"
    );

    // A real miss: the last line changed under the token. (Text appended
    // to the prompt would still prove: the covered bytes are unchanged.)
    write(&app, pane_id, b"\r\x1b[2Kdone\r\n");
    let full_after = app.handle_pane_read("r".into(), read1000(&pane, None));
    let miss_raw = app.handle_pane_read("r".into(), read1000(&pane, Some(&token)));
    assert!(!miss_raw.contains("\"continued\""), "{miss_raw}");
    assert_eq!(miss_raw, full_after, "a miss costs the client nothing");
}

/// T17: a continuation that also asks `continuity` gets a token over the
/// full window, which the next read can continue from (S3's chain).
#[tokio::test]
async fn t17_a_continued_reply_chains_a_token_over_the_full_window() {
    let bytes = numbered_lines(0..1500);
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);
    let screenful = read(&mut app, read120(&pane));

    let chained_request = |token: &str| PaneReadParams {
        continuity: true,
        ..read1000(&pane, Some(token))
    };
    let first = read(&mut app, chained_request(&token_of(&screenful)));
    assert!(first.continued.is_some());
    let full = read(
        &mut app,
        PaneReadParams {
            continuity: true,
            ..read1000(&pane, None)
        },
    );
    assert_eq!(
        first.continuity, full.continuity,
        "the token names the full window"
    );

    // A static poll continues from the previous wide read.
    let second = read(&mut app, chained_request(&token_of(&first)));
    let parts = second.continued.clone().expect("a static poll hits");
    assert_eq!(parts.after, "");
    assert_eq!(reassemble(&parts, covered_of(&full.text)), full.text);

    // One new line pushes only the uncovered first line out of the window:
    // the covered stretch now starts the window and still proves.
    write(&app, pane_id, b"one more line\r\n");
    let third = read(&mut app, chained_request(&token_of(&second)));
    let parts = third.continued.clone().expect("one new line still hits");
    assert_eq!(parts.before, "");
    assert_eq!(parts.after, "\r\none more line");

    // Two more lines push covered bytes out of the window: a miss.
    write(&app, pane_id, b"two more\r\nthree more\r\n");
    let fourth = read(&mut app, chained_request(&token_of(&third)));
    assert_eq!(fourth.continued, None);
    assert!(fourth.text.ends_with("two more\r\nthree more"));
}

/// Amendment 1: every pane.read reply carries the pane's alternate-screen
/// state at read time, for every source, format and offset.
#[tokio::test]
async fn every_pane_read_reply_carries_the_alternate_screen_at_read_time() {
    let bytes = numbered_lines(0..200);
    let (mut app, pane, pane_id) = app_with_bytes(80, 24, &bytes);
    let requests = || {
        let mut all = Vec::new();
        for source in [
            ReadSource::Visible,
            ReadSource::Recent,
            ReadSource::RecentUnwrapped,
            ReadSource::Detection,
        ] {
            for format in [ReadFormat::Ansi, ReadFormat::Text] {
                all.push(PaneReadParams {
                    format,
                    ..params(&pane, source, 120, None)
                });
            }
        }
        for source in [ReadSource::Recent, ReadSource::RecentUnwrapped] {
            for offset in [Some(0), Some(5)] {
                all.push(params(&pane, source, 120, offset));
            }
        }
        all.push(read120(&pane));
        all
    };

    for (bytes, expected) in [
        (&b""[..], false),
        (&b"\x1b[?1049h\x1b[Hfull screen app"[..], true),
        (&b"\x1b[?1049l"[..], false),
        (&b"\x1b[?47h"[..], true),
        (&b"\x1b[?47l"[..], false),
    ] {
        write(&app, pane_id, bytes);
        for request in requests() {
            let label = format!(
                "{:?} {:?} {:?}",
                request.source, request.format, request.offset_from_bottom
            );
            let raw = app.handle_pane_read("alt".into(), request);
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                value["result"]["read"]["alternate_screen"], expected,
                "{label}"
            );
        }
    }
}

/// The fact and the window come from one content state: PTY output, resize
/// and clear are held off while the handler reads them.
#[tokio::test]
async fn the_alternate_screen_and_the_window_are_read_under_one_content_state() {
    let (app, _, pane_id) = app_with_bytes(80, 24, &numbered_lines(0..10));
    let runtime = runtime(&app, pane_id);
    assert!(!runtime.test_content_writers_excluded());
    assert!(runtime.read_content_stable(|| runtime.test_content_writers_excluded()));
    assert!(!runtime.test_content_writers_excluded());
}

/// Older payloads without the new fields still parse, and an old-style
/// request carries none of them on the wire.
#[test]
fn the_new_fields_are_additive_on_the_wire() {
    let parsed: PaneReadParams = serde_json::from_value(serde_json::json!({
        "pane_id": "w1:p1",
        "source": "recent_unwrapped",
        "lines": 120,
        "format": "ansi",
    }))
    .unwrap();
    assert!(!parsed.continuity);
    assert_eq!(parsed.continue_from, None);
    let wire = serde_json::to_value(&parsed).unwrap();
    assert!(wire.get("continuity").is_none() && wire.get("continue_from").is_none());

    let old_reply: PaneReadResult = serde_json::from_value(serde_json::json!({
        "pane_id": "w1:p1",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "source": "recent",
        "format": "ansi",
        "text": "x",
        "revision": 0,
        "truncated": false,
    }))
    .unwrap();
    assert_eq!(old_reply.alternate_screen, None);
    assert_eq!(old_reply.continuity, None);
    assert_eq!(old_reply.continued, None);
}
