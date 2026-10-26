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
// Descriptors are transferred in batches of this size. A single SCM_RIGHTS
// control message caps out at 253 descriptors on Linux and 254 on macOS, so the
// batch stays well below both limits and the number of panes stays unbounded.
#[cfg(unix)]
const FDS_PER_MESSAGE: usize = 64;
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

/// Manifest extension advertising that the exporter can carry each pane's
/// replay as a per-pane frame before the descriptor transfer instead of
/// inline under the legacy budgets. The manifest keeps its legacy shape —
/// version, inline replay, and all — so an importer that predates frames
/// ignores the unknown field, answers with a plain `validated`, and gets a
/// legacy export; an importer that understands frames answers
/// `validated replay-frames` and the frames follow. The handshake cannot
/// negotiate in the other direction: the importer speaks first with a token
/// line a legacy exporter exact-matches, so any importer-initiated
/// capability byte would desync a legacy exporter's `validated` read.
#[cfg(unix)]
pub(crate) const EXTENSION_REPLAY_FRAMES: &str = "replay-frames";
/// The validation reply that acknowledges the replay-frames extension. Any
/// change to the frame format needs a new extension name: this ack is the
/// whole negotiation.
#[cfg(unix)]
const VALIDATED_REPLAY_FRAMES: &str = "validated replay-frames";
/// Importer-side sanity cap for one pane's replay frame. A frame larger than
/// this is a corrupt stream, not history: the frame budget an exporter
/// computes is clamped to this value, so a legitimate frame never exceeds it.
#[cfg(unix)]
pub(crate) const MAX_REPLAY_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Aggregate payload/storage ceiling while both servers and their terminals
/// are alive. Enforced independently of the per-frame wire sanity limit.
#[cfg(unix)]
pub(crate) const MAX_REPLAY_FRAMES_TOTAL: usize = 32 * 1024 * 1024;
/// Replay frame wire format: magic, pane index (its position in
/// `manifest.panes`), payload length, payload. The index lets the importer
/// catch a desync that a small garbage length would otherwise hide.
#[cfg(unix)]
const REPLAY_FRAME_MAGIC: [u8; 8] = *b"HRD-RPL1";
#[cfg(unix)]
const REPLAY_FRAME_HEADER_BYTES: usize = 8 + 4 + 8;

/// How one handoff carries pane replay, settled when the importer answers
/// the manifest: it either validated the legacy inline replay or
/// acknowledged the frame extension.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayTransport {
    /// Replay rides inline in the manifest under the legacy budgets.
    InlineLegacy,
    /// Replay precedes the descriptor transfer as one frame per pane.
    ReplayFrames,
}

/// Cap actual rendered bytes, not an estimate based on terminal storage.
#[cfg(unix)]
pub(crate) fn replay_frame_budget(rendered_bytes: usize) -> usize {
    rendered_bytes
        .min(MAX_REPLAY_FRAME_BYTES)
        .min(MAX_REPLAY_FRAMES_TOTAL)
}

/// Water-fill the aggregate budget: satisfy small histories first, then
/// share the remainder among larger ones. Return budgets in manifest order.
#[cfg(unix)]
pub(crate) fn replay_frame_budgets(rendered_bytes: &[usize]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rendered_bytes.len()).collect();
    order.sort_by_key(|index| (rendered_bytes[*index], *index));
    let mut budgets = vec![0; rendered_bytes.len()];
    let mut remaining = MAX_REPLAY_FRAMES_TOTAL;
    for (position, index) in order.into_iter().enumerate() {
        let share = remaining / (rendered_bytes.len() - position);
        budgets[index] = replay_frame_budget(rendered_bytes[index]).min(share);
        remaining -= budgets[index];
    }
    budgets
}

