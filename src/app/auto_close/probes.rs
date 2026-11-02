//! Bounded detached probes. A stalled filesystem occupies a worker, never the
//! runtime loop; repeated requests share that worker instead of spawning more.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub terminal: String,
    pub pid: Option<u32>,
    pub input_seq: u64,
    pub status_at: u64,
    pub agent: Option<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct Request {
    pub key: Key,
    pub process: Option<crate::pane::PaneProcessProbe>,
}

#[derive(Clone)]
pub(super) struct Facts {
    pub main_checkout: Option<String>,
    pub bare_prompt: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Id {
    Pane(String),
    Skill(String),
}

enum Job {
    Pane(Request),
    Skill(String),
}

enum Answer {
    Pane(Key, Facts),
    Skill(bool),
}

impl Job {
    fn id(&self) -> Id {
        match self {
            Self::Pane(request) => Id::Pane(request.key.terminal.clone()),
            Self::Skill(path) => Id::Skill(path.clone()),
        }
    }

    fn run(&self) -> Answer {
        match self {
            Self::Pane(request) => {
                let (cwd, bare_prompt) =
                    request.process.as_ref().map_or((None, false), |process| {
                        process.inspect(request.key.agent.is_none())
                    });
                let cwd = cwd.unwrap_or_else(|| request.key.cwd.clone());
                let main_checkout = crate::workspace::git_space_metadata(&cwd)
                    .filter(|info| !info.is_linked_worktree)
                    .map(|info| info.checkout_key);
                Answer::Pane(
                    request.key.clone(),
                    Facts {
                        main_checkout,
                        bare_prompt,
                    },
                )
            }
            Self::Skill(path) => {
                let path = if let Some(relative) = path.strip_prefix("~/") {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
                } else {
                    Some(PathBuf::from(path))
                };
                Answer::Skill(path.is_some_and(|path| path.is_file()))
            }
        }
    }
}

#[derive(Default)]
struct State {
    jobs: VecDeque<Job>,
    pending: HashSet<Id>,
    ready: HashMap<Id, (Instant, Answer)>,
    workers: usize,
}

#[derive(Clone, Default)]
pub(super) struct Probes(Arc<Mutex<State>>);

impl Probes {
    pub(super) fn retain(&self, terminals: &HashSet<String>, skill: &str) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let live = |id: &Id| match id {
            Id::Pane(id) => terminals.contains(id),
            Id::Skill(path) => path == skill,
        };
        state.ready.retain(|id, _| live(id));
        state.jobs.retain(|job| live(&job.id()));
        state.pending.retain(live);
    }
    fn queue(&self, state: &mut State, job: Job) {
        if state.pending.insert(job.id()) {
            state.jobs.push_back(job);
        }
        if state.workers >= 4 || state.jobs.is_empty() {
            return;
        }
        state.workers += 1;
        let probes = self.clone();
        if let Err(error) =
            crate::thread_spawn::spawn_named("herdr-auto-close-probe", move || probes.run())
        {
            state.workers -= 1;
            tracing::warn!(%error, "failed to start auto-close probe");
        }
    }

    fn run(&self) {
        loop {
            let job = {
                let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
                let Some(job) = state.jobs.pop_front() else {
                    state.workers -= 1;
                    return;
                };
                job
            };
            let answer = job.run();
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.pending.remove(&job.id()) {
                state.ready.insert(job.id(), (Instant::now(), answer));
            }
        }
    }

    pub(super) fn pane(&self, request: &Request, after: Option<Instant>) -> Option<Facts> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((at, Answer::Pane(key, facts))) =
            state.ready.get(&Id::Pane(request.key.terminal.clone()))
        {
            if key == &request.key && after.is_none_or(|after| *at >= after) {
                return Some(facts.clone());
            }
        }
        self.queue(&mut state, Job::Pane(request.clone()));
        None
    }

    pub(super) async fn skill(&self, path: &str) -> bool {
        let after = Instant::now();
        loop {
            {
                let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
                if let Some((at, Answer::Skill(present))) = state.ready.get(&Id::Skill(path.into()))
                {
                    if *at >= after {
                        return *present;
                    }
                }
                self.queue(&mut state, Job::Skill(path.into()));
            }
            if after.elapsed() >= Duration::from_secs(5) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(super) fn refresh(&self, panes: &mut [super::PaneSnapshot], after: Instant) -> bool {
        let mut ready = true;
        for pane in panes {
            if let Some(request) = &pane.probe {
                if let Some(facts) = self.pane(request, Some(after)) {
                    pane.main_checkout = facts.main_checkout;
                    pane.bare_prompt = facts.bare_prompt;
                    pane.probe_ready = true;
                } else {
                    ready = false;
                    pane.probe_ready = false;
                }
            }
        }
        ready
    }

    pub(super) async fn resolve(
        &self,
        mut panes: Vec<super::PaneSnapshot>,
        after: Instant,
    ) -> Vec<super::PaneSnapshot> {
        let started = Instant::now();
        loop {
            if self.refresh(&mut panes, after) {
                return panes;
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return panes;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
