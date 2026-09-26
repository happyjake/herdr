//! Codex screens against the active codex manifest plus its built-in
//! supplement. Codex draws its composer and footer while a turn runs, so the
//! only idle evidence is the separator that closes a finished turn; screens
//! without it must stay unknown rather than fall back to idle.
//!
//! The supplement is owned by this codebase rather than by the remotely
//! updated manifest catalog, so its rule is pinned against captured screens
//! here; the negative controls assert only the resulting state, never the
//! bundled manifest's rule ids or priorities, and the engine precedence tests
//! use synthetic manifests and minimal strings.

use super::*;

const SUPPLEMENT_RULE: &str = "turn_complete_separator";

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "../../../tests/fixtures/agent-screen/codex/",
            $name
        ))
    };
}

fn explain_bundled(screen: &str) -> DetectionExplain {
    // Scratch config and state dirs keep the bundled manifest active.
    let detected = with_codex_manifest_dirs("fixture", || explain(Agent::Codex, screen));
    assert!(matches!(detected.source, Some(ManifestSource::Bundled)));
    detected
}

fn supplement_matched(detected: &DetectionExplain) -> bool {
    detected
        .matched_rule
        .as_ref()
        .is_some_and(|rule| rule.supplement && rule.id == SUPPLEMENT_RULE)
}

fn assert_turn_complete(screen: &str) {
    let detected = explain_bundled(screen);

    assert_eq!(detected.state, AgentState::Idle);
    assert!(supplement_matched(&detected));
    // A pane holds this verdict until it settles, and a working pane also
    // passes through the working-to-idle hold.
    assert!(detected.idle_needs_settling);
    assert!(!detected.visible_idle);
}

fn assert_not_turn_complete(screen: &str, state: AgentState) {
    let detected = explain_bundled(screen);

    assert_eq!(detected.state, state);
    assert!(!supplement_matched(&detected));
    assert!(!detected.idle_needs_settling);
    assert!(!detected.visible_idle);
}

#[test]
fn codex_finished_turn_separator_reads_idle() {
    assert_turn_complete(fixture!("idle-turn-complete.txt"));
    assert_turn_complete(fixture!("idle-same-day.txt"));
    // The highest reasoning effort draws » as the composer glyph.
    assert_turn_complete(fixture!("idle-ultra-effort.txt"));
    // An empty focused composer can draw a braille starfield after its glyph.
    assert_turn_complete(fixture!("idle-starfield-composer.txt"));
}

#[test]
fn codex_separator_quoted_while_an_answer_streams_reads_idle_for_a_frame() {
    // Codex hides its status line while an answer commits to history, so a
    // frame whose last committed line quotes a separator is indistinguishable
    // from a finished turn. Only the settle hold in the pane tells them apart.
    assert_turn_complete(fixture!("streaming-quoted-separator.txt"));
}

#[test]
fn codex_footer_or_tip_wrapped_to_column_zero_stays_unknown() {
    // Known false negatives on narrow panes: the anchor only accepts indented
    // footer and tip lines.
    assert_not_turn_complete(fixture!("footer-wrapped-narrow.txt"), AgentState::Unknown);
    assert_not_turn_complete(fixture!("tip-wrapped-narrow.txt"), AgentState::Unknown);
}

#[test]
fn codex_time_without_a_turn_duration_is_not_a_separator() {
    // Codex's time format strings suggest a turn under a minute may end with
    // a bare time, but that shape has not been observed on screen, and a time
    // alone is ordinary output; such a turn stays unknown.
    assert_not_turn_complete(fixture!("bare-time-short-turn.txt"), AgentState::Unknown);
    assert_not_turn_complete(fixture!("time-in-tool-output.txt"), AgentState::Unknown);
    assert_not_turn_complete(fixture!("date-time-in-prose.txt"), AgentState::Unknown);
    assert_not_turn_complete(fixture!("time-between-rules.txt"), AgentState::Unknown);
}

#[test]
fn codex_turn_started_below_a_separator_is_not_idle() {
    // The submitted prompt sits between the previous turn's separator and the
    // composer, whatever the composer draws after its glyph.
    assert_not_turn_complete(
        fixture!("sparkle-composer-bullet-working.txt"),
        AgentState::Working,
    );
    for screen in [
        fixture!("sparkle-composer-no-status.txt"),
        fixture!("sparkle-composer-hollow-working.txt"),
        fixture!("sparkle-composer-plain-working.txt"),
        fixture!("sparkle-composer-indented-prose.txt"),
    ] {
        let detected = explain_bundled(screen);
        assert_ne!(detected.state, AgentState::Idle);
        assert!(!supplement_matched(&detected));
    }
}