/// Keep a suffix of complete rendered rows. The terminal formatter resets
/// styles before newlines, so a row boundary is also an ANSI boundary.
/// Preserve alternate-screen entry even when no content row fits. A nonempty
/// reset-only frame distinguishes an intentional cut from a missing frame.
#[cfg(unix)]
pub(crate) fn budget_replay_frame(history: String, budget: usize) -> io::Result<(String, usize)> {
    if history.len() <= budget {
        return Ok((history, 0));
    }
    let alternate_prefix = "\x1b[?1049h\x1b[H";
    let (prefix, preserved_bytes) = if history.starts_with(alternate_prefix) {
        (alternate_prefix, alternate_prefix.len())
    } else {
        ("\x1b[0m", 0)
    };
    if budget < prefix.len() {
        return Err(io::Error::other(
            "handoff replay budget cannot retain terminal mode",
        ));
    }
    let earliest = history.len() - (budget - prefix.len());
    let start = if earliest > 0 && history.as_bytes()[earliest - 1] == b'\n' {
        earliest
    } else {
        history.as_bytes()[earliest..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(history.len(), |offset| earliest + offset + 1)
    };
    Ok((
        format!("{prefix}{}", &history[start..]),
        start - preserved_bytes,
    ))
}

/// The importer replays every frame through its terminal parsers before it
/// reports restored, so the exporter's wait scales with what it sent on top
/// of a floor that covers the legacy-sized restores. A generous scale costs
/// nothing: a dead importer surfaces as end-of-file, not as a timeout, and a
/// spurious timeout is safe because the importer never touches the pane
/// descriptors before a commit it will not receive.
#[cfg(unix)]
pub(crate) fn replay_restored_timeout(replay_bytes: usize) -> Duration {
    READY_TIMEOUT + Duration::from_secs((replay_bytes / (8 * 1024 * 1024)) as u64)
}

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
    /// A host restart or shutdown announced to the old server but not yet
    /// committed. Absent from manifests written before this field existed.
    #[serde(default)]
    pub host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
    /// Capabilities of the exporter the importer may opt into in its
    /// validation reply. Unknown to importers that predate it, and serde
    /// ignores it there, so a legacy importer still sees a legacy manifest.
    /// That tolerance is the rollback contract: this struct must never gain
    /// `deny_unknown_fields`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
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
) -> io::Result<(UnixStream, ReplayTransport)> {
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
    // Replay frames require the importer's explicit acknowledgment. Every legacy
    // importer replies with plain validated and therefore keeps the legacy
    // replay limits, preserving rollback safety. Version fields validate the
    // target only; they do not select the replay transport.
    let transport = match validated.trim_end() {
        "validated" => ReplayTransport::InlineLegacy,
        VALIDATED_REPLAY_FRAMES => {
            if !manifest
                .extensions
                .iter()
                .any(|extension| extension == EXTENSION_REPLAY_FRAMES)
            {
                return Err(io::Error::other(
                    "handoff import acknowledged replay frames the manifest never advertised",
                ));
            }
            ReplayTransport::ReplayFrames
        }
        _ => return Err(io::Error::other("handoff import did not validate manifest")),
    };
    let _ = std::fs::remove_file(socket_path);
    Ok((stream, transport))
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    send_fds(stream, fds)?;

    wait_restored(stream, READY_TIMEOUT)
}

#[cfg(unix)]
pub(crate) fn wait_restored(stream: &mut UnixStream, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    let restored = read_line_unbuffered(&mut *stream)?;
    if restored.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff import did not report restored runtimes",
        ));
    }
    Ok(())
}

/// One pane's replay as a frame before the descriptor transfer. An empty or
/// absent history travels as a zero-length payload.
#[cfg(unix)]
pub(crate) fn send_replay_frame(
    stream: &mut UnixStream,
    pane_index: u32,
    payload: Option<&str>,
) -> io::Result<()> {
    let payload = payload.unwrap_or_default();
    let mut header = [0u8; REPLAY_FRAME_HEADER_BYTES];
    header[..8].copy_from_slice(&REPLAY_FRAME_MAGIC);
    header[8..12].copy_from_slice(&pane_index.to_le_bytes());
    header[12..20].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload.as_bytes())?;
    Ok(())
}

