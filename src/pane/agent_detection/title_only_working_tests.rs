//! Publish sequences for title-only working over an unchanged screen.
//! Synthetic manifests exercise fallback idle, explicit title idle and
//! explicit screen idle without pinning any bundled agent's UI conventions.

use super::*;

const FINISHED: &str = "done-marker\nprompt-marker";
const WORKING: &str = "active-marker\nprompt-marker";
const COMPOSER: &str = "prompt-marker";
const TITLE: &str = "ready";
const IDENTIFIED_TICK: std::time::Duration = std::time::Duration::from_millis(300);
const BOOT_SPIN: std::time::Duration = std::time::Duration::from_millis(2_200);

fn secs(value: f64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rules {
    Fallback,
    TitleIdle,
    ScreenIdle,
}

const ALL_RULES: [Rules; 3] = [Rules::Fallback, Rules::TitleIdle, Rules::ScreenIdle];

fn with_rules<T>(rules: Rules, f: impl FnOnce() -> T) -> T {
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    let old_config = std::env::var_os("XDG_CONFIG_HOME");
    let old_state = std::env::var_os("XDG_STATE_HOME");
    let base = std::env::temp_dir().join(format!("herdr-title-only-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::env::set_var("XDG_CONFIG_HOME", base.join("config"));
    std::env::set_var("XDG_STATE_HOME", base.join("state"));
    // The agent name is only a registry key; every rule is synthetic.
    let mut manifest = String::from(
        r#"
id = "claude"
min_engine_version = 3

[[rules]]
id = "title_activity"
state = "working"
priority = 300
region = "osc_title"
visible_working = true
regex = ['^busy-']

[[rules]]
id = "screen_activity"
state = "working"
priority = 200
region = "whole_recent"
visible_working = true
contains = ["active-marker"]
"#,
    );
    match rules {
        Rules::Fallback => {}
        Rules::TitleIdle => manifest.push_str(
            r#"
[[rules]]
id = "title_ready"
state = "idle"
priority = 100
region = "osc_title"
visible_idle = true
contains = ["ready"]
"#,
        ),
        Rules::ScreenIdle => manifest.push_str(
            r#"
[[rules]]
id = "screen_ready"
state = "idle"
priority = 100
region = "whole_recent"
visible_idle = true
contains = ["done-marker"]
"#,
        ),
    }
    let path = crate::config::config_dir().join("agent-detection/claude.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, manifest).unwrap();
    crate::detect::manifest::reload_manifests();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match old_config {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match old_state {
        Some(value) => std::env::set_var("XDG_STATE_HOME", value),
        None => std::env::remove_var("XDG_STATE_HOME"),
    }
    crate::detect::manifest::reload_manifests();
    let _ = std::fs::remove_dir_all(base);
    match result {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error),
    }
}

fn spinning(at: std::time::Duration) -> String {
    format!("busy-{}", (at.as_millis() / 100) % 10)
}

fn detect(screen: &str, title: &str) -> Option<AgentDetection> {
    detection_update_for_publish_with_osc(Some(Agent::Claude), screen, title, "", false)
}

type Frame = (String, String);
type Published = Vec<(std::time::Duration, AgentState)>;

/// Starts from a pane that has published what `start` reads, then feeds
/// frames through the real read, detection and publish decisions, and
/// returns that starting state with every published change of state.
fn run_from(
    rules: Rules,
    start: (&str, &str),
    frames: impl Fn(std::time::Duration) -> Frame,
    total: std::time::Duration,
) -> (AgentState, Published) {
    run_with(rules, start, true, frames, total)
}

/// [`run_from`], with `start` already read by this pane task or, when
/// `start_read` is false, by an earlier one: a fresh task has read nothing.
fn run_with(
    rules: Rules,
    start: (&str, &str),
    start_read: bool,
    frames: impl Fn(std::time::Duration) -> Frame,
    total: std::time::Duration,
) -> (AgentState, Published) {
    with_rules(rules, || {
        let steady = detect(start.0, start.1).expect("the starting screen is not skipped");
        let start_state = steady.state;
        let t0 = std::time::Instant::now();
        let mut state = start_state;
        let mut last_visible = (
            steady.visible_idle && start_state == AgentState::Idle,
            steady.visible_blocker && start_state == AgentState::Blocked,
            steady.visible_working && start_state == AgentState::Working,
        );
        let mut pending = PendingIdleConfirmation::default();
        let mut published = Vec::new();
        let mut last_frame: Frame = (start.0.to_string(), start.1.to_string());
        let mut content_seq = 0_u64;
        let mut screen_text = ScreenTextBaseline::default();
        let mut last_scan_seq = None;
        if start_read {
            screen_text.observe(start.0.to_owned());
            last_scan_seq = Some(content_seq);
        }
        let mut at = std::time::Duration::ZERO;
        while at <= total {
            let frame = frames(at);
            if frame != last_frame {
                content_seq += 1;
                last_frame = frame.clone();
            }
            let read = decide_detection_screen_read(DetectionScreenReadInput {
                state,
                agent: Some(Agent::Claude),
                pending_idle_active: pending.active(),
                agent_changed: false,
                process_exited: false,
                current_detection_content_seq: Some(content_seq),
                last_screen_scan_detection_content_seq: last_scan_seq,
            });
            if read == DetectionScreenReadDecision::Read {
                last_scan_seq = Some(content_seq);
                let screen_changed = screen_text.observe(frame.0.clone()).screen_changed;
                match detect(&frame.0, &frame.1) {
                    Some(detection) => {
                        let input = ScreenDetectionPublishInput {
                            current_state: state,
                            last_visible_idle: last_visible.0,
                            last_visible_blocker: last_visible.1,
                            last_visible_working: last_visible.2,
                            last_visible_signal_refresh: None,
                            screen_detection: detection,
                            screen_changed,
                            process_exited: false,
                            agent_changed: false,
                            now: t0 + at,
                        };
                        if let DetectionPublishDecision::Publish {
                            state: next,
                            visible_idle,
                            visible_blocker,
                            visible_working,
                            ..
                        } = decide_screen_detection_publish(input, &mut pending)
                        {
                            last_visible = (visible_idle, visible_blocker, visible_working);
                            if next != state {
                                published.push((at, next));
                            }
                            state = next;
                        }
                    }
                    None => pending.clear(),
                }
            }
            at += if pending.active() {
                AGENT_PENDING_IDLE_RECHECK
            } else {
                IDENTIFIED_TICK
            };
        }
        (start_state, published)
    })
}

/// [`run_from`] a pane showing the finished turn under a still title.
fn run(
    rules: Rules,
    frames: impl Fn(std::time::Duration) -> Frame,
    total: std::time::Duration,
) -> (AgentState, Published) {
    run_from(rules, (FINISHED, TITLE), frames, total)
}

fn first(published: &Published, wanted: AgentState) -> Option<std::time::Duration> {
    published
        .iter()
        .find(|(_, state)| *state == wanted)
        .map(|(at, _)| *at)
}

/// The finished screen with its title spinning for `spin`, then still.
fn spin_over_finished(spin: std::time::Duration) -> impl Fn(std::time::Duration) -> Frame {
    move |at| {
        let title = if at < spin {
            spinning(at)
        } else {
            TITLE.to_string()
        };
        (FINISHED.to_string(), title)
    }
}

/// The finished transcript, a new prompt and `answer` above the composer.
fn answering(answer: &str, closing: &str) -> String {
    let transcript = &FINISHED[..FINISHED.find(COMPOSER).expect("the fixture has a composer")];
    let footer = &FINISHED[FINISHED.find(COMPOSER).unwrap()..];
    format!("{transcript}› Summarize the result.\n\n\n• {answer}\n{closing}\n\n{footer}")
}

#[test]
fn a_finished_turn_reads_idle_under_explicit_idle_rules() {
    for rules in [Rules::TitleIdle, Rules::ScreenIdle] {
        let start = with_rules(rules, || detect(FINISHED, TITLE).map(|d| d.state));
        assert_eq!(start, Some(AgentState::Idle), "{rules:?}");
    }
}

#[test]
fn a_tool_server_boot_spin_over_a_finished_turn_publishes_nothing() {
    for rules in ALL_RULES {
        let (start, published) = run(rules, spin_over_finished(BOOT_SPIN), secs(20.0));
        assert_eq!(published, Vec::new(), "{rules:?} from {start:?}");
    }
}

#[test]
fn a_fresh_task_reading_a_spin_first_does_not_take_it_for_a_changed_screen() {
    // A pane task starts with nothing read, as after a restart. Its first read
    // finds the finished turn under a spinning title: there is no earlier
    // screen to differ from, so the spin is held like any other.
    for rules in ALL_RULES {
        let (start, published) = run_with(
            rules,
            (FINISHED, TITLE),
            false,
            spin_over_finished(BOOT_SPIN),
            secs(20.0),
        );
        assert_eq!(published, Vec::new(), "{rules:?} from {start:?}");
    }
}

#[test]
fn a_ten_second_spin_over_a_finished_turn_publishes_nothing() {
    // A tool server that fails to boot spins the title until the agent's
    // startup timeout, ten seconds by default.
    for rules in ALL_RULES {
        let (start, published) = run(rules, spin_over_finished(secs(10.0)), secs(25.0));
        assert_eq!(published, Vec::new(), "{rules:?} from {start:?}");
    }
}

#[test]
fn a_title_that_keeps_spinning_starts_a_turn_after_the_grace() {
    for rules in ALL_RULES {
        let (_, published) = run(
            rules,
            |at| (FINISHED.to_string(), spinning(at)),
            TITLE_ONLY_WORKING_GRACE * 2,
        );
        let working_at = first(&published, AgentState::Working)
            .unwrap_or_else(|| panic!("{rules:?}: working never published: {published:?}"));
        assert!(
            working_at >= TITLE_ONLY_WORKING_GRACE,
            "{rules:?}: {published:?}"
        );
        assert!(
            working_at <= TITLE_ONLY_WORKING_GRACE + IDENTIFIED_TICK,
            "{rules:?}: {published:?}"
        );
    }
}

#[test]
fn a_spinner_glyph_left_in_a_still_title_still_matures() {
    // Nothing is drawn after the title is set, so only the held verdict keeps
    // the screen being read.
    let still = "busy-0".to_string();
    for rules in ALL_RULES {
        let (_, published) = run(
            rules,
            |_| (FINISHED.to_string(), still.clone()),
            TITLE_ONLY_WORKING_GRACE * 2,
        );
        let working_at = first(&published, AgentState::Working)
            .unwrap_or_else(|| panic!("{rules:?}: working never published: {published:?}"));
        assert!(
            working_at <= TITLE_ONLY_WORKING_GRACE + IDENTIFIED_TICK,
            "{rules:?}: {published:?}"
        );
    }
}

#[test]
fn a_turn_drawn_on_screen_publishes_working_at_once_and_then_its_finish() {
    let turn_starts = secs(0.3);
    for rules in ALL_RULES {
        let (start, published) = run(
            rules,
            |at| {
                if at >= turn_starts && at < BOOT_SPIN {
                    (WORKING.to_string(), spinning(at))
                } else {
                    (FINISHED.to_string(), TITLE.to_string())
                }
            },
            BOOT_SPIN * 3,
        );
        let working_at = first(&published, AgentState::Working)
            .unwrap_or_else(|| panic!("{rules:?}: working never published: {published:?}"));
        assert!(
            working_at <= turn_starts + IDENTIFIED_TICK,
            "{rules:?}: {published:?}"
        );
        assert_eq!(
            published.last().map(|(_, state)| *state),
            Some(start),
            "{rules:?}: the turn ends where it started: {published:?}"
        );
    }
}

#[test]
fn a_status_row_drawn_during_a_held_spin_publishes_at_once() {
    let row_drawn = secs(1.0);
    for rules in ALL_RULES {
        let (start, published) = run(
            rules,
            |at| {
                if at < row_drawn {
                    (FINISHED.to_string(), spinning(at))
                } else if at < BOOT_SPIN {
                    (WORKING.to_string(), spinning(at))
                } else {
                    (FINISHED.to_string(), TITLE.to_string())
                }
            },
            BOOT_SPIN * 3,
        );
        let working_at = first(&published, AgentState::Working)
            .unwrap_or_else(|| panic!("{rules:?}: working never published: {published:?}"));
        assert!(working_at >= row_drawn, "{rules:?}: {published:?}");
        assert!(
            working_at <= row_drawn + AGENT_PENDING_IDLE_RECHECK,
            "{rules:?}: {published:?}"
        );
        assert_eq!(
            published.last().map(|(_, state)| *state),
            Some(start),
            "{rules:?}: {published:?}"
        );
    }
}

#[test]
fn an_answer_streaming_with_its_status_row_hidden_publishes_working_and_its_finish() {
    // A status row may disappear while answer lines stream in, so a turn whose
    // status row came and went between two reads shows only the spinning
    // title and a growing answer.
    let words = [
        "The", "suite", "passes", "on", "both", "targets,", "and", "the", "report", "is", "filed.",
    ];
    let stream_starts = secs(0.3);
    let stream_ends = stream_starts + AGENT_PENDING_IDLE_RECHECK * words.len() as u32;
    let finished = answering(&words.join(" "), "done-marker");
    for rules in ALL_RULES {
        let finish_state = with_rules(rules, || detect(&finished, TITLE).map(|d| d.state));
        let (_, published) = run(
            rules,
            |at| {
                if at < stream_starts {
                    (FINISHED.to_string(), TITLE.to_string())
                } else if at < stream_ends {
                    let shown = ((at - stream_starts).as_millis() / 100) as usize + 1;
                    (answering(&words[..shown].join(" "), ""), spinning(at))
                } else {
                    (finished.clone(), TITLE.to_string())
                }
            },
            stream_ends * 3,
        );
        let working_at = first(&published, AgentState::Working)
            .unwrap_or_else(|| panic!("{rules:?}: working never published: {published:?}"));
        assert!(
            working_at <= stream_starts + IDENTIFIED_TICK,
            "{rules:?}: {published:?}"
        );
        assert_eq!(
            published.last().map(|(_, state)| *state),
            finish_state,
            "{rules:?}: {published:?}"
        );
    }
}

#[test]
fn a_working_verdict_the_screen_supports_never_waits() {
    // Only the title is held back: working that a screen rule supports
    // publishes on the read that finds it, even when nothing else was drawn.
    let state =
        |state, visible_idle, visible_working, working_from_title_only| DetectionPublishState {
            state,
            visible_idle,
            visible_blocker: false,
            visible_working,
            working_from_title_only,
        };
    let decide = |working_from_title_only| {
        decide_detection_transition(
            DetectionTransitionInput {
                previous_publish: state(AgentState::Idle, true, false, false),
                next_publish: state(AgentState::Working, false, true, working_from_title_only),
                screen_changed: false,
                agent_changed: false,
                process_exited: false,
                stable_refresh_due: false,
                now: std::time::Instant::now(),
            },
            &mut PendingIdleConfirmation::default(),
        )
    };
    assert_eq!(decide(false), DetectionTransitionDecision::PublishNext);
    assert_eq!(decide(true), DetectionTransitionDecision::NoPublish);
}

#[test]
fn a_mature_title_only_turn_publishes_its_finish() {
    for rules in ALL_RULES {
        let (start, published) = run(
            rules,
            spin_over_finished(TITLE_ONLY_WORKING_GRACE + secs(1.0)),
            TITLE_ONLY_WORKING_GRACE * 3,
        );
        assert_eq!(start, AgentState::Idle);
        assert_eq!(
            published
                .iter()
                .map(|(_, state)| *state)
                .collect::<Vec<_>>(),
            vec![AgentState::Working, AgentState::Idle],
            "{rules:?}"
        );
    }
}

#[test]
fn a_matching_screen_rule_corroborates_a_title_working_verdict() {
    with_rules(Rules::TitleIdle, || {
        let title_only = detect(FINISHED, "busy-0").unwrap();
        assert_eq!(title_only.state, AgentState::Working);
        assert!(title_only.working_from_title_only);
        let corroborated = detect(WORKING, "busy-0").unwrap();
        assert_eq!(corroborated.state, AgentState::Working);
        assert!(!corroborated.working_from_title_only);
    });
}
