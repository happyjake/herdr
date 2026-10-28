use crate::platform::ForegroundProcess;

pub(super) fn should_skip() -> bool {
    nested_report(std::process::id(), crate::platform::hook_report_process)
}

/// Walk the reporting process's ancestry up to the pane's Herdr server.
/// Launchers and helpers of the same agent are one harness, even when separated
/// by wrappers. Suppress only distinct agent identities in a complete chain;
/// unreadable processes, cycles and the depth limit all fail open.
fn nested_report(
    pid: u32,
    mut process: impl FnMut(u32) -> Option<(u32, ForegroundProcess)>,
) -> bool {
    let Some((mut parent, reporter)) = process(pid) else {
        return false;
    };
    if reporter.argv.as_ref().is_none_or(|argv| argv.is_empty()) {
        return false;
    }
    let mut visited = std::collections::HashSet::from([pid]);
    let mut first_harness = None;
    let mut nested = false;
    for _ in 0..128 {
        if parent == 0 {
            return nested;
        }
        if !visited.insert(parent) {
            return false;
        }
        let Some((next, ancestor)) = process(parent) else {
            return false;
        };
        let Some(argv) = ancestor.argv.as_ref().filter(|argv| !argv.is_empty()) else {
            return false;
        };
        if ancestor.name == reporter.name && argv.get(1).is_some_and(|arg| arg == "server") {
            return nested;
        }
        let job = crate::platform::ForegroundJob {
            process_group_id: parent,
            processes: vec![ancestor],
        };
        if let Some((agent, _)) = crate::detect::identify_agent_in_job(&job) {
            match first_harness {
                Some(first) if first != agent => nested = true,
                None => first_harness = Some(agent),
                _ => {}
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
        for nested in ["claude", "codex", "opencode", "agy"] {
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
    fn opencode_native_process_and_npm_launcher_are_one_harness() {
        assert!(!nested_report(
            50,
            table(&[
                (50, 40, "herdr", &["herdr"]),
                (40, 30, "sh", &["sh", "-c", "hook"]),
                (30, 20, "opencode", &["opencode"]),
                (
                    20,
                    10,
                    "node",
                    &["node", "/opt/node_modules/opencode-ai/bin/opencode"],
                ),
                (10, 0, "herdr", &["herdr", "server"]),
            ]),
        ));
    }

    #[test]
    fn windows_codex_native_node_and_cmd_launchers_are_one_harness() {
        assert!(!nested_report(
            50,
            table(&[
                (50, 40, "herdr.exe", &["herdr.exe"]),
                (40, 30, "codex.exe", &["codex.exe"]),
                (
                    30,
                    20,
                    "node.exe",
                    &[
                        "node.exe",
                        r"C:\npm\node_modules\@openai\codex\bin\codex.js"
                    ],
                ),
                (20, 10, "cmd.exe", &["cmd.exe", "/C", r"C:\npm\codex.cmd"]),
                (10, 0, "herdr.exe", &["herdr.exe", "server"]),
            ]),
        ));
    }

    #[test]
    fn same_harness_helpers_and_wrappers_do_not_suppress_reports() {
        for wrapper in [
            "node", "bun", "npx", "uv", "python3", "sh", "bash", "zsh", "fish", "login", "sudo",
            "env", "tmux", "ssh", "herdr",
        ] {
            for harness in ["claude", "codex", "pi", "opencode", "opencode2"] {
                assert!(
                    !nested_report(
                        50,
                        table(&[
                            (50, 40, "herdr", &["herdr"]),
                            (40, 30, harness, &[harness]),
                            (30, 20, wrapper, &[wrapper, harness]),
                            (20, 10, harness, &[harness]),
                            (10, 0, "herdr", &["herdr", "server"]),
                        ]),
                    ),
                    "{harness} via {wrapper}"
                );
            }
        }
    }

    #[test]
    fn distinct_harnesses_are_nested_across_herdr_helpers() {
        assert!(nested_report(
            50,
            table(&[
                (50, 40, "herdr", &["herdr"]),
                (40, 30, "claude", &["claude"]),
                (30, 20, "herdr", &["herdr", "agent", "run"]),
                (20, 10, "pi", &["pi"]),
                (10, 0, "herdr", &["herdr", "server"]),
            ]),
        ));
    }

    #[test]
    fn incomplete_ancestry_fails_open_even_after_distinct_harnesses() {
        let entries = [
            (50, 40, "herdr", &["herdr"][..]),
            (40, 30, "claude", &["claude"][..]),
            (30, 20, "pi", &["pi"][..]),
            (20, 10, "sh", &["sh"][..]),
            (10, 0, "herdr", &["herdr", "server"][..]),
        ];
        assert!(nested_report(50, table(&entries)));
        for (missing, ..) in entries {
            let mut processes = table(&entries);
            assert!(
                !nested_report(50, |pid| {
                    if pid == missing {
                        None
                    } else {
                        processes(pid)
                    }
                }),
                "unreadable process {missing}"
            );

            let mut processes = table(&entries);
            assert!(
                !nested_report(50, |pid| {
                    let (parent, mut process) = processes(pid)?;
                    if pid == missing {
                        process.argv = None;
                    }
                    Some((parent, process))
                }),
                "unreadable arguments for {missing}"
            );
        }
    }

    #[test]
    fn cyclic_or_overlong_ancestry_fails_open_after_distinct_harnesses() {
        assert!(!nested_report(
            50,
            table(&[
                (50, 40, "herdr", &["herdr"]),
                (40, 30, "claude", &["claude"]),
                (30, 40, "pi", &["pi"]),
            ]),
        ));
        assert!(!nested_report(1, |pid| {
            let name = match pid {
                1 => "herdr",
                2 => "claude",
                3 => "pi",
                _ => "sh",
            };
            table(&[(pid, pid + 1, name, &[name])])(pid)
        }));
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
