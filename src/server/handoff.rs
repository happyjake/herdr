#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::{info, warn};

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
pub(crate) const MAX_FDS_PER_HANDOFF: usize = 64;
// Sized so a long-lived agent transcript survives a handoff with a few
// thousand lines of scrollback intact (the phone pages through it), not
// just a couple of screens.
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 256 * 1024;
// Aggregate replay ceiling across all panes of one handoff. The manifest
// travels as ONE line and the importer rejects lines over
// MAX_MANIFEST_LINE_BYTES, so the raw replay total must leave room for
// JSON escaping of ANSI controls (~2x) plus the snapshot and pane
// metadata. The export loop divides this fairly: each pane gets
// min(MAX_REPLAY_BYTES_PER_PANE, MAX_REPLAY_BYTES_TOTAL / panes).
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_TOTAL: usize = 6 * 1024 * 1024;
#[cfg(unix)]
pub(crate) const MAX_MANIFEST_LINE_BYTES: usize = 16 * 1024 * 1024;
#[cfg(unix)]
pub(crate) const COMMIT_TIMEOUT: Duration = READY_TIMEOUT;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub version: u32,
    pub source_version: String,
    pub source_protocol: u32,
    pub expected_version: Option<String>,
    pub expected_protocol: Option<u32>,
    pub snapshot: crate::persist::SessionSnapshot,
    pub panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    /// An outer window title set over the API outlives the server that took the
    /// call, so a handoff carries it rather than falling back to the config.
    /// Absent from manifests written before this field existed.
    #[serde(default)]
    pub api_window_title: Option<String>,
}

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub stream: UnixStream,
}

#[cfg(unix)]
pub(crate) fn handoff_socket_path() -> PathBuf {
    crate::session::data_dir().join(format!("herdr-handoff-{}.sock", std::process::id()))
}

#[cfg(unix)]
pub(crate) fn spawn_handoff_import(
    import_exe: Option<&Path>,
    socket_path: &Path,
    token: &str,
) -> io::Result<Child> {
    let fallback_exe;
    let exe = if let Some(import_exe) = import_exe {
        import_exe
    } else {
        fallback_exe = std::env::current_exe().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to determine herdr executable path: {err}"),
            )
        })?;
        &fallback_exe
    };
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg("--handoff-import")
        .arg(socket_path)
        .arg(token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if crate::session::explicit_session_requested() {
        // The import child no longer has the original `--session` argument, so
        // stale socket overrides must not mask the inherited HERDR_SESSION.
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);
    }
    crate::platform::detach_server_daemon_command(&mut command);
    command.spawn().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn handoff import server at {}: {err}",
                exe.display()
            ),
        )
    })
}

#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => {
            info!(pid, status = %status, "handoff import server exited during rollback");
            return;
        }
        Ok(None) => {}
        Err(err) => {
            warn!(pid, err = %err, "failed to inspect handoff import server before rollback");
        }
    }

    if let Err(err) = child.kill() {
        warn!(pid, err = %err, "failed to kill handoff import server during rollback");
    }
    match child.wait() {
        Ok(status) => {
            info!(pid, status = %status, "handoff import server reaped during rollback");
        }
        Err(err) => {
            warn!(pid, err = %err, "failed to reap handoff import server during rollback");
        }
    }
}

#[cfg(unix)]
pub(crate) fn bind_listener(socket_path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    restrict_socket_permissions(socket_path)?;
    Ok(listener)
}

