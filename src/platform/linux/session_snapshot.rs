//! Procfs read policy shared by foreground detection and session snapshots.

pub(super) fn process_state_allows_remote_memory_read(state: char) -> bool {
    !matches!(state, 'D' | 'Z' | 'X' | 'x')
}

pub(super) fn process_allows_remote_memory_read(
    state: char,
    comm: &str,
    running_inside_wsl: bool,
) -> bool {
    process_state_allows_remote_memory_read(state)
        && (!running_inside_wsl || crate::detect::identify_agent(comm).is_none())
}

pub(super) fn read_argv(
    pid: u32,
    inside_wsl: bool,
    read_state: impl FnOnce(u32) -> Option<(i32, String, char)>,
    read: impl FnOnce(u32) -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    let (_, comm, state) = read_state(pid)?;
    process_allows_remote_memory_read(state, &comm, inside_wsl)
        .then(|| read(pid))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::session_process_snapshot_from_output;

    #[test]
    fn snapshot_keeps_uninterruptible_process_identity_without_reading_argv() {
        let mut reads = Vec::new();
        let processes = session_process_snapshot_from_output(
            "200 D Mon Oct 12 11:39:11 2026\n201 S Mon Oct 12 11:39:12 2026\n",
            |pid| {
                read_argv(
                    pid,
                    false,
                    |pid| Some((200, "helper".into(), if pid == 200 { 'D' } else { 'S' })),
                    |pid| {
                        reads.push(pid);
                        Some(vec![format!("process-{pid}")])
                    },
                )
            },
        );
        assert_eq!(reads, vec![201]);
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].pid, 200);
        assert_eq!(processes[0].started_at, "Mon Oct 12 11:39:11 2026");
        assert!(processes[0].argv.is_empty());
        assert_eq!(processes[1].argv, vec!["process-201"]);
    }

    #[test]
    fn snapshot_on_wsl_skips_known_agents_but_reads_wrappers() {
        for inside_wsl in [false, true] {
            let mut reads = Vec::new();
            let processes = session_process_snapshot_from_output(
                "200 S Mon Oct 12 11:39:11 2026\n201 S Mon Oct 12 11:39:12 2026\n",
                |pid| {
                    read_argv(
                        pid,
                        inside_wsl,
                        |pid| Some((200, if pid == 200 { "codex" } else { "node" }.into(), 'S')),
                        |pid| {
                            reads.push(pid);
                            Some(vec![format!("process-{pid}")])
                        },
                    )
                },
            );
            assert_eq!(
                reads,
                if inside_wsl {
                    vec![201]
                } else {
                    vec![200, 201]
                }
            );
            assert_eq!(processes.len(), 2);
            assert_eq!(processes[0].pid, 200);
            assert_eq!(processes[0].started_at, "Mon Oct 12 11:39:11 2026");
            assert_eq!(processes[0].argv.is_empty(), inside_wsl);
            assert_eq!(processes[1].argv, vec!["process-201"]);
        }
    }
}
