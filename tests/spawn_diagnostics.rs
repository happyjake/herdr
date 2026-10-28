#![cfg(unix)]

pub mod support;

#[test]
fn quiet_child_diagnostics_return_without_changing_pty_flags() {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new("/bin/sleep");
    command.arg("60");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let fd = pair.master.as_raw_fd().unwrap();
    let before = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let diagnostic = support::spawned_pty_diagnostics(&mut *child, &*pair.master);
    let after = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    child.kill().unwrap();
    drop(pair.master);
    child.wait().unwrap();
    assert_eq!(after, before);
    assert!(diagnostic.contains("still running"), "{diagnostic}");
}

#[test]
fn failed_child_diagnostics_include_pty_output_and_exit_status() {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.args(["-c", "printf 'startup diagnostic\\n' >&2; exit 17"]);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    // A PTY child may wait for its output to drain before exit completes.
    let mut diagnostic = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        diagnostic.push_str(&support::spawned_pty_diagnostics(
            &mut *child,
            &*pair.master,
        ));
        if diagnostic.contains("exit code: 17") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(pair.master);
    assert_eq!(child.wait().unwrap().exit_code(), 17);
    assert!(diagnostic.contains("startup diagnostic"), "{diagnostic}");
    assert!(diagnostic.contains("exit code: 17"), "{diagnostic}");
}
