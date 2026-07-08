//! The codebuddy agent's bundled manifest against captured screens. The
//! engine tests next door cover rule semantics with synthetic manifests;
//! codebuddy has no other screen-level regression coverage, so its captured
//! states are pinned here.

use super::*;

fn osc_explain(
    agent: Agent,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> DetectionExplain {
    explain_with_input(
        agent,
        DetectionInput {
            screen,
            osc_title,
            osc_progress,
        },
    )
}

#[test]
fn codebuddy_manifest_detects_captured_idle_blocked_working_and_skip_states() {
    fn assert_codebuddy_blocked_by(screen: &str, expected_rule_id: &str) {
        let detected = explain(Agent::Codebuddy, screen);

        assert_eq!(detected.state, AgentState::Blocked);
        assert_eq!(
            detected.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some(expected_rule_id)
        );
        assert!(detected.visible_blocker);
    }

    fn assert_codebuddy_skips(screen: &str, expected_rule_id: &str) {
        let detected = explain(Agent::Codebuddy, screen);

        assert_eq!(detected.state, AgentState::Unknown);
        assert_eq!(
            detected.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some(expected_rule_id)
        );
        assert!(detected.skip_state_update);
        assert!(!detected.visible_idle);
        assert!(!detected.visible_blocker);
        assert!(!detected.visible_working);
    }

    let idle = explain(
        Agent::Codebuddy,
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/idle.txt"),
    );

    assert_eq!(idle.state, AgentState::Idle);
    assert_eq!(
        idle.matched_rule.as_ref().map(|rule| rule.id.as_str()),
        Some("live_prompt_box")
    );
    assert!(idle.visible_idle);

    let visible_working = explain(
        Agent::Codebuddy,
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/working.txt"),
    );

    assert_eq!(visible_working.state, AgentState::Working);
    assert_eq!(
        visible_working
            .matched_rule
            .as_ref()
            .map(|rule| rule.id.as_str()),
        Some("visible_working")
    );
    assert!(visible_working.visible_working);

    let osc_working = osc_explain(
        Agent::Codebuddy,
        "",
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/working-osc-title.txt"),
        "",
    );

    assert_eq!(osc_working.state, AgentState::Working);
    assert_eq!(
        osc_working
            .matched_rule
            .as_ref()
            .map(|rule| rule.id.as_str()),
        Some("osc_title_working")
    );
    assert!(osc_working.visible_working);

    assert_codebuddy_blocked_by(
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/trust.txt"),
        "workspace_trust_prompt",
    );

    assert_codebuddy_blocked_by(
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/permission.txt"),
        "bash_permission_prompt",
    );

    assert_codebuddy_blocked_by(
        include_str!(
            "../../../tests/fixtures/agent-screen/codebuddy/permission-selection-option-2.txt"
        ),
        "bash_permission_prompt",
    );

    assert_codebuddy_blocked_by(
        include_str!(
            "../../../tests/fixtures/agent-screen/codebuddy/permission-selection-option-3.txt"
        ),
        "bash_permission_prompt",
    );

    assert_codebuddy_blocked_by(
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/trust-narrow-wrapped.txt"),
        "workspace_trust_prompt",
    );

    let blocked_with_stale_spinner = osc_explain(
        Agent::Codebuddy,
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/permission.txt"),
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/working-osc-title.txt"),
        "",
    );

    assert_eq!(blocked_with_stale_spinner.state, AgentState::Blocked);
    assert_eq!(
        blocked_with_stale_spinner
            .matched_rule
            .as_ref()
            .map(|rule| rule.id.as_str()),
        Some("bash_permission_prompt")
    );
    assert!(blocked_with_stale_spinner.visible_blocker);

    assert_codebuddy_skips(
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/model-picker.txt"),
        "model_picker_menu",
    );

    assert_codebuddy_skips(
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/model-picker-session.txt"),
        "model_picker_menu",
    );

    assert_codebuddy_skips(
        include_str!("../../../tests/fixtures/agent-screen/codebuddy/transcript-viewer.txt"),
        "transcript_viewer",
    );
}
