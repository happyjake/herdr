//! agy's model list, for the agent settings answer.
//!
//! Only `agy models` knows which models agy offers, and it asks agy's
//! service over the network, which takes seconds. So the server holds the
//! last list it read and lists again in the background once that list is
//! stale. An answer waits for agy only while the server holds no list at
//! all, and then only briefly.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::schema::AgyModel;

/// A listing older than this is read again on the next ask.
const FRESH_FOR: Duration = Duration::from_secs(60 * 60);
/// After a listing that read nothing, the next is tried only this much later.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long one `agy models` gets before it is killed.
const LIST_DEADLINE: Duration = Duration::from_secs(30);
/// How long an answer waits for a listing in flight while no list is held.
const FIRST_WAIT: Duration = Duration::from_secs(1);
/// The listing is a few dozen short lines; more than this is not a listing.
const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

/// Start reading agy's model list in the background, so that the first
/// settings answer after a server start already has it.
pub(crate) fn warm() {
    cache().refresh(Instant::now());
}

/// The held model list. Starts a background listing when the list is
/// stale; waits briefly for one in flight only while no list is held.
pub(super) fn models() -> Vec<AgyModel> {
    cache().models(Instant::now(), FIRST_WAIT)
}

fn cache() -> &'static Arc<ModelCache> {
    static CACHE: OnceLock<Arc<ModelCache>> = OnceLock::new();
    CACHE.get_or_init(|| ModelCache::new(Box::new(list_with_agy)))
}

type Lister = dyn Fn() -> Option<Vec<AgyModel>> + Send + Sync;

struct ModelCache {
    lister: Box<Lister>,
    state: Mutex<State>,
    listed: Condvar,
}

#[derive(Default)]
struct State {
    models: Vec<AgyModel>,
    /// When the last listing ended, and whether it read a list.
    last: Option<(Instant, bool)>,
    listing: bool,
}

