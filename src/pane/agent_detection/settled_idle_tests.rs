//! Publish sequences for an idle verdict that needs settling. Codex's turn
//! separator can also be the last committed line of an answer that is still
//! streaming, with no status line on screen, so a single frame must never
//! publish idle; a finished turn's screen stays put and publishes once the
//! verdict has held for the settle window.

use super::*;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "../../../tests/fixtures/agent-screen/codex/",
            $name
        ))
    };
}

const STREAMING: &str = fixture!("streaming-quoted-separator.txt");
const FINISHED: &str = fixture!("idle-turn-complete.txt");

/// The same answer one commit tick later: the next streamed line lands below
/// the quoted separator.
fn streamed_further() -> String {
    STREAMING.replacen(
        "  Worked for 2m 3s · 9:41 AM\n",
        "  Worked for 2m 3s · 9:41 AM\n\n  It only counts once nothing else follows it.\n",
        1,
    )
}

/// Feeds screens through the real detection and publish decision, one read
/// per pending-idle recheck, and returns each published state with its time.
fn run(
    start: AgentState,
    frames: impl Fn(std::time::Duration) -> String,
    total: std::time::Duration,
) -> Vec<(std::time::Duration, AgentState)> {
    crate::detect::manifest::codex_tests::with_codex_manifest_dirs("settled-idle", || {
        let t0 = std::time::Instant::now();
        let mut state = start;
        let mut pending = PendingIdleConfirmation::default();
        let mut published = Vec::new();
        let mut at = std::time::Duration::ZERO;
        while at <= total {
            let detection = detection_update_for_publish(Some(Agent::Codex), &frames(at), false)
                .expect("codex screen is not skipped");
            let input = ScreenDetectionPublishInput {
                current_state: state,
                last_visible_idle: false,
                last_visible_blocker: false,
                last_visible_working: false,
                last_visible_signal_refresh: None,
                screen_detection: detection,
                process_exited: false,
                agent_changed: false,
                now: t0 + at,
            };
            if let DetectionPublishDecision::Publish { state: next, .. } =
                decide_screen_detection_publish(input, &mut pending)
            {
                state = next;
                published.push((at, next));
            }
            at += AGENT_PENDING_IDLE_RECHECK;
        }
        published
    })
}

fn first_idle(published: &[(std::time::Duration, AgentState)]) -> Option<std::time::Duration> {
    published
        .iter()
        .find(|(_, state)| *state == AgentState::Idle)
        .map(|(at, _)| *at)
}

#[test]
fn a_separator_seen_for_one_read_of_a_changing_screen_never_publishes_idle() {
    let changed = streamed_further();
    for start in [AgentState::Unknown, AgentState::Working] {
        let published = run(
            start,
            |at| {
                if at.is_zero() {
                    STREAMING.to_string()
                } else {
                    changed.clone()
                }
            },
            SETTLED_IDLE_WINDOW * 2,
        );
        assert_eq!(
            first_idle(&published),
            None,
            "from {start:?}: {published:?}"
        );
    }
}

#[test]
fn a_separator_screen_held_static_publishes_idle_only_after_the_window() {
    for (screen, start) in [
        (STREAMING, AgentState::Unknown),
        (FINISHED, AgentState::Unknown),
        (FINISHED, AgentState::Working),
    ] {
        let published = run(start, |_| screen.to_string(), SETTLED_IDLE_WINDOW * 2);
        let idle_at = first_idle(&published).expect("idle publishes once the verdict settles");
        assert!(
            idle_at >= SETTLED_IDLE_WINDOW,
            "from {start:?}: {published:?}"
        );
        assert!(
            idle_at <= SETTLED_IDLE_WINDOW + AGENT_PENDING_IDLE_RECHECK,
            "from {start:?}: {published:?}"
        );
    }
}

#[test]
fn a_different_verdict_restarts_the_settle_window() {
    let changed = streamed_further();
    let window = SETTLED_IDLE_WINDOW;
    let published = run(
        AgentState::Unknown,
        |at| {
            // Static, one changed read just before the window ends, static again.
            if at == window - AGENT_PENDING_IDLE_RECHECK {
                changed.clone()
            } else {
                FINISHED.to_string()
            }
        },
        window * 3,
    );
    let idle_at = first_idle(&published).expect("idle publishes after the restart");
    assert!(idle_at >= window * 2, "{published:?}");
}