/// Read one frame per pane, in manifest order, and replace each pane's
/// inline replay with the frame's fuller history.
#[cfg(unix)]
fn recv_replay_frames(
    stream: &UnixStream,
    panes: &mut [crate::handoff_runtime::HandoffRuntimeState],
) -> io::Result<()> {
    let mut stream = stream;
    let mut received_bytes = 0usize;
    let mut retained_bytes: usize = panes
        .iter()
        .map(|pane| pane.initial_history_ansi.as_ref().map_or(0, String::len))
        .sum();
    if retained_bytes > MAX_REPLAY_FRAMES_TOTAL {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff aggregate replay exceeds limit",
        ));
    }
    for (index, pane) in panes.iter_mut().enumerate() {
        let mut header = [0u8; REPLAY_FRAME_HEADER_BYTES];
        stream.read_exact(&mut header)?;
        if header[..8] != REPLAY_FRAME_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("handoff replay frame {index} had a bad magic"),
            ));
        }
        let pane_index = u32::from_le_bytes(
            header[8..12]
                .try_into()
                .expect("header slice has four bytes"),
        );
        if pane_index as usize != index {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("handoff replay frame {index} arrived out of order as {pane_index}"),
            ));
        }
        let length = u64::from_le_bytes(
            header[12..20]
                .try_into()
                .expect("header slice has eight bytes"),
        );
        if length > MAX_REPLAY_FRAME_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("handoff replay frame {index} announced {length} bytes"),
            ));
        }
        // Check the untrusted u64 before converting or allocating. Include
        // still-retained inline fallbacks in the storage bound as well.
        let inline_bytes = pane.initial_history_ansi.as_ref().map_or(0, String::len);
        let retained_without_pane = retained_bytes - inline_bytes;
        if length > (MAX_REPLAY_FRAMES_TOTAL - received_bytes) as u64
            || (length != 0 && length > (MAX_REPLAY_FRAMES_TOTAL - retained_without_pane) as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff aggregate replay exceeds limit",
            ));
        }
        received_bytes += length as usize;
        if length == 0 {
            // Missing replay retains the legacy fallback, counted above.
            continue;
        }
        pane.initial_history_ansi = None;
        retained_bytes = retained_without_pane + length as usize;
        let mut payload = vec![0u8; length as usize];
        stream.read_exact(&mut payload)?;
        let history = String::from_utf8(payload)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        pane.initial_history_ansi = Some(history);
    }
    info!(
        replay_bytes = received_bytes,
        retained_bytes, "live handoff replay received"
    );
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
    let mut manifest: HandoffManifest =
        serde_json::from_str(&manifest_line).map_err(io::Error::other)?;
    drop(manifest_line);
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
    // A manifest that advertises replay frames gets them: the ack names the
    // extension so the exporter knows which transport to use. An exporter
    // that predates frames never advertises and gets the plain legacy ack.
    let replay_frames = manifest
        .extensions
        .iter()
        .any(|extension| extension == EXTENSION_REPLAY_FRAMES);
    if replay_frames {
        stream
            .write_all(VALIDATED_REPLAY_FRAMES.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))?;
    } else {
        stream.write_all(b"validated\n")?;
    }
    stream.flush()?;
    // Frames travel before the descriptors: a descriptor batch is delivered
    // by recvmsg, which on macOS can glue bytes that follow it onto the
    // record the fd reader consumes — so nothing may follow the batches.
    // Frames are read with exact lengths and can be over-read into nothing.
    if replay_frames {
        recv_replay_frames(&stream, &mut manifest.panes)?;
    }
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
    host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
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
        host_shutdown_intent,
        extensions: vec![EXTENSION_REPLAY_FRAMES.to_string()],
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
        let carried_alternate = pane.carries_alternate_screen();
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
pub(crate) fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    for batch in fds.chunks(FDS_PER_MESSAGE) {
        send_fd_batch(stream, batch)?;
    }
    Ok(())
}