#[cfg(unix)]
pub(crate) fn accept_and_validate_on(
    listener: UnixListener,
    socket_path: &Path,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<UnixStream> {
    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    let token_line = read_line_unbuffered(&mut stream)?;
    if token_line.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff import token mismatch",
        ));
    }

    serde_json::to_writer(&mut stream, manifest).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let validated = read_line_unbuffered(&mut stream)?;
    if validated.trim_end() != "validated" {
        return Err(io::Error::other("handoff import did not validate manifest"));
    }
    let _ = std::fs::remove_file(socket_path);
    Ok(stream)
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS_PER_HANDOFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("handoff supports at most {MAX_FDS_PER_HANDOFF} pane file descriptors at once"),
        ));
    }
    send_fds(stream, fds)?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let restored = read_line_unbuffered(&mut *stream)?;
    if restored.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff import did not report restored runtimes",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let ready = read_line_unbuffered(&mut *stream)?;
    if ready.trim_end() != "ready" {
        return Err(io::Error::other("handoff import did not report ready"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_owned_ack(stream: &mut UnixStream) {
    if let Err(err) = stream.set_read_timeout(Some(OWNED_ACK_TIMEOUT)) {
        warn!(err = %err, "failed to set handoff ownership ack timeout");
        return;
    }
    match read_line_unbuffered(&mut *stream) {
        Ok(owned) if owned.trim_end() == "owned" => {}
        Ok(other) => {
            warn!(
                response = %other.trim_end(),
                "handoff import sent unexpected ownership ack after commit"
            );
        }
        Err(err) => {
            warn!(err = %err, "handoff import ownership ack was not received after commit");
        }
    }
}

#[cfg(unix)]
pub(crate) fn receive(socket_path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let manifest_line = read_line_unbuffered(&mut stream)?;
    let manifest: HandoffManifest =
        serde_json::from_str(&manifest_line).map_err(io::Error::other)?;
    if manifest.version != HANDOFF_VERSION {
        return Err(io::Error::other(format!(
            "unsupported handoff version {}",
            manifest.version
        )));
    }
    if manifest
        .expected_protocol
        .is_some_and(|protocol| protocol != crate::protocol::PROTOCOL_VERSION)
    {
        return Err(io::Error::other(format!(
            "handoff expected protocol {}, but this server speaks protocol {}",
            manifest.expected_protocol.unwrap_or_default(),
            crate::protocol::PROTOCOL_VERSION
        )));
    }
    if manifest
        .expected_version
        .as_deref()
        .is_some_and(|version| version != crate::build_info::version())
    {
        return Err(io::Error::other(format!(
            "handoff expected herdr v{}, but this server is v{}",
            manifest.expected_version.as_deref().unwrap_or("unknown"),
            crate::build_info::version()
        )));
    }
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    Ok(ReceivedHandoff {
        manifest,
        fds,
        stream,
    })
}

#[cfg(unix)]
pub(crate) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let committed = read_line_unbuffered(&mut *stream)?;
    if committed.trim_end() != "committed" {
        return Err(io::Error::other("handoff source did not commit"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn manifest_for(
    snapshot: crate::persist::SessionSnapshot,
    panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    expected_protocol: Option<u32>,
    expected_version: Option<String>,
    api_window_title: Option<String>,
) -> HandoffManifest {
    HandoffManifest {
        version: HANDOFF_VERSION,
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        expected_version,
        expected_protocol,
        snapshot,
        panes,
        api_window_title,
    }
}

/// Repair manifests from pre-self-describing exporters (their history
/// streams never carry `?1049h`; the screen mode lived only in
/// `input_state.alternate_screen`). For a PLAIN pane that flag is the app's
/// real state — a vim-style alternate-screen app must come back on the alt
/// screen, so its stream gets the mode switch prepended. For an AGENT pane
/// the flag is presumed forced by the old agent-identity heuristic (claude
/// and pi are primary-screen TUIs); honoring it is what marooned agent
/// transcripts on the alt screen, so those panes stay primary and heal.
/// New-format streams re-enter the alt screen themselves and pass through
/// untouched.
#[cfg(unix)]
pub(crate) fn honor_legacy_alternate_screen_panes(manifest: &mut HandoffManifest) {
    use std::collections::HashSet;

    let agent_panes: HashSet<u32> = manifest
        .snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
        .flat_map(|tab| tab.panes.iter())
        .filter(|(_, pane)| {
            pane.agent_session.is_some() || pane.agent_name.is_some() || pane.launch_argv.is_some()
        })
        .map(|(pane_id, _)| *pane_id)
        .collect();

    for pane in manifest.panes.iter_mut() {
        let carried_alternate = pane
            .input_state
            .as_ref()
            .is_some_and(|input_state| input_state.alternate_screen);
        let stream_sets_mode = pane
            .initial_history_ansi
            .as_deref()
            .is_some_and(|history| history.contains("\x1b[?1049h"));
        if carried_alternate && !stream_sets_mode && !agent_panes.contains(&pane.pane_id) {
            let history = pane.initial_history_ansi.take().unwrap_or_default();
            pane.initial_history_ansi = Some(format!("\x1b[?1049h{history}"));
        }
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(accepted),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff import connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn read_line_unbuffered(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff stream closed while reading line",
            ));
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
        if bytes.len() > MAX_MANIFEST_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff line exceeded maximum size",
            ));
        }
    }
}

#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() {
        return Ok(());
    }
    let byte = [b'F'];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to allocate fd control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    if expected == 0 {
        return Ok(Vec::new());
    }
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = expected * std::mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("handoff fd control message was truncated"));
    }

    let mut out = Vec::new();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
        }
        let data_len = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
        let count = data_len / std::mem::size_of::<RawFd>();
        let data = libc::CMSG_DATA(cmsg) as *const RawFd;
        for idx in 0..count {
            out.push(*data.add(idx));
        }
    }
    if out.len() != expected {
        for fd in out {
            let _ = unsafe { libc::close(fd) };
        }
        return Err(io::Error::other(format!(
            "expected {expected} handoff fds, received fewer"
        )));
    }
    Ok(out)
}

