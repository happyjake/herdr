//! Platform-specific process and filesystem operations.
//!
//! Centralizes OS-dependent behavior behind a clean boundary so core
//! modules don't scatter `#[cfg]` branches through product logic.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: u32,
    pub processes: Vec<ForegroundProcess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

pub(crate) fn detached_custom_command_process(command: &str) -> std::process::Command {
    let mut process = detached_custom_command_process_platform(command);
    configure_background_command(&mut process);
    process
}

pub(crate) fn pane_custom_command_pty_builder(command: &str) -> portable_pty::CommandBuilder {
    pane_custom_command_pty_builder_platform(command)
}

pub(crate) fn apply_pane_runtime_marker(command: &mut portable_pty::CommandBuilder) {
    apply_pane_runtime_marker_platform(command);
}

#[cfg(not(windows))]
pub(crate) fn terminal_title_for_presentation(title: &str) -> &str {
    title
}

#[cfg(not(windows))]
fn apply_pane_runtime_marker_platform(_command: &mut portable_pty::CommandBuilder) {}

pub(crate) fn configure_background_command(command: &mut std::process::Command) {
    configure_background_command_platform(command);
}

#[cfg(not(windows))]
fn configure_background_command_platform(_command: &mut std::process::Command) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformCapabilities {
    pub(crate) live_handoff: bool,
    pub(crate) direct_terminal_attach: bool,
    pub(crate) preserve_legacy_doubled_escape_input: bool,
}

pub(crate) const fn capabilities() -> PlatformCapabilities {
    PlatformCapabilities {
        live_handoff: cfg!(unix),
        direct_terminal_attach: cfg!(unix),
        preserve_legacy_doubled_escape_input: cfg!(target_os = "macos"),
    }
}

#[cfg(not(windows))]
pub fn launch_server_daemon_command(command: &mut std::process::Command) -> std::io::Result<u32> {
    command.spawn().map(|child| child.id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutableFileIdentity {
    volume: u64,
    file: u64,
}

impl ExecutableFileIdentity {
    const fn new(volume: u64, file: u64) -> Self {
        Self { volume, file }
    }
}

pub(crate) fn executable_file_identity(
    path: &std::path::Path,
) -> std::io::Result<ExecutableFileIdentity> {
    executable_file_identity_platform(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn current_process_is_detached_server_daemon() -> bool {
    unsafe { libc::getsid(0) == libc::getpid() }
}

/// Raised by the SIGWINCH handler, consumed by the host resize watcher.
#[cfg(unix)]
static TERMINAL_RESIZE_SIGNALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn record_terminal_resize_signal(_signal: libc::c_int) {
    TERMINAL_RESIZE_SIGNALLED.store(true, std::sync::atomic::Ordering::Release);
}

/// Records SIGWINCH events that size polling can miss.
#[cfg(unix)]
pub(crate) fn watch_terminal_resize_signal() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction =
        record_terminal_resize_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // Keep blocking stdin and socket reads from failing with EINTR.
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
pub(crate) fn watch_terminal_resize_signal() {}

/// Returns whether a terminal size change was signalled since the last call.
#[cfg(unix)]
pub(crate) fn take_terminal_resize_signal() -> bool {
    TERMINAL_RESIZE_SIGNALLED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Windows relies on size polling.
#[cfg(not(unix))]
pub(crate) fn take_terminal_resize_signal() -> bool {
    false
}

/// The machine's hostname, used as the default server display name.
/// `None` when the OS reports no usable name.
#[cfg(unix)]
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let result = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if result != 0 {
        return None;
    }
    let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(windows)]
pub fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

pub(crate) fn read_limited_reader(
    mut reader: impl std::io::Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_len = remaining.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if bytes_read == 0 {
            return if bytes.is_empty() {
                Ok(LimitedRead::Empty)
            } else {
                Ok(LimitedRead::Complete(bytes))
            };
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let mut sentinel = [0_u8; 1];
    loop {
        return match reader.read(&mut sentinel) {
            Ok(0) if bytes.is_empty() => Ok(LimitedRead::Empty),
            Ok(0) => Ok(LimitedRead::Complete(bytes)),
            Ok(_) => Ok(LimitedRead::Oversized),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

/// Bytes still writable by this user on the volume holding `path`, which
/// must already exist. Used to refuse an upload before its first byte
/// rather than after filling the volume it lands on.
#[cfg(unix)]
// The block-count and block-size fields are 32 bits wide on some Unixes and
// 64 on others, so the widening casts below are load-bearing on one target
// and redundant on the next.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn available_bytes_on_volume(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains an interior NUL byte",
        )
    })?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(raw.as_ptr(), &mut stats) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Block counts are expressed in fragments; f_bsize is the documented
    // stand-in when a filesystem reports no fragment size.
    let block_bytes = if stats.f_frsize > 0 {
        stats.f_frsize as u64
    } else {
        stats.f_bsize as u64
    };
    Ok((stats.f_bavail as u64).saturating_mul(block_bytes))
}

#[cfg(windows)]
pub(crate) fn available_bytes_on_volume(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains an interior NUL byte",
        ));
    }
    wide.push(0);

    let mut available: u64 = 0;
    let queried = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if queried == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(available)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn available_bytes_on_volume(_path: &std::path::Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform reports no free-space figure",
    ))
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteSshConfigPaths {
    pub(crate) user_config: Option<std::path::PathBuf>,
    pub(crate) system_config: Option<std::path::PathBuf>,
    pub(crate) multiplexing: bool,
}

#[cfg(unix)]
mod unix_common;
#[cfg(unix)]
pub(crate) use unix_common::{begin_cli_output, end_cli_output};

#[cfg(not(unix))]
pub(crate) fn begin_cli_output() {}

#[cfg(not(unix))]
pub(crate) fn end_cli_output() {}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn available_pane_shell_from_job(child_pid: u32, job: ForegroundJob) -> Option<String> {
    if job.process_group_id != child_pid
        || job.processes.iter().any(|process| process.pid != child_pid)
    {
        return None;
    }
    job.processes
        .into_iter()
        .find(|process| process.pid == child_pid)
        .map(|process| process.name)
        .filter(|name| is_pane_shell_process_name(name))
}

fn normalized_process_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn is_powershell_process_name(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "pwsh" | "powershell"
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn interactive_unix_shell_command(
    argv: &[String],
    shell_name: &str,
    quote_posix_arg: fn(&str) -> String,
) -> Option<String> {
    let quote = if is_powershell_process_name(shell_name) {
        quote_powershell_arg
    } else {
        quote_posix_arg
    };
    let mut parts = argv.iter();
    let mut command = quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&quote(part));
    }
    Some(command)
}

