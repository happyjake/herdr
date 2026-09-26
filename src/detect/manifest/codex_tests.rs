//! Codex screens against the active codex manifest plus its built-in
//! supplement. Codex draws its composer and footer while a turn runs, so the
//! only idle evidence is the separator that closes a finished turn; screens
//! without it must stay unknown rather than fall back to idle.

use super::*;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "../../../tests/fixtures/agent-screen/codex/",
            $name
        ))
    };
}

fn assert_detected(
    screen: &str,
    state: AgentState,
    rule_id: Option<&str>,
    supplement: bool,
) -> DetectionExplain {
    // Scratch config and state dirs keep the bundled manifest active.
    let detected = with_codex_manifest_dirs("fixture", || explain(Agent::Codex, screen));
    assert!(matches!(detected.source, Some(ManifestSource::Bundled)));

    assert_eq!(detected.state, state);
    assert_eq!(
        detected.matched_rule.as_ref().map(|rule| rule.id.as_str()),
        rule_id
    );
    assert_eq!(
        detected.matched_rule.as_ref().map(|rule| rule.supplement),
        rule_id.map(|_| supplement)
    );
    assert_eq!(detected.visible_idle, state == AgentState::Idle);
    detected
}

fn assert_turn_complete(screen: &str) {
    assert_detected(
        screen,
        AgentState::Idle,
        Some("turn_complete_separator"),
        true,
    );
}

fn assert_ambiguous(screen: &str) {
    let detected = assert_detected(screen, AgentState::Unknown, None, false);
    assert_eq!(
        detected.fallback_reason.as_deref(),
        Some("codex_state_ambiguous")
    );
    assert!(detected
        .evaluated_rules
        .iter()
        .any(|rule| rule.supplement && rule.id == "turn_complete_separator" && !rule.matched));
}

#[test]
fn codex_finished_turn_separator_reads_idle() {
    assert_turn_complete(fixture!("idle-turn-complete.txt"));
    assert_turn_complete(fixture!("idle-short-turn.txt"));
}

#[test]
fn codex_running_or_resumed_turn_is_not_idle() {
    let working = assert_detected(
        fixture!("working.txt"),
        AgentState::Working,
        Some("screen_working_fallback"),
        false,
    );
    assert!(working.visible_working);

    // Mid-turn prose with no status line was the false idle the generic fallback produced.
    assert_ambiguous(fixture!("working-sentence-no-marker.txt"));
    // A prompt submitted after the separator starts a new turn.
    assert_ambiguous(fixture!("separator-then-submitted.txt"));

    let blocked = assert_detected(
        fixture!("blocked-after-separator.txt"),
        AgentState::Blocked,
        Some("live_strong_blocker"),
        false,
    );
    assert!(blocked.visible_blocker);
}

#[test]
fn codex_screens_without_turn_end_evidence_stay_unknown() {
    assert_ambiguous(fixture!("startup-banners.txt"));
    assert_ambiguous(fixture!("auth-expired.txt"));
    assert_ambiguous(fixture!("refusal-notice.txt"));
}

#[test]
fn codex_supplement_applies_under_a_remote_manifest_without_the_rule() {
    with_codex_manifest_dirs("supplement-remote", || {
        let remote = format!(
            "{}\n[[rules]]\nid = \"remote_working\"\nstate = \"working\"\ncontains = [\"remote-working-marker\"]\n",
            "id = \"codex\"\nversion = \"9999.01.01.1\"\nmin_engine_version = 3\nupdated_at = \"2026-06-10T12:00:00Z\"\n"
        );
        let path = crate::detect::manifest_update::remote_manifest_path(Agent::Codex);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, remote).unwrap();
        reload_manifests();

        let idle = explain(Agent::Codex, fixture!("idle-turn-complete.txt"));
        assert!(matches!(idle.source, Some(ManifestSource::Remote { .. })));
        assert_eq!(idle.manifest_version.as_deref(), Some("9999.01.01.1"));
        assert_eq!(idle.state, AgentState::Idle);
        assert!(idle.visible_idle);
        let rule = idle.matched_rule.expect("supplement rule matched");
        assert_eq!(rule.id, "turn_complete_separator");
        assert!(rule.supplement);

        // A remote rule that matches still wins over the supplement.
        let screen = format!(
            "{}remote-working-marker\n",
            fixture!("idle-turn-complete.txt")
        );
        let working = explain(Agent::Codex, &screen);
        assert_eq!(working.state, AgentState::Working);
        assert_eq!(
            working.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("remote_working")
        );
    });
}

#[test]
fn codex_supplement_defers_to_a_local_override() {
    with_codex_manifest_dirs("supplement-override", || {
        let path = override_path(Agent::Codex).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            "id = \"codex\"\n\n[[rules]]\nid = \"test\"\nstate = \"working\"\ncontains = [\"override-marker\"]\n",
        )
        .unwrap();
        reload_manifests();

        let detected = explain(Agent::Codex, fixture!("idle-turn-complete.txt"));
        assert!(matches!(detected.source, Some(ManifestSource::Override(_))));
        assert_eq!(detected.state, AgentState::Unknown);
        assert!(detected.matched_rule.is_none());
    });
}

fn with_codex_manifest_dirs<T>(name: &str, f: impl FnOnce() -> T) -> T {
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
