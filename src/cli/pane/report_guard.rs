use crate::platform::ForegroundProcess;

pub(super) fn should_skip() -> bool {
    nested_report(std::process::id(), crate::platform::hook_report_process)
}

/// Walk only the reporting process's ancestry. The pane shell is the last
/// ancestor below the Herdr server, whose executable name matches this CLI.
/// Missing observations do not prove nesting; a cycle is never counted twice.
fn nested_report(
    pid: u32,
    mut process: impl FnMut(u32) -> Option<(u32, ForegroundProcess)>,
) -> bool {
    let Some((mut parent, reporter)) = process(pid) else {
        return false;
    };
    let mut visited = std::collections::HashSet::from([pid]);
    let mut harnesses = 0;
    let mut native_codex_child = false;
    for _ in 0..128 {
        if parent == 0 || !visited.insert(parent) {
            break;
        }
        let Some((next, ancestor)) = process(parent) else {
            break;
        };
        if ancestor.name == reporter.name {
            break;
        }
        // The npm Codex entry point launches a native child. Those two
        // adjacent processes run one harness, not a nested conversation.
        let codex_launcher = native_codex_child
            && matches!(
                ancestor.name.as_str(),
                "node" | "node.exe" | "bun" | "bun.exe"
            );
        native_codex_child =
            crate::detect::identify_agent(&ancestor.name) == Some(crate::detect::Agent::Codex);
        let job = crate::platform::ForegroundJob {
            process_group_id: parent,
            processes: vec![ancestor],
        };
        if let Some((agent, _)) = crate::detect::identify_agent_in_job(&job) {
            if codex_launcher && agent == crate::detect::Agent::Codex {
                parent = next;
                continue;
            }
            harnesses += 1;
            if harnesses > 1 {
                return true;
            }
        }
        parent = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(
        entries: &[(u32, u32, &str, &[&str])],
    ) -> impl FnMut(u32) -> Option<(u32, ForegroundProcess)> {
        let entries: std::collections::HashMap<_, _> = entries
            .iter()
            .map(|(pid, parent, name, argv)| {
                (
                    *pid,
                    (
                        *parent,
                        ForegroundProcess {
                            pid: *pid,
                            name: (*name).into(),
                            argv0: argv.first().map(|value| (*value).into()),
                            argv: Some(argv.iter().map(|value| (*value).into()).collect()),
                            cmdline: None,
                        },
                    ),
                )
            })
            .collect();
        move |pid| entries.get(&pid).cloned()
    }

    #[test]
    fn nested_harness_reports_are_suppressed_across_shells_and_runtimes() {
        for nested in ["claude", "codex", "pi", "agy"] {
            assert!(
                nested_report(
                    60,
                    table(&[
                        (60, 50, "herdr", &["herdr"]),
                        (50, 40, "python3", &["python3", "-c", "hook"]),
                        (40, 30, nested, &[nested]),
                        (30, 20, "sh", &["sh", "-c", "nested command"]),
                        (
                            20,
                            10,
                            "node",
                            &[
                                "node",
                                "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
                            ]
                        ),
                        (10, 1, "zsh", &["zsh"]),
                        (1, 0, "herdr", &["herdr", "server"]),
                    ])
                ),
                "{nested}"
            );
        }
    }

    #[test]
    fn a_top_level_report_stops_at_the_pane_server() {
        assert!(!nested_report(
            50,
            table(&[
                (50, 40, "herdr", &["herdr"]),
                (40, 30, "sh", &["sh", "-c", "hook"]),
                (30, 20, "claude", &["claude"]),
                (20, 10, "zsh", &["zsh"]),
                (10, 1, "herdr", &["herdr", "server"]),
                (1, 0, "codex", &["codex"]),
            ])
        ));
    }

    #[test]
    fn a_native_codex_and_its_npm_launcher_are_one_harness() {
        assert!(!nested_report(
            50,
            table(&[
                (50, 40, "herdr", &["herdr"]),
                (40, 30, "codex", &["codex"]),
                (
                    30,
                    20,
                    "node",
                    &["node", "/opt/node_modules/@openai/codex/bin/codex.js"]
                ),
                (20, 10, "zsh", &["zsh"]),
                (10, 0, "herdr", &["herdr", "server"]),
            ])
        ));
    }

    #[test]
    fn missing_processes_and_cycles_do_not_invent_nested_harnesses() {
        assert!(!nested_report(50, table(&[])));
        assert!(!nested_report(
            50,
            table(&[
                (50, 40, "herdr", &["herdr"]),
                (40, 30, "claude", &["claude"]),
                (30, 40, "sh", &["sh"]),
            ])
        ));
    }

    #[test]
    fn hook_json_preserves_optional_report_fields_and_rejects_other_methods() {
        let leaf = super::super::bindings::LEAVES
            .iter()
            .find(|leaf| leaf.name == "report-agent")
            .unwrap();
        let value = serde_json::json!({
            "id": "hook:1", "method": "pane.report_agent", "params": {
                "pane_id": "w1:p1", "source": "herdr:pi", "agent": "pi",
                "state": "idle", "cancelled": true, "seq": 12,
                "agent_session_path": "/records/session.jsonl"
            }
        });
        let request = leaf
            .request(&[
                "--hook-json".into(),
                value.to_string(),
                "--check-only".into(),
            ])
            .unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), value);
        assert!(leaf
            .request(&[
                "--hook-json".into(),
                serde_json::json!({
                    "id": "bad", "method": "pane.close", "params": { "pane_id": "w1:p1" }
                })
                .to_string(),
                "--check-only".into()
            ])
            .is_err());
    }
}
