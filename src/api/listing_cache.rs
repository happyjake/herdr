//! A model list read by running an agent's own command, held between asks.
//!
//! Listing runs a program, which takes from a fraction of a second to
//! several seconds. So the server holds the last list it read and lists
//! again in the background once that list is stale, or once the settings it
//! was read under changed. An answer waits for a listing only while it holds
//! no list read under the current settings, and then only briefly.

use std::io::Read;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A listing older than this is read again on the next ask.
pub(super) const FRESH_FOR: Duration = Duration::from_secs(60 * 60);
/// After a listing that read nothing, the next is tried only this much
/// later, unless the settings change first.
pub(super) const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long one listing command gets before it is killed.
pub(super) const LIST_DEADLINE: Duration = Duration::from_secs(30);
/// How long an answer waits for a listing in flight while it holds no list
/// read under the current settings.
pub(super) const FIRST_WAIT: Duration = Duration::from_secs(1);
/// A listing is a few dozen short lines; more than this is not a listing.
const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

/// Reads the list, or None when the command is missing, fails, runs past
/// its deadline, or lists nothing.
pub(super) type Lister<T> = dyn Fn() -> Option<Vec<T>> + Send + Sync;

/// The held list of `T`. `K` identifies the settings a listing was read
/// under: a list read under other settings is stale at once.
pub(super) struct ListingCache<T, K> {
    thread_name: &'static str,
    lister: Box<Lister<T>>,
    state: Mutex<State<T, K>>,
    listed: Condvar,
}

struct State<T, K> {
    /// The last list read, and the settings it was read under.
    held: Option<(K, Vec<T>)>,
    /// When the last listing ended, whether it read a list, and the settings
    /// it ran under.
    last: Option<(Instant, bool, K)>,
    /// The settings the listing in flight runs under.
    listing: Option<K>,
}