#[cfg(unix)]
pub(crate) fn log_import_result(panes: usize) {
    info!(panes, "handoff import ready");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn empty_snapshot() -> crate::persist::SessionSnapshot {
        crate::persist::SessionSnapshot {
            version: 0,
            workspaces: Vec::new(),
            active: None,
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    #[test]
    fn a_handoff_carries_an_api_set_window_title() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_manifest_written_before_the_title_field_still_loads() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
        );
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("api_window_title");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.api_window_title.is_none());
    }


    fn manifest_pane(
        pane_id: u32,
        alternate_screen: bool,
        initial_history_ansi: Option<&str>,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        crate::handoff_runtime::HandoffRuntimeState {
            pane_id,
            child_pid: 100 + pane_id,
            rows: 24,
            cols: 80,
            cell_width_px: 0,
            cell_height_px: 0,
            keyboard_protocol_flags: 0,
            keyboard_protocol_ansi: None,
            input_state: Some(crate::pane::InputState {
                alternate_screen,
                application_cursor: false,
                bracketed_paste: false,
                focus_reporting: false,
                mouse_protocol_mode: crate::input::MouseProtocolMode::None,
                mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Default,
                mouse_alternate_scroll: false,
                modify_other_keys: false,
            }),
            initial_history_ansi: initial_history_ansi.map(str::to_string),
        }
    }

    fn snapshot_with_agent_flags(panes: &[(u32, bool)]) -> crate::persist::SessionSnapshot {
        let pane_snapshots = panes
            .iter()
            .map(|(pane_id, is_agent)| {
                (
                    *pane_id,
                    crate::persist::PaneSnapshot {
                        cwd: "/tmp".into(),
                        label: None,
                        agent_name: None,
                        agent_session: is_agent.then(|| crate::persist::PaneAgentSessionSnapshot {
                            source: "claude-hooks".into(),
                            agent: "claude".into(),
                            kind: crate::agent_resume::AgentSessionRefKind::Id,
                            value: "session-1".into(),
                        }),
                        launch_argv: None,
                    },
                )
            })
            .collect();
        crate::persist::SessionSnapshot {
            version: 0,
            workspaces: vec![crate::persist::WorkspaceSnapshot {
                id: None,
                custom_name: None,
                identity_cwd: "/tmp".into(),
                worktree_space: None,
                public_pane_numbers: std::collections::HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![crate::persist::TabSnapshot {
                    custom_name: None,
                    layout: crate::persist::LayoutSnapshot::Pane(panes[0].0),
                    panes: pane_snapshots,
                    zoomed: false,
                    focused: None,
                    root_pane: Some(panes[0].0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    /// Legacy manifests (pre-self-describing exporters) carry the screen
    /// mode only in the input_state flag. A plain pane's flag is honored —
    /// its stream gets the mode switch prepended — while an agent pane's
    /// flag is presumed forced by the old agent-identity heuristic and the
    /// pane heals to the primary screen. New-format streams pass untouched.
    #[test]
    fn legacy_alternate_flags_honored_for_plain_panes_only() {
        let mut manifest = manifest_for(
            snapshot_with_agent_flags(&[(1, true), (2, false), (3, false), (4, false)]),
            vec![
                manifest_pane(1, true, Some("\x1b[Hclaude frame")),
                manifest_pane(2, true, Some("\x1b[Hvim frame")),
                manifest_pane(3, true, Some("\x1b[?1049h\x1b[Hnew-format frame")),
                manifest_pane(4, false, Some("primary history")),
            ],
            None,
            None,
        );

        honor_legacy_alternate_screen_panes(&mut manifest);

        // Poisoned agent pane heals: stream untouched, imports as primary.
        assert_eq!(
            manifest.panes[0].initial_history_ansi.as_deref(),
            Some("\x1b[Hclaude frame"),
        );
        // Genuine legacy alt pane: the mode switch is prepended.
        assert_eq!(
            manifest.panes[1].initial_history_ansi.as_deref(),
            Some("\x1b[?1049h\x1b[Hvim frame"),
        );
        // Self-describing stream passes through.
        assert_eq!(
            manifest.panes[2].initial_history_ansi.as_deref(),
            Some("\x1b[?1049h\x1b[Hnew-format frame"),
        );
        // Primary pane untouched.
        assert_eq!(
            manifest.panes[3].initial_history_ansi.as_deref(),
            Some("primary history"),
        );
    }

    /// A legacy alt pane with NO carried history still needs the mode
    /// switch so the app keeps drawing on the screen it believes it is on.
    #[test]
    fn legacy_alternate_flag_without_history_still_enters_alt() {
        let mut manifest = manifest_for(
            snapshot_with_agent_flags(&[(1, false)]),
            vec![manifest_pane(1, true, None)],
            None,
            None,
        );

        honor_legacy_alternate_screen_panes(&mut manifest);

        assert_eq!(
            manifest.panes[0].initial_history_ansi.as_deref(),
            Some("\x1b[?1049h"),
        );
    }

    /// The aggregate replay budget must fit the one-line manifest frame
    /// even after worst-case JSON escaping (~2x for ANSI-control-heavy
    /// content) plus generous room for the snapshot and pane metadata.
    #[test]
    fn replay_budget_fits_manifest_frame_limit() {
        assert!(MAX_REPLAY_BYTES_TOTAL * 2 + 2 * 1024 * 1024 <= MAX_MANIFEST_LINE_BYTES);
    }
}