impl ModelCache {
    fn new(lister: Box<Lister>) -> Arc<Self> {
        Arc::new(Self {
            lister,
            state: Mutex::new(State::default()),
            listed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start a listing unless one is in flight or the last one is recent
    /// enough.
    fn refresh(self: &Arc<Self>, now: Instant) {
        let mut state = self.lock();
        let due = !state.listing
            && match state.last {
                None => true,
                Some((at, true)) => now.saturating_duration_since(at) >= FRESH_FOR,
                Some((at, false)) => now.saturating_duration_since(at) >= RETRY_AFTER,
            };
        if !due {
            return;
        }
        state.listing = true;
        drop(state);
        let cache = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("agy-models".into())
            .spawn(move || {
                let listed = (cache.lister)();
                cache.finish(listed);
            });
        if spawned.is_err() {
            self.finish(None);
        }
    }

    fn finish(&self, listed: Option<Vec<AgyModel>>) {
        let mut state = self.lock();
        state.listing = false;
        state.last = Some((Instant::now(), listed.is_some()));
        if let Some(models) = listed {
            state.models = models;
        }
        self.listed.notify_all();
    }

    fn models(self: &Arc<Self>, now: Instant, wait: Duration) -> Vec<AgyModel> {
        self.refresh(now);
        let state = self.lock();
        if !state.models.is_empty() || !state.listing {
            return state.models.clone();
        }
        let (state, _) = self
            .listed
            .wait_timeout_while(state, wait, |state| state.listing)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.models.clone()
    }
}

/// Run `agy models`, or None when agy is missing, fails, runs past its
/// deadline, or lists nothing.
fn list_with_agy() -> Option<Vec<AgyModel>> {
    let mut command = Command::new("agy");
    command.arg("models");
    let (status, output) = run_bounded(command, LIST_DEADLINE)?;
    if !status.success() {
        return None;
    }
    let models = parse_models(&String::from_utf8_lossy(&output));
    (!models.is_empty()).then_some(models)
}

/// Run a command with no input, keep at most [MAX_OUTPUT_BYTES] of its
/// stdout, and kill it at the deadline.
fn run_bounded(mut command: Command, deadline: Duration) -> Option<(ExitStatus, Vec<u8>)> {
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    // A reader of its own, so a child that stops writing without exiting
    // still meets the deadline.
    let reader = std::thread::Builder::new()
        .name("agy-models-read".into())
        .spawn(move || {
            let mut output = Vec::new();
            let read = (&mut stdout)
                .take(MAX_OUTPUT_BYTES)
                .read_to_end(&mut output);
            let _ = sender.send(read.map(|_| output));
        });
    let output = match reader {
        Ok(_) => receiver
            .recv_timeout(deadline.saturating_sub(started.elapsed()))
            .ok()
            .and_then(Result::ok),
        Err(_) => None,
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return output.map(|output| (status, output)),
            Ok(None) if output.is_some() && started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// One model per `<id>\t<name>` line, the first of any repeated id. Any
/// other line (a progress note, a blank) is not a model.
fn parse_models(text: &str) -> Vec<AgyModel> {
    let mut models: Vec<AgyModel> = Vec::new();
    for line in text.lines() {
        let Some((id, name)) = line.split_once('\t') else {
            continue;
        };
        let (id, name) = (id.trim(), name.trim());
        if id.is_empty()
            || name.is_empty()
            || id.contains(char::is_whitespace)
            || models.iter().any(|model| model.id == id)
        {
            continue;
        }
        models.push(AgyModel {
            id: id.to_string(),
            name: name.to_string(),
        });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn model(id: &str, name: &str) -> AgyModel {
        AgyModel {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn parse_reads_tab_separated_ids_and_names_in_order() {
        let text = "Fetching available models...\n\
            gemini-pro-orchestrator-argon\tOrchestrator (Argon)\n\
            \n\
            gemini-4-argon-high\tGemini 4 Argon (High)\r\n\
            gemini-pro-orchestrator-argon\tA repeat\n\
            spaced id\tNot a model\n\
            \tNo id\n\
            no-name\t\n";
        assert_eq!(
            parse_models(text),
            vec![
                model("gemini-pro-orchestrator-argon", "Orchestrator (Argon)"),
                model("gemini-4-argon-high", "Gemini 4 Argon (High)"),
            ]
        );
    }

    /// A lister that blocks until released, counting its runs.
    struct Gate {
        runs: AtomicUsize,
        release: Mutex<Option<Option<Vec<AgyModel>>>>,
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

        fn cache(self: &Arc<Self>) -> Arc<ModelCache> {
            let gate = Arc::clone(self);
            ModelCache::new(Box::new(move || {
                gate.runs.fetch_add(1, Ordering::SeqCst);
                let mut release = gate.release.lock().unwrap();
                loop {
                    if let Some(answer) = release.take() {
                        return answer;
                    }
                    release = gate.released.wait(release).unwrap();
                }
            }))
        }

        fn answer(&self, answer: Option<Vec<AgyModel>>) {
            *self.release.lock().unwrap() = Some(answer);
            self.released.notify_all();
        }
    }

    fn settle(cache: &ModelCache) {
        let state = cache.lock();
        let _ = cache
            .listed
            .wait_timeout_while(state, Duration::from_secs(5), |state| state.listing)
            .unwrap();
    }

    #[test]
    fn an_answer_waits_briefly_for_the_first_listing_and_never_runs_two_at_once() {
        let gate = Gate::new();
        let cache = gate.cache();
        let now = Instant::now();
        // Nothing held and the listing is slow: the answer gives up empty.
        assert!(cache.models(now, Duration::from_millis(20)).is_empty());
        assert!(cache.models(now, Duration::ZERO).is_empty());
        assert!(cache.lock().listing);

        // An answer that waits long enough gets the listing it waited for.
        let waiting = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.models(now, Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(20));
        gate.answer(Some(vec![model("a", "A")]));
        assert_eq!(waiting.join().unwrap(), vec![model("a", "A")]);
        assert_eq!(gate.runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_held_list_is_answered_at_once_and_read_again_only_when_stale() {
        let gate = Gate::new();
        let cache = gate.cache();
        let now = Instant::now();
        gate.answer(Some(vec![model("a", "A")]));
        cache.refresh(now);
        settle(&cache);
        assert_eq!(gate.runs.load(Ordering::SeqCst), 1);

        // Fresh: answered from what is held, no second listing.
        assert_eq!(cache.models(now, Duration::ZERO), vec![model("a", "A")]);
        assert_eq!(gate.runs.load(Ordering::SeqCst), 1);

        // Stale: the held list is answered while a new listing runs.
        let later = Instant::now() + FRESH_FOR;
        assert_eq!(
            cache.models(later, Duration::from_secs(5)),
            vec![model("a", "A")]
        );
        assert!(cache.lock().listing);

        // A failed listing keeps the list it had.
        gate.answer(None);
        settle(&cache);
        assert_eq!(gate.runs.load(Ordering::SeqCst), 2);
        assert_eq!(cache.models(later, Duration::ZERO), vec![model("a", "A")]);
        assert_eq!(gate.runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_listing_is_tried_again_only_after_a_pause() {
        let gate = Gate::new();
        let cache = gate.cache();
        let now = Instant::now();
        gate.answer(None);
        cache.refresh(now);
        settle(&cache);
        assert!(cache.models(Instant::now(), Duration::ZERO).is_empty());
        assert_eq!(gate.runs.load(Ordering::SeqCst), 1);

        gate.answer(Some(vec![model("b", "B")]));
        cache.refresh(Instant::now() + RETRY_AFTER);
        settle(&cache);
        assert_eq!(gate.runs.load(Ordering::SeqCst), 2);
        assert_eq!(
            cache.models(Instant::now(), Duration::ZERO),
            vec![model("b", "B")]
        );
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
    fn a_command_that_exits_gives_its_status_and_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'a\\tA\\n'; echo progress >&2"]);
        let (status, output) = run_bounded(command, Duration::from_secs(10)).unwrap();
        assert!(status.success());
        assert_eq!(
            parse_models(&String::from_utf8_lossy(&output)),
            vec![model("a", "A")]
        );
    }
}