pub(crate) fn quote_powershell_arg(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'=')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = normalized_process_name(name);
    matches!(
        normalized.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "elvish"
            | "xonsh"
            | "nu"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

/// Cwd of the pane's effective shell: the direct child, or the nested
/// shell the user is actually driving. A `cd` typed in a nested shell
/// never moves the direct child, so probing only `child_pid` would serve
/// the spawn directory forever. The driven shell is found on the
/// foreground ancestry — the ppid chain from the PTY's foreground
/// process-group leader up to the child — never by scanning the process
/// table: background jobs and an agent's own tool shells sit outside
/// that chain, and the bounded climb stays cheap on hot paths (snapshot
/// assembly and label rendering call this per pane).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn pane_shell_cwd(
    child_pid: u32,
    foreground_process_group: Option<u32>,
) -> Option<std::path::PathBuf> {
    if child_pid == 0 {
        return process_cwd(child_pid);
    }
    // A pane whose direct child is not a shell (an agent launched as the
    // pane command) keeps plain child probing — its tool shells must not
    // be mistaken for a user-driven shell.
    let Some((_, child_name)) = process_parent_and_name(child_pid) else {
        return process_cwd(child_pid);
    };
    if !is_pane_shell_process_name(&child_name) {
        return process_cwd(child_pid);
    }
    let Some(mut current) = foreground_process_group.and_then(live_foreground_group_member) else {
        return process_cwd(child_pid);
    };
    for _ in 0..MAX_FOREGROUND_ANCESTRY {
        if current == child_pid {
            break;
        }
        let Some((ppid, name)) = process_parent_and_name(current) else {
            break;
        };
        if is_pane_shell_process_name(&name) {
            // First shell met climbing from the foreground is the one the
            // user is driving.
            if let Some(cwd) = process_cwd(current) {
                return Some(cwd);
            }
            break;
        }
        if ppid <= 1 {
            break;
        }
        current = ppid;
    }
    process_cwd(child_pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pane_shell_cwd(
    child_pid: u32,
    _foreground_process_group: Option<u32>,
) -> Option<std::path::PathBuf> {
    process_cwd(child_pid)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_FOREGROUND_ANCESTRY: usize = 16;

/// A process group outlives its leader (a pipeline whose first command
/// exited still holds the terminal), so the group id is not always a live
/// pid. Leader first — the cheap, common case — then any live member.
/// Member enumeration scans the machine, and a dead-leader pipeline can
/// hold the terminal for hours of repeated cwd calls, so the member that
/// answered is remembered per group and revalidated with one getpgid per
/// call — membership is checked, never trusted, so a died member costs
/// exactly one fresh enumeration. (A recycled pid landing in the same
/// group could fool the check; that coincidence only risks reading a
/// wrong cwd until the next call.)
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn live_foreground_group_member(foreground_process_group: u32) -> Option<u32> {
    if process_parent_and_name(foreground_process_group).is_some() {
        return Some(foreground_process_group);
    }
    let cache = dead_leader_member_cache();
    if let Ok(mut members) = cache.lock() {
        if let Some(&member) = members.get(&foreground_process_group) {
            let pgid = unsafe { libc::getpgid(member as libc::pid_t) };
            if pgid == foreground_process_group as libc::pid_t {
                return Some(member);
            }
            members.remove(&foreground_process_group);
        }
    }
    let found = process_group_member_pids(foreground_process_group)
        .into_iter()
        .find(|pid| process_parent_and_name(*pid).is_some());
    if let Ok(mut members) = cache.lock() {
        if members.len() > 64 {
            members.clear();
        }
        match found {
            Some(member) => {
                members.insert(foreground_process_group, member);
            }
            None => {
                members.remove(&foreground_process_group);
            }
        }
    }
    found
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn dead_leader_member_cache() -> &'static std::sync::Mutex<std::collections::HashMap<u32, u32>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, u32>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_agent_hint(_pid: u32) -> Option<crate::detect::Agent> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn parse_agent_env_hint(environ: &[u8]) -> Option<crate::detect::Agent> {
    for record in environ.split(|&byte| byte == 0) {
        let Some(value) = record.strip_prefix(b"HERDR_AGENT=") else {
            continue;
        };
        return crate::detect::parse_agent_label(std::str::from_utf8(value).ok()?);
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
pub(crate) struct InputSourceRestore;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn pump_input_source_runloop() {}

/// Switches the host keyboard input source while prefix mode is active.
///
/// `App` drives this through a trait so the prefix-mode transitions can be
/// tested with a fake, without touching the real macOS APIs or leaking a
/// platform-specific restore type into `App`.
pub(crate) trait PrefixInputSource {
    /// Switch to an ASCII-capable input source for prefix commands. No-op if
    /// the current source is already ASCII-capable, the platform is
    /// unsupported, or the switch fails. Calling it again before `restore`
    /// keeps the source saved by the first call.
    fn switch_to_ascii(&mut self);

    /// Restore whatever `switch_to_ascii` saved. No-op if nothing was switched.
    fn restore(&mut self);
}

/// Production [`PrefixInputSource`] backed by the per-platform API.
#[derive(Default)]
pub(crate) struct RealPrefixInputSource {
    restore: Option<InputSourceRestore>,
}

impl PrefixInputSource for RealPrefixInputSource {
    fn switch_to_ascii(&mut self) {
        if self.restore.is_none() {
            // Drain pending input-source-change notifications so the read below is fresh (see
            // `pump_input_source_runloop`); a no-op on non-macOS.
            pump_input_source_runloop();
            self.restore = switch_to_ascii_input_source();
        }
    }

    fn restore(&mut self) {
        let _ = self.restore.take();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn terminal_resize_signal_is_recorded_once_per_delivery() {
        watch_terminal_resize_signal();
        assert!(!take_terminal_resize_signal());

        unsafe {
            libc::raise(libc::SIGWINCH);
        }

        assert!(take_terminal_resize_signal());
        assert!(!take_terminal_resize_signal());
    }

    #[test]
    fn pane_shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish", "pwsh", "powershell.exe"] {
            assert!(is_pane_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_pane_shell_process_name(program), "{program}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unique_cd_target(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-pane-shell-cwd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    // Fixtures run in their own process group and group-kill it on Drop,
    // so a panicking assertion cannot leak nested shells or sleeps
    // (nextest reports leaked descendants as leaky tests).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct Fixture {
        child: Option<std::process::Child>,
        pid: u32,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Fixture {
        fn pid(&self) -> u32 {
            self.pid
        }

        /// Reap the root process (the group leader) while keeping the
        /// group-kill on Drop — for tests that need a dead leader.
        fn reap_root(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn spawn_fixture(program: &str, args: &[&str], cwd: Option<&std::path::Path>) -> Fixture {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new(program);
        cmd.args(args).process_group(0);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let spawned = cmd.spawn().unwrap();
        let pid = spawned.id();
        Fixture {
            child: Some(spawned),
            pid,
        }
    }

    // Matcher-based on purpose: macOS /bin/sh reports kernel comm "bash",
    // so exact-name matching never finds it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn wait_for_descendant(parent: u32, matches: fn(&str) -> bool) -> Option<u32> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            for pid in session_processes(parent) {
                if pid == parent {
                    continue;
                }
                if let Some((ppid, comm)) = process_parent_and_name(pid) {
                    if ppid == parent && matches(&comm) {
                        return Some(pid);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        None
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn poll_pane_shell_cwd_until(
        pid: u32,
        foreground: Option<u32>,
        expect: &std::path::Path,
    ) -> Option<std::path::PathBuf> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        let mut last = None;
        while std::time::Instant::now() < deadline {
            last = pane_shell_cwd(pid, foreground);
            if last.as_deref() == Some(expect) {
                return last;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        last
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pane_shell_cwd_follows_a_cd_in_the_direct_child_shell() {
        let target = unique_cd_target("direct");
        let child = spawn_fixture(
            "/bin/sh",
            &["-c", &format!("cd \"{}\" && sleep 30; :", target.display())],
            None,
        );
        let seen = poll_pane_shell_cwd_until(child.pid(), Some(child.pid()), &target);
        drop(child);
        let _ = std::fs::remove_dir_all(&target);
        assert_eq!(seen.as_deref(), Some(target.as_path()));
    }

    // The shape behind stale pane cwds: the pane's direct child stays where
    // it spawned while the user cd's inside a nested shell launched from it
    // and then runs a program there. The foreground process is that
    // program; the driven shell is its parent.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pane_shell_cwd_follows_a_cd_in_a_nested_shell() {
        let target = unique_cd_target("nested");
        let child = spawn_fixture(
            "/bin/sh",
            &[
                "-c",
                &format!("/bin/sh -c 'cd \"{}\" && sleep 30; :'; :", target.display()),
            ],
            None,
        );
        let inner = wait_for_descendant(child.pid(), is_pane_shell_process_name)
            .expect("nested shell appears");
        let foreground =
            wait_for_descendant(inner, |comm| comm == "sleep").expect("program appears");
        let seen = poll_pane_shell_cwd_until(child.pid(), Some(foreground), &target);
        drop(child);
        let _ = std::fs::remove_dir_all(&target);
        assert_eq!(seen.as_deref(), Some(target.as_path()));
    }

    // A pane whose direct child is not a shell (an agent launched as the
    // pane command) must keep plain child probing: the agent's own tool
    // shell working elsewhere is not a user-driven shell.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pane_shell_cwd_ignores_tool_shells_under_a_non_shell_child() {
        let target = unique_cd_target("toolshell");
        let child = spawn_fixture(
            "/usr/bin/find",
            &[
                ".",
                "-maxdepth",
                "0",
                "-exec",
                "/bin/sh",
                "-c",
                &format!("cd \"{}\" && sleep 30; :", target.display()),
                ";",
            ],
            None,
        );
        let tool_shell = wait_for_descendant(child.pid(), is_pane_shell_process_name)
            .expect("tool shell appears");
        // Give the tool shell time to reach the target before asserting the
        // guard holds anyway.
        let _ = poll_pane_shell_cwd_until(tool_shell, Some(tool_shell), &target);
        let seen = pane_shell_cwd(child.pid(), Some(tool_shell));
        let direct = process_cwd(child.pid());
        drop(child);
        let _ = std::fs::remove_dir_all(&target);
        assert_ne!(seen.as_deref(), Some(target.as_path()));
        assert_eq!(seen, direct);
    }

    // A backgrounded shell job is not the driven shell: while the user sits
    // at the pane shell's prompt, a `(cd elsewhere; ...) &` child must not
    // hijack the served cwd.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pane_shell_cwd_ignores_background_shell_jobs() {
        let home = unique_cd_target("bgjob-home");
        let target = unique_cd_target("bgjob-target");
        let child = spawn_fixture(
            "/bin/sh",
            &[
                "-c",
                &format!(
                    "/bin/sh -c 'cd \"{}\" && sleep 30; :' & sleep 30; :",
                    target.display()
                ),
            ],
            Some(&home),
        );
        let background = wait_for_descendant(child.pid(), is_pane_shell_process_name)
            .expect("background job appears");
        let _ = poll_pane_shell_cwd_until(background, Some(background), &target);
        let seen = pane_shell_cwd(child.pid(), Some(child.pid()));
        drop(child);
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(seen.as_deref(), Some(home.as_path()));
    }

    // A pipeline whose first command exited leaves a foreground group
    // whose leader pid is gone while members still hold the terminal.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn live_foreground_group_member_survives_a_dead_leader() {
        let mut fixture = spawn_fixture("/bin/sh", &["-c", "sleep 30 & :"], None);
        let leader = fixture.pid();
        fixture.reap_root();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while process_parent_and_name(leader).is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            process_parent_and_name(leader).is_none(),
            "group leader must be gone"
        );
        let member = live_foreground_group_member(leader).expect("a live member is found");
        assert_ne!(member, leader);
        assert_eq!(
            unsafe { libc::getpgid(member as libc::pid_t) },
            leader as libc::pid_t
        );
        // The remembered member is revalidated, never trusted: once it dies
        // the resolver must not serve it again.
        unsafe {
            libc::kill(member as libc::pid_t, libc::SIGKILL);
        }
        let gone = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while process_parent_and_name(member).is_some() && std::time::Instant::now() < gone {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            process_parent_and_name(member).is_none(),
            "member must be gone"
        );
        assert_ne!(live_foreground_group_member(leader), Some(member));
    }

    #[test]
    fn detached_custom_command_preserves_unix_login_shell_flag() {
        let cmd = detached_custom_command_process("echo hello");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-lc"),
                std::ffi::OsStr::new("echo hello")
            ]
        );
    }

    #[test]
    fn pane_custom_command_builder_preserves_unix_shell_flag() {
        let expected: Vec<std::ffi::OsString> =
            vec!["/bin/sh".into(), "-c".into(), "echo hello".into()];
        assert_eq!(
            pane_custom_command_pty_builder("echo hello").get_argv(),
            &expected
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_accepts_known_agents() {
        assert_eq!(
            parse_agent_env_hint(b"PATH=/bin\0HERDR_AGENT=claude\0TERM=xterm\0"),
            Some(crate::detect::Agent::Claude)
        );
        assert_eq!(
            parse_agent_env_hint(b"HERDR_AGENT=codex"),
            Some(crate::detect::Agent::Codex)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_ignores_missing_or_unknown_agents() {
        assert_eq!(parse_agent_env_hint(b"PATH=/bin\0TERM=xterm\0"), None);
        assert_eq!(parse_agent_env_hint(b"HERDR_AGENT=not-an-agent\0"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn interactive_shell_command_quotes_for_posix_and_powershell() {
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "a'b".into(),
            "$HOME".into(),
            "semi;colon".into(),
            "@options".into(),
        ];
        assert_eq!(
            interactive_shell_command(&argv, "bash").as_deref(),
            Some("pi '' 'two words' 'a'\\''b' '$HOME' 'semi;colon' @options")
        );
        assert_eq!(
            interactive_shell_command(&argv, "pwsh").as_deref(),
            Some("pi '' 'two words' 'a''b' '$HOME' 'semi;colon' '@options'")
        );
    }

    #[test]
    fn read_limited_reader_returns_complete_data_under_limit() {
        let input = std::io::Cursor::new(b"image".to_vec());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_returns_empty_for_empty_input() {
        let input = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Empty
        );
    }

    #[test]
    fn read_limited_reader_accepts_data_exactly_at_limit() {
        let input = std::io::Cursor::new(b"four".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Complete(b"four".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_rejects_data_over_limit() {
        let input = std::io::Cursor::new(b"oversized".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Oversized
        );
    }

    #[test]
    fn read_limited_reader_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: std::io::Cursor<Vec<u8>>,
        }

        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.inner.read(buffer)
            }
        }

        let input = InterruptedOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"image".to_vec()),
        };
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }
}