#[cfg(unix)]
fn send_fd_batch(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
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
fn close_raw_fds(fds: &[RawFd]) {
    for fd in fds {
        let _ = unsafe { libc::close(*fd) };
    }
}

#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    let mut out: Vec<RawFd> = Vec::with_capacity(expected);
    while out.len() < expected {
        let wanted = (expected - out.len()).min(FDS_PER_MESSAGE);
        let batch = match recv_fd_batch(stream, wanted) {
            Ok(batch) => batch,
            Err(err) => {
                close_raw_fds(&out);
                return Err(err);
            }
        };
        if batch.is_empty() {
            let received = out.len();
            close_raw_fds(&out);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "handoff stream closed after {received} of {expected} pane file descriptors"
                ),
            ));
        }
        out.extend(batch);
    }
    Ok(out)
}

#[cfg(unix)]
fn recv_fd_batch(stream: &UnixStream, wanted: usize) -> io::Result<Vec<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = wanted * std::mem::size_of::<RawFd>();
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

    let mut out = Vec::new();
    unsafe {
        let control_end = control.as_ptr() as usize + msg.msg_controllen as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                // Bound the payload by both the header's own length and the
                // bytes the kernel wrote into `control`, so the read below can
                // never run past the buffer.
                let available = control_end.saturating_sub(data as usize);
                let data_len = ((*cmsg).cmsg_len as usize)
                    .saturating_sub(libc::CMSG_LEN(0) as usize)
                    .min(available);
                let count = data_len / std::mem::size_of::<RawFd>();
                let data = data as *const RawFd;
                for idx in 0..count {
                    out.push(*data.add(idx));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    // Truncation means the kernel closed the descriptors that did not fit, so
    // the batch is unrecoverable rather than merely short.
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        close_raw_fds(&out);
        return Err(io::Error::other("handoff fd control message was truncated"));
    }
    if read == 0 {
        close_raw_fds(&out);
        return Ok(Vec::new());
    }
    if out.len() > wanted {
        let received = out.len();
        close_raw_fds(&out);
        return Err(io::Error::other(format!(
            "handoff fd message carried {received} descriptors, expected at most {wanted}"
        )));
    }
    if out.is_empty() {
        return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
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
            None,
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_handoff_carries_an_announced_host_shutdown() {
        let intent: crate::platform::HostShutdownIntent =
            serde_json::from_value(serde_json::json!({ "announced_at_ns": 42 }))
                .expect("intent should deserialize");
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, Some(intent));
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");
        let decoded: HandoffManifest =
            serde_json::from_slice(&encoded).expect("manifest should deserialize");

        assert_eq!(decoded.host_shutdown_intent, Some(intent));
    }

    #[test]
    fn a_manifest_written_before_the_host_shutdown_field_still_loads() {
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("host_shutdown_intent");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.host_shutdown_intent.is_none());
    }

    #[test]
    fn an_older_server_imports_a_manifest_with_an_announced_host_shutdown() {
        /// The manifest as servers before `host_shutdown_intent` decode it,
        /// frozen here so the new field stays ignorable by them.
        #[derive(Deserialize)]
        #[allow(dead_code)] // Only decoding is under test.
        struct PreHostShutdownManifest {
            version: u32,
            source_version: String,
            source_protocol: u32,
            expected_version: Option<String>,
            expected_protocol: Option<u32>,
            snapshot: crate::persist::SessionSnapshot,
            panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
            #[serde(default)]
            api_window_title: Option<String>,
        }

        let intent: crate::platform::HostShutdownIntent =
            serde_json::from_value(serde_json::json!({ "announced_at_ns": 42 }))
                .expect("intent should deserialize");
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
            Some(intent),
        );
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");

        let older: PreHostShutdownManifest =
            serde_json::from_slice(&encoded).expect("an older server should still load it");

        assert_eq!(older.version, HANDOFF_VERSION);
        assert_eq!(older.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_manifest_written_before_the_title_field_still_loads() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
            None,
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
            terminal_title: None,
            input_state: Some(crate::pane::InputState {
                alternate_screen,
                application_cursor: false,
                bracketed_paste: false,
                focus_reporting: false,
                mouse_protocol_mode: crate::input::MouseProtocolMode::None,
                mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Default,
                mouse_alternate_scroll: false,
                modify_other_keys: false,
                color_scheme_reporting: false,
            }),
            initial_history_ansi: initial_history_ansi.map(str::to_string),
            agent_state: None,
        }
    }

    fn snapshot_with_agent_flags(panes: &[(u32, bool)]) -> crate::persist::SessionSnapshot {
        let pane_snapshots = panes
            .iter()
            .map(|(pane_id, is_agent)| {
                (
                    *pane_id,
                    crate::persist::PaneSnapshot {
                        ended_agent: None,
                        started_by: None,
                        session_reported: false,
                        starter_label: None,
                        cwd: "/tmp".into(),
                        label: None,
                        agent_name: None,
                        managed_agent_kind: None,
                        agent_resume: None,
                        agent_session: is_agent.then(|| crate::persist::PaneAgentSessionSnapshot {
                            source: "claude-hooks".into(),
                            agent: "claude".into(),
                            kind: crate::agent_resume::AgentSessionRefKind::Id,
                            value: "session-1".into(),
                            record_path: None,
                        }),
                        launch_argv: None,
                        agent_status: None,
                        agent_status_changed_at: None,
                        agent_status_resolve_by: None,
                        agent_status_saw_other: false,
                        agent_status_reported_through: None,
                        agent_status_captured_pair_reported: true,
                        pinned: false,
                        flag: None,
                        flag_set_through: None,
                        label_source: None,
                        label_at: None,
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
        const { assert!(MAX_REPLAY_BYTES_TOTAL * 2 + 2 * 1024 * 1024 <= MAX_MANIFEST_LINE_BYTES) }
    }

    #[test]
    fn a_handoff_manifest_advertises_replay_frames() {
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        assert!(manifest
            .extensions
            .iter()
            .any(|extension| extension == EXTENSION_REPLAY_FRAMES));
    }

    #[test]
    fn a_manifest_written_before_extensions_still_loads() {
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("extensions");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("a pre-extensions manifest should still load");

        assert!(older.extensions.is_empty());
    }

    /// Rollback safety pinned: the manifest as an importer that predates
    /// replay frames decodes it. The extension advertisement must stay
    /// invisible to it, inline replay and all.
    #[test]
    fn a_legacy_importer_ignores_the_replay_frames_advertisement() {
        #[derive(Deserialize)]
        #[allow(dead_code)] // Only decoding is under test.
        struct LegacyManifest {
            version: u32,
            source_version: String,
            source_protocol: u32,
            expected_version: Option<String>,
            expected_protocol: Option<u32>,
            snapshot: crate::persist::SessionSnapshot,
            panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
            #[serde(default)]
            api_window_title: Option<String>,
            #[serde(default)]
            host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
        }

        let manifest = manifest_for(
            empty_snapshot(),
            vec![manifest_pane(1, false, Some("legacy inline replay"))],
            None,
            None,
            None,
            None,
        );
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");

        let legacy: LegacyManifest =
            serde_json::from_slice(&encoded).expect("a legacy importer should still load it");

        assert_eq!(legacy.version, HANDOFF_VERSION);
        assert_eq!(
            legacy.panes[0].initial_history_ansi.as_deref(),
            Some("legacy inline replay"),
        );
    }

    /// Short unique socket paths: macOS caps a unix socket path at 104
    /// bytes and the default temp directory is already long.
    fn test_socket_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "hho-{}-{}-{tag}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Play the importer side of the accept handshake: connect, token, read
    /// the manifest, answer with `reply`.
    fn scripted_importer(socket_path: PathBuf, token: String, reply: &'static str) -> String {
        let mut stream = UnixStream::connect(socket_path).expect("importer should connect");
        stream.write_all(token.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        let _manifest_line = read_line_unbuffered(&mut stream).expect("manifest line");
        stream.write_all(reply.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        // The exporter answers a bad reply by hanging up; a good reply keeps
        // the stream open for what follows.
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => "closed".to_string(),
            Ok(_) => "data".to_string(),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => "open".to_string(),
            Err(err) => panic!("unexpected importer read error: {err}"),
        }
    }

    fn accept_with_scripted_reply(
        manifest: &HandoffManifest,
        reply: &'static str,
    ) -> (io::Result<ReplayTransport>, String) {
        let socket_path = test_socket_path("accept");
        let _ = std::fs::remove_file(&socket_path);
        let listener = bind_listener(&socket_path).expect("listener should bind");
        let token = "test-token".to_string();
        let importer = {
            let socket_path = socket_path.clone();
            let token = token.clone();
            std::thread::spawn(move || scripted_importer(socket_path, token, reply))
        };
        let accepted = accept_and_validate_on(listener, &socket_path, &token, manifest);
        let outcome = importer.join().expect("importer thread should finish");
        let _ = std::fs::remove_file(&socket_path);
        (accepted.map(|(_, transport)| transport), outcome)
    }

    fn advertise_manifest() -> HandoffManifest {
        manifest_for(empty_snapshot(), Vec::new(), None, None, None, None)
    }

    #[test]
    fn a_plain_validated_answer_keeps_the_legacy_inline_transport() {
        let (accepted, outcome) = accept_with_scripted_reply(&advertise_manifest(), "validated");
        assert_eq!(
            accepted.expect("a plain validated answer should be accepted"),
            ReplayTransport::InlineLegacy
        );
        assert_eq!(outcome, "open");
    }

    #[test]
    fn a_replay_frames_answer_switches_to_the_frame_transport() {
        let (accepted, outcome) =
            accept_with_scripted_reply(&advertise_manifest(), "validated replay-frames");
        assert_eq!(
            accepted.expect("a replay-frames answer should be accepted"),
            ReplayTransport::ReplayFrames
        );
        assert_eq!(outcome, "open");
    }

    #[test]
    fn a_replay_frames_answer_without_the_advertisement_fails_the_handoff() {
        // An importer that acknowledges frames the manifest never offered is
        // not a negotiating peer; it is a broken one.
        let mut manifest = advertise_manifest();
        manifest.extensions.clear();
        let (accepted, _) = accept_with_scripted_reply(&manifest, "validated replay-frames");
        assert!(accepted.is_err());
    }

    #[test]
    fn an_unexpected_validation_answer_fails_the_handoff() {
        let (accepted, outcome) =
            accept_with_scripted_reply(&advertise_manifest(), "validated anything-else");
        assert!(accepted.is_err());
        assert_eq!(outcome, "closed");
    }

    #[test]
    fn replay_frames_round_trip_overlays_each_panes_history() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair");
        // A read timeout turns a regression that waits for one frame too many
        // into a failure instead of a hung test binary.
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut panes = vec![
            manifest_pane(1, false, Some("legacy cut")),
            manifest_pane(2, false, None),
            manifest_pane(3, false, Some("legacy cut")),
        ];
        let frames: [Option<&str>; 3] =
            [Some("full history one"), None, Some("full history three")];
        for (index, frame) in frames.iter().enumerate() {
            send_replay_frame(&mut sender, index as u32, *frame).expect("frame should send");
        }
        drop(sender);

        recv_replay_frames(&receiver, &mut panes).expect("frames should receive");

        assert_eq!(
            panes[0].initial_history_ansi.as_deref(),
            Some("full history one")
        );
        assert_eq!(panes[1].initial_history_ansi, None);
        assert_eq!(
            panes[2].initial_history_ansi.as_deref(),
            Some("full history three")
        );
    }

    #[test]
    fn an_empty_frame_keeps_the_inline_legacy_replay() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut panes = vec![manifest_pane(1, false, Some("legacy cut"))];
        send_replay_frame(&mut sender, 0, None).expect("frame should send");
        drop(sender);

        recv_replay_frames(&receiver, &mut panes).expect("frames should receive");

        // A bigger budget never legitimately renders less than the inline
        // legacy replay, so an empty frame must not clear it.
        assert_eq!(panes[0].initial_history_ansi.as_deref(), Some("legacy cut"));
    }

    #[test]
    fn replay_frames_reject_a_bad_magic() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut panes = vec![manifest_pane(1, false, None)];
        let mut header = [0u8; REPLAY_FRAME_HEADER_BYTES];
        header[..8].copy_from_slice(b"HRD-RPL9");
        sender.write_all(&header).unwrap();
        drop(sender);

        let err = recv_replay_frames(&receiver, &mut panes).expect_err("bad magic must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn replay_frames_reject_an_out_of_order_pane() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut panes = vec![manifest_pane(1, false, None), manifest_pane(2, false, None)];
        send_replay_frame(&mut sender, 1, Some("pane two first")).unwrap();
        drop(sender);

        let err = recv_replay_frames(&receiver, &mut panes).expect_err("out of order must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn replay_frames_reject_an_oversize_payload_before_reading_it() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut panes = vec![manifest_pane(1, false, None)];
        let mut header = [0u8; REPLAY_FRAME_HEADER_BYTES];
        header[..8].copy_from_slice(&REPLAY_FRAME_MAGIC);
        header[12..20].copy_from_slice(&((MAX_REPLAY_FRAME_BYTES as u64) + 1).to_le_bytes());
        sender.write_all(&header).unwrap();
        drop(sender);
        // No payload follows: a reader that accepted the length would block
        // until the sender closed.

        let err = recv_replay_frames(&receiver, &mut panes).expect_err("oversize must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn replay_frame_budget_clamps_to_the_wire_cap() {
        assert_eq!(
            replay_frame_budget(crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES),
            crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES
        );
        assert_eq!(replay_frame_budget(1), 1);
        assert_eq!(replay_frame_budget(usize::MAX), 32 * 1024 * 1024);
    }

    #[test]
    fn handoff_rejects_aggregate_replay_before_reading_the_excess_payload() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let writer = std::thread::spawn(move || {
            send_replay_frame(&mut sender, 0, Some(&"x".repeat(16 * 1024 * 1024))).unwrap();
            let mut header = [0u8; REPLAY_FRAME_HEADER_BYTES];
            header[..8].copy_from_slice(&REPLAY_FRAME_MAGIC);
            header[8..12].copy_from_slice(&1u32.to_le_bytes());
            header[12..20].copy_from_slice(&(16 * 1024 * 1024 + 1u64).to_le_bytes());
            sender.write_all(&header).unwrap();
            // No excess payload follows: the length must be refused first.
        });
        let mut panes = vec![manifest_pane(1, false, None), manifest_pane(2, false, None)];
        let error =
            recv_replay_frames(&receiver, &mut panes).expect_err("aggregate must be bounded");
        writer.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("aggregate"), "{error}");
    }

    #[test]
    fn replay_restored_timeout_grows_with_the_replay_size() {
        assert_eq!(replay_restored_timeout(0), READY_TIMEOUT);
        assert!(replay_restored_timeout(256 * 1024 * 1024) > READY_TIMEOUT);
    }

    #[tokio::test]
    async fn handoff_styled_history_keeps_complete_newest_rows_within_budget() {
        let styles = [
            "\x1b[38;2;111;122;133m\x1b[48;2;144;155;166m\x1b[58;2;177;188;199m\x1b[4:3m",
            "\x1b[38;2;211;222;233m\x1b[48;2;244;155;166m\x1b[58;2;177;188;199m\x1b[4:2m",
        ];
        let mut input = String::new();
        for row in 1..=6_000 {
            let text = format!("ROW-{row:05}-{}", "x".repeat(110));
            for (column, character) in text.chars().enumerate() {
                input.push_str(styles[column % 2]);
                input.push(character);
            }
            if row != 6_000 {
                input.push_str("\r\n");
            }
        }
        let runtime = crate::pane::PaneRuntime::test_with_scrollback_bytes(
            120,
            24,
            10_000_000,
            input.as_bytes(),
        );
        drop(input);
        let full = runtime.handoff_history_ansi(usize::MAX).unwrap();
        assert!(
            full.len() > 40_000_000,
            "fixture must exceed the former ANSI estimate: {}",
            full.len()
        );
        let budget = replay_frame_budgets(&[full.len()])[0];
        let (frame, dropped) = budget_replay_frame(full.clone(), budget).unwrap();
        assert!(frame.len() <= 32 * 1024 * 1024);
        assert!(dropped > 0);
        assert_eq!(frame.len() + dropped, full.len() + "\x1b[0m".len());
        assert!(
            full.as_bytes()[dropped - 1] == b'\n',
            "cut must follow a complete row"
        );
        assert_eq!(frame.strip_prefix("\x1b[0m").unwrap(), &full[dropped..]);
        assert_eq!(
            budget_replay_frame(full, budget).unwrap().0,
            frame,
            "cut must be deterministic"
        );

        let restored = crate::pane::PaneRuntime::test_with_scrollback_bytes(
            120,
            24,
            10_000_000,
            frame.as_bytes(),
        );
        let text = restored.recent_unwrapped_text_snapshot(usize::MAX).text;
        let rows: Vec<_> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let first = rows.first().unwrap()[4..9].parse::<usize>().unwrap();
        assert!(first > 1);
        assert_eq!(rows.len(), 6_000 - first + 1);
        for (offset, row) in rows.iter().enumerate() {
            assert_eq!(
                *row,
                format!("ROW-{:05}-{}", first + offset, "x".repeat(110))
            );
        }
        assert_eq!(
            restored.recent_unwrapped_ansi(1),
            runtime.recent_unwrapped_ansi(1),
            "newest row must retain its ANSI styles"
        );
        eprintln!("styled handoff measurement: retained_bytes={} dropped_bytes={dropped} first_row={first} rows={}", frame.len(), rows.len());
    }

    /// The importer side, played by receive() against a scripted exporter:
    /// a manifest without the extension gets the plain legacy ack and no
    /// frames follow; a manifest with it gets the frames ack. Zero panes
    /// keep both flows inside the socket exchange.
    fn receive_against_exporter(tag: &str, extensions: &[&str]) -> (String, HandoffManifest) {
        let socket_path = test_socket_path(tag);
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let manifest = {
            let mut manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
            manifest.extensions = extensions
                .iter()
                .map(|extension| extension.to_string())
                .collect();
            serde_json::to_string(&manifest).expect("manifest should serialize")
        };
        let exporter = {
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("exporter should accept");
                let token_line = read_line_unbuffered(&mut stream).expect("token line");
                assert_eq!(token_line.trim_end(), "test-token");
                stream.write_all(manifest.as_bytes()).unwrap();
                stream.write_all(b"\n").unwrap();
                stream.flush().unwrap();
                let ack = read_line_unbuffered(&mut stream).expect("ack line");
                ack.trim_end().to_string()
            })
        };

        let received = receive(&socket_path, "test-token").expect("receive should succeed");
        let ack = exporter.join().expect("exporter thread should finish");
        let _ = std::fs::remove_file(&socket_path);
        (ack, received.manifest)
    }

    #[test]
    fn an_importer_answers_a_manifest_without_extensions_with_the_legacy_ack() {
        let (ack, _) = receive_against_exporter("plain", &[]);
        assert_eq!(ack, "validated");
    }

    #[test]
    fn an_importer_answers_a_manifest_with_replay_frames_with_the_frames_ack() {
        let (ack, _) = receive_against_exporter("frames", &[EXTENSION_REPLAY_FRAMES]);
        assert_eq!(ack, VALIDATED_REPLAY_FRAMES);
    }
}