#[test]
fn codex_running_or_resumed_turn_is_not_idle() {
    assert_not_turn_complete(fixture!("working.txt"), AgentState::Working);
    // Mid-turn prose with no status line was the false idle the generic fallback produced.
    assert_not_turn_complete(
        fixture!("working-sentence-no-marker.txt"),
        AgentState::Unknown,
    );
    // A prompt submitted after the separator starts a new turn.
    assert_not_turn_complete(
        fixture!("separator-then-submitted.txt"),
        AgentState::Unknown,
    );
    assert_not_turn_complete(fixture!("blocked-after-separator.txt"), AgentState::Blocked);
}

#[test]
fn codex_screens_without_turn_end_evidence_stay_unknown() {
    assert_not_turn_complete(fixture!("startup-banners.txt"), AgentState::Unknown);
    assert_not_turn_complete(fixture!("auth-expired.txt"), AgentState::Unknown);
    assert_not_turn_complete(fixture!("refusal-notice.txt"), AgentState::Unknown);
}

const SEPARATOR_SCREEN: &str = "• Done.\n\n  Worked for 1m 2s · 9:41 AM\n\n›\n";

fn synthetic_codex_manifest(version: Option<&str>) -> String {
    let header = match version {
        Some(version) => format!(
            "id = \"codex\"\nversion = \"{version}\"\nmin_engine_version = 3\nupdated_at = \"2026-06-10T12:00:00Z\"\n"
        ),
        None => "id = \"codex\"\n".to_string(),
    };
    format!(
        "{header}\n[[rules]]\nid = \"test_working\"\nstate = \"working\"\ncontains = [\"working-marker\"]\n"
    )
}

#[test]
fn codex_supplement_applies_only_when_the_active_remote_manifest_has_no_match() {
    with_codex_manifest_dirs("supplement-remote", || {
        let path = crate::detect::manifest_update::remote_manifest_path(Agent::Codex);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, synthetic_codex_manifest(Some("9999.01.01.1"))).unwrap();
        reload_manifests();

        let idle = explain(Agent::Codex, SEPARATOR_SCREEN);
        assert!(matches!(idle.source, Some(ManifestSource::Remote { .. })));
        assert_eq!(idle.manifest_version.as_deref(), Some("9999.01.01.1"));
        assert_eq!(idle.state, AgentState::Idle);
        assert!(supplement_matched(&idle));

        // A matching rule in the active manifest wins over the supplement.
        let working = explain(Agent::Codex, &format!("working-marker\n{SEPARATOR_SCREEN}"));
        assert_eq!(working.state, AgentState::Working);
        let rule = working.matched_rule.expect("remote rule matched");
        assert_eq!(rule.id, "test_working");
        assert!(!rule.supplement);
        assert!(!working.evaluated_rules.iter().any(|rule| rule.supplement));

        let no_separator = explain(Agent::Codex, "• Still thinking about it.\n\n›\n");
        assert_eq!(no_separator.state, AgentState::Unknown);
        assert_eq!(
            no_separator.fallback_reason.as_deref(),
            Some("codex_state_ambiguous")
        );
    });
}

#[test]
fn codex_supplement_defers_to_a_local_override() {
    with_codex_manifest_dirs("supplement-override", || {
        let path = override_path(Agent::Codex).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, synthetic_codex_manifest(None)).unwrap();
        reload_manifests();

        let detected = explain(Agent::Codex, SEPARATOR_SCREEN);
        assert!(matches!(detected.source, Some(ManifestSource::Override(_))));
        assert_eq!(detected.state, AgentState::Unknown);
        assert!(detected.matched_rule.is_none());
    });
}

pub(crate) fn with_codex_manifest_dirs<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    let old_config = std::env::var_os("XDG_CONFIG_HOME");
    let old_state = std::env::var_os("XDG_STATE_HOME");
    let base = std::env::temp_dir().join(format!(
        "herdr-codex-manifest-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::env::set_var("XDG_CONFIG_HOME", base.join("config"));
    std::env::set_var("XDG_STATE_HOME", base.join("state"));
    reload_manifests();
    let result = f();
    match old_config {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match old_state {
        Some(value) => std::env::set_var("XDG_STATE_HOME", value),
        None => std::env::remove_var("XDG_STATE_HOME"),
    }
    reload_manifests();
    let _ = std::fs::remove_dir_all(&base);
    result
}