impl<T, K> ListingCache<T, K>
where
    T: Clone + Send + 'static,
    K: Clone + PartialEq + Send + 'static,
{
    pub(super) fn new(thread_name: &'static str, lister: Box<Lister<T>>) -> Arc<Self> {
        Arc::new(Self {
            thread_name,
            lister,
            state: Mutex::new(State {
                held: None,
                last: None,
                listing: None,
            }),
            listed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State<T, K>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start a listing unless one is in flight, or the last one ran under
    /// these settings and is recent enough.
    pub(super) fn refresh(self: &Arc<Self>, now: Instant, key: &K) {
        let mut state = self.lock();
        let due = state.listing.is_none()
            && match &state.last {
                None => true,
                Some((_, _, last_key)) if last_key != key => true,
                Some((at, true, _)) => now.saturating_duration_since(*at) >= FRESH_FOR,
                Some((at, false, _)) => now.saturating_duration_since(*at) >= RETRY_AFTER,
            };
        if !due {
            return;
        }
        state.listing = Some(key.clone());
        drop(state);
        let cache = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(self.thread_name.into())
            .spawn(move || {
                let listed = (cache.lister)();
                cache.finish(listed);
            });
        if spawned.is_err() {
            self.finish(None);
        }
    }

    fn finish(&self, listed: Option<Vec<T>>) {
        let mut state = self.lock();
        if let Some(key) = state.listing.take() {
            state.last = Some((Instant::now(), listed.is_some(), key.clone()));
            if let Some(list) = listed {
                state.held = Some((key, list));
            }
        }
        self.listed.notify_all();
    }

    /// The held list, or None while no listing has read one. Starts a
    /// background listing when the list is stale; waits up to `wait` for
    /// one in flight only while no list read under `key` is held.
    pub(super) fn get(self: &Arc<Self>, now: Instant, key: &K, wait: Duration) -> Option<Vec<T>> {
        self.refresh(now, key);
        let state = self.lock();
        let current = matches!(&state.held, Some((held_key, _)) if held_key == key);
        if current || state.listing.is_none() {
            return state.held.as_ref().map(|(_, list)| list.clone());
        }
        let (state, _) = self
            .listed
            .wait_timeout_while(state, wait, |state| state.listing.is_some())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.held.as_ref().map(|(_, list)| list.clone())
    }
}

/// Run a command with no input, keep its stdout, and stop it at the
/// deadline. None when it cannot start, runs past the deadline, or writes
/// more than [MAX_OUTPUT_BYTES].
///
/// The command may be a shell whose startup files leave background jobs
/// that hold stdout open after the shell exits. So it runs in a session of
/// its own; its output is complete once it has exited and its stdout has
/// been quiet for [LINGER]; and what is left of its session then gets the
/// hangup a closed terminal sends. At the deadline the whole session is
/// killed.
pub(super) fn run_bounded(
    mut command: Command,
    deadline: Duration,
) -> Option<(ExitStatus, Vec<u8>)> {
    let started = Instant::now();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    start_own_session(&mut command);
    let mut child = command.spawn().ok()?;
    let Some(chunks) = child.stdout.take().and_then(read_in_background) else {
        kill(&mut child);
        return None;
    };
    let mut output = Vec::new();
    let mut open = true;
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        if started.elapsed() >= deadline || output.len() as u64 > MAX_OUTPUT_BYTES {
            kill(&mut child);
            return None;
        }
        if open {
            match chunks.recv_timeout(POLL) {
                Ok(chunk) => {
                    output.extend_from_slice(&chunk);
                    if let Some((_, quiet_since)) = &mut exited {
                        *quiet_since = Instant::now();
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => open = false,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else if exited.is_none() {
            std::thread::sleep(POLL);
        }
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some((status, Instant::now())),
                Ok(None) => {}
                Err(_) => {
                    kill(&mut child);
                    return None;
                }
            }
        }
        if let Some((status, quiet_since)) = exited {
            if !open || quiet_since.elapsed() >= LINGER {
                #[cfg(unix)]
                signal_session(&child, libc::SIGHUP);
                return Some((status, output));
            }
        }
    }
}

/// How often a running listing is checked.
const POLL: Duration = Duration::from_millis(20);
/// How long stdout may stay open and quiet after the command exits before
/// the output counts as complete.
const LINGER: Duration = Duration::from_millis(500);

/// Stdout in chunks as they arrive, read on a thread of its own so that a
/// command that stops writing without exiting still meets the deadline. The
/// channel closes at end of file.
fn read_in_background(mut stdout: ChildStdout) -> Option<mpsc::Receiver<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("model-listing-read".into())
        .spawn(move || {
            let mut chunk = [0; 8192];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(read) => {
                        if sender.send(chunk[..read].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })
        .ok()?;
    Some(receiver)
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    signal_session(child, libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

/// The command leads a new session, so its process group holds the
/// programs it starts, and it has no controlling terminal for an
/// interactive shell to take over.
#[cfg(unix)]
fn start_own_session(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Signal the process group the command leads. Harmless once the group is
/// empty.
#[cfg(unix)]
fn signal_session(child: &Child, signal: libc::c_int) {
    if let Ok(group) = libc::pid_t::try_from(child.id()) {
        // SAFETY: killpg reads only its arguments.
        unsafe {
            libc::killpg(group, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A lister that blocks until released, counting its runs.
    struct Gate {
        runs: AtomicUsize,
        release: Mutex<Option<Option<Vec<&'static str>>>>,
        released: Condvar,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                runs: AtomicUsize::new(0),
                release: Mutex::new(None),
                released: Condvar::new(),
            })
        }

        fn cache<K: Clone + PartialEq + Send + 'static>(
            self: &Arc<Self>,
        ) -> Arc<ListingCache<&'static str, K>> {
            let gate = Arc::clone(self);
            ListingCache::new(
                "test-listing",
                Box::new(move || {
                    gate.runs.fetch_add(1, Ordering::SeqCst);
                    let mut release = gate.release.lock().unwrap();
                    loop {
                        if let Some(answer) = release.take() {
                            return answer;
                        }
                        release = gate.released.wait(release).unwrap();
                    }
                }),
            )
        }

        fn answer(&self, answer: Option<Vec<&'static str>>) {
            *self.release.lock().unwrap() = Some(answer);
            self.released.notify_all();
        }

        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    fn settle<T, K>(cache: &ListingCache<T, K>)
    where
        T: Clone + Send + 'static,
        K: Clone + PartialEq + Send + 'static,
    {
        let state = cache.lock();
        let _ = cache
            .listed
            .wait_timeout_while(state, Duration::from_secs(5), |state| {
                state.listing.is_some()
            })
            .unwrap();
    }

    #[test]
    fn an_answer_waits_briefly_for_the_first_listing_and_never_runs_two_at_once() {
        let gate = Gate::new();
        let cache = gate.cache::<()>();
        let now = Instant::now();
        // Nothing held and the listing is slow: the answer gives up empty.
        assert_eq!(cache.get(now, &(), Duration::from_millis(20)), None);
        assert_eq!(cache.get(now, &(), Duration::ZERO), None);
        assert!(cache.lock().listing.is_some());

        // An answer that waits long enough gets the listing it waited for.
        let waiting = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.get(now, &(), Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(20));
        gate.answer(Some(vec!["a"]));
        assert_eq!(waiting.join().unwrap(), Some(vec!["a"]));
        assert_eq!(gate.runs(), 1);
    }

    #[test]
    fn a_held_list_is_answered_at_once_and_read_again_only_when_stale() {
        let gate = Gate::new();
        let cache = gate.cache::<()>();
        let now = Instant::now();
        gate.answer(Some(vec!["a"]));
        cache.refresh(now, &());
        settle(&cache);
        assert_eq!(gate.runs(), 1);

        // Fresh: answered from what is held, no second listing.
        assert_eq!(cache.get(now, &(), Duration::ZERO), Some(vec!["a"]));
        assert_eq!(gate.runs(), 1);

        // Stale: the held list is answered at once while a new listing runs.
        let later = Instant::now() + FRESH_FOR;
        assert_eq!(
            cache.get(later, &(), Duration::from_secs(5)),
            Some(vec!["a"])
        );
        assert!(cache.lock().listing.is_some());

        // A failed listing keeps the list it had.
        gate.answer(None);
        settle(&cache);
        assert_eq!(gate.runs(), 2);
        assert_eq!(cache.get(later, &(), Duration::ZERO), Some(vec!["a"]));
        assert_eq!(gate.runs(), 2);
    }

    #[test]
    fn a_failed_listing_is_tried_again_only_after_a_pause() {
        let gate = Gate::new();
        let cache = gate.cache::<()>();
        let now = Instant::now();
        gate.answer(None);
        cache.refresh(now, &());
        settle(&cache);
        assert_eq!(cache.get(Instant::now(), &(), Duration::ZERO), None);
        assert_eq!(gate.runs(), 1);

        gate.answer(Some(vec!["b"]));
        cache.refresh(Instant::now() + RETRY_AFTER, &());
        settle(&cache);
        assert_eq!(gate.runs(), 2);
        assert_eq!(
            cache.get(Instant::now(), &(), Duration::ZERO),
            Some(vec!["b"])
        );
    }

    #[test]
    fn changed_settings_list_again_at_once_and_the_answer_waits_for_it() {
        let gate = Gate::new();
        let cache = gate.cache::<u32>();
        let now = Instant::now();
        gate.answer(Some(vec!["old"]));
        cache.refresh(now, &1);
        settle(&cache);

        // Same settings, fresh list: nothing runs.
        assert_eq!(cache.get(now, &1, Duration::ZERO), Some(vec!["old"]));
        assert_eq!(gate.runs(), 1);

        // New settings: a listing starts at once and the answer waits for it.
        let waiting = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.get(now, &2, Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(20));
        gate.answer(Some(vec!["new"]));
        assert_eq!(waiting.join().unwrap(), Some(vec!["new"]));
        assert_eq!(gate.runs(), 2);

        // A listing that fails under new settings does not wait out the
        // pause before the next settings change lists again.
        gate.answer(None);
        assert_eq!(
            cache.get(now, &3, Duration::from_secs(5)),
            Some(vec!["new"])
        );
        assert_eq!(gate.runs(), 3);
        assert_eq!(cache.get(now, &3, Duration::ZERO), Some(vec!["new"]));
        assert_eq!(gate.runs(), 3);
        gate.answer(Some(vec!["newer"]));
        assert_eq!(
            cache.get(now, &4, Duration::from_secs(5)),
            Some(vec!["newer"])
        );
        assert_eq!(gate.runs(), 4);
    }

    #[test]
    fn an_answer_past_its_wait_gets_the_list_held_under_older_settings() {
        let gate = Gate::new();
        let cache = gate.cache::<u32>();
        let now = Instant::now();
        gate.answer(Some(vec!["old"]));
        cache.refresh(now, &1);
        settle(&cache);

        assert_eq!(
            cache.get(now, &2, Duration::from_millis(20)),
            Some(vec!["old"])
        );
        gate.answer(Some(vec!["new"]));
        settle(&cache);
        assert_eq!(cache.get(now, &2, Duration::ZERO), Some(vec!["new"]));
        assert_eq!(gate.runs(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_past_its_deadline_is_killed_and_reads_as_nothing() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'a\\tA\\n'; exec sleep 30"]);
        let started = Instant::now();
        assert!(run_bounded(command, Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_exits_gives_its_status_and_stdout_only() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'a\\tA\\n'; echo progress >&2"]);
        let (status, output) = run_bounded(command, Duration::from_secs(10)).unwrap();
        assert!(status.success());
        assert_eq!(output, b"a\tA\n");
    }

    #[cfg(unix)]
    fn scratch_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("listing-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Whether a process is gone, waiting up to two seconds for it to go.
    #[cfg(unix)]
    fn gone(pid_file: &std::path::Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        let pid: libc::pid_t = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        loop {
            // SAFETY: signal 0 only checks that the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_background_job_left_holding_stdout_does_not_hold_the_listing() {
        let pid_file = scratch_file("left-job.pid");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "sleep 30 & echo $! > \"$0\"; printf 'a\\tA\\n'",
            pid_file.to_str().unwrap(),
        ]);
        let started = Instant::now();
        let (status, output) = run_bounded(command, Duration::from_secs(20)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(status.success());
        assert_eq!(output, b"a\tA\n");
        assert!(gone(&pid_file), "the job left behind got no hangup");
    }

    #[cfg(unix)]
    #[test]
    fn a_command_past_its_deadline_is_killed_with_the_jobs_it_started() {
        let pid_file = scratch_file("deadline-job.pid");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "trap '' HUP; sleep 30 & echo $! > \"$0\"; wait",
            pid_file.to_str().unwrap(),
        ]);
        assert!(run_bounded(command, Duration::from_millis(500)).is_none());
        assert!(gone(&pid_file), "the job outlived the deadline");
    }
}
