//! Serving `server.lookup_place`.
//!
//! The lookup reads dotfiles, walks session directories, and asks tmux what
//! it is showing. None of that belongs on the app thread, which is also
//! driving PTYs, rendering, and every other queued request, so the request
//! is answered here on the connection thread instead. The only thing the
//! app is asked for is a list of the directories its live panes stand in —
//! state it already holds in memory — through the `pane.list` it already
//! answers; the saved layout and every other source are read off the
//! connection's own blocking pool.
//!
//! One deadline covers the whole request — the wait for a turn, the ask to
//! the app, tmux, and the scan alike — and a request that runs out of time
//! anywhere under it answers an empty list, the same answer a desk with no
//! match gives. Inside that, tmux and the app each carry their own shorter
//! bound.
//!
//! Only two lookups run at a time per server process, and the turn is held
//! by the scan rather than by the request waiting on it. A blocking scan
//! cannot be cancelled, so a request that gives up leaves its scan running
//! and its turn taken until the scan is genuinely over; the next request
//! waits for a real vacancy instead of starting work beside it. The
//! blocking pool is sized to the same number, so even a mistake cannot
//! grow it.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use crate::api::schema::{
    Method, PaneListParams, PlaceInfo, Request, ResponseResult, ServerLookupPlaceParams,
    SuccessResponse,
};
use crate::api::ApiRequestSender;
use crate::place_lookup::{self, Desk, Place, MAX_PLACES};

/// How long a whole lookup may take before it answers with nothing.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(5);

/// Lookups allowed to run at once in this process.
const CONCURRENT_LOOKUPS: usize = 2;

/// How long the app gets to hand back its live pane directories. Shorter
/// than the whole request's deadline, so a silent app is the inner bound
/// rather than the thing that spends the request.
const LIVE_DIRS_TIMEOUT: Duration = Duration::from_secs(2);

/// Where one lookup's memory is to be read from.
///
/// Production fills this from the running server and the real home; a test
/// hands over a fixture, which is what keeps the tests off the machine's
/// own desk.
pub(super) struct Sources {
    /// Reads the directories the server's live panes stand in. Held as
    /// something still to do, not something already done, because asking
    /// the app blocks and every part of a lookup belongs inside the same
    /// deadline and the same turn.
    pub live: Box<dyn FnOnce() -> Vec<PathBuf> + Send>,
    /// Home directory carrying the dotfile sources.
    pub home: Option<PathBuf>,
    /// The saved layout to read, when there is one to read.
    pub persisted: Option<PathBuf>,
    /// Whether tmux is worth asking.
    pub consult_tmux: bool,
}

/// Answer `server.lookup_place` for one request.
pub(super) fn handle_lookup(
    request_id: String,
    params: &ServerLookupPlaceParams,
    api_tx: &ApiRequestSender,
) -> String {
    let (query, limit) = match read_params(params) {
        Ok(read) => read,
        Err(message) => {
            return crate::api::server::error_response_json(
                request_id,
                "invalid_params",
                message.into(),
            )
        }
    };

    let api_tx = api_tx.clone();
    let sources = Sources {
        live: Box::new(move || live_pane_dirs(&api_tx)),
        home: crate::worktree::home_dir(),
        persisted: Some(crate::persist::session_path()),
        consult_tmux: true,
    };

    respond(request_id, query, limit, sources)
}

/// Answer one already-read lookup from the sources it names.
fn respond(request_id: String, query: &str, limit: usize, sources: Sources) -> String {
    encode(
        request_id,
        blocking_lookup(query, limit, LOOKUP_DEADLINE, sources),
    )
}

fn encode(request_id: String, places: Vec<Place>) -> String {
    let places = places
        .into_iter()
        .map(|place| PlaceInfo {
            path: place.path,
            source: place.source,
        })
        .collect();
    serde_json::to_string(&SuccessResponse {
        id: request_id.clone(),
        result: ResponseResult::ServerLookupPlace { places },
    })
    .unwrap_or_else(|_| {
        crate::api::server::error_response_json(
            request_id,
            "internal_error",
            "failed to encode response".into(),
        )
    })
}

/// Read a lookup's query and limit, or say why they are no request at all.
///
/// The limit is capped rather than refused, so a client that asks for more
/// than the server will ever answer still gets an answer.
fn read_params(params: &ServerLookupPlaceParams) -> Result<(&str, usize), &'static str> {
    let query = params.query.trim();
    if query.is_empty() {
        return Err("query must not be empty");
    }
    let limit = match params.limit {
        None => MAX_PLACES,
        Some(0) => return Err("limit must be at least 1"),
        Some(limit) => (limit as usize).min(MAX_PLACES),
    };
    Ok((query, limit))
}

/// Ask the app where its live panes stand. This is the only part of a
/// lookup the app thread ever runs, and it reads state already in memory.
fn live_pane_dirs(api_tx: &ApiRequestSender) -> Vec<PathBuf> {
    let response = crate::api::server::dispatch_to_app_with_timeout(
        Request {
            id: "internal:place_lookup:panes".into(),
            method: Method::PaneList(PaneListParams { workspace_id: None }),
        },
        api_tx,
        Some(LIVE_DIRS_TIMEOUT),
    );
    let Ok(response) = serde_json::from_str::<SuccessResponse>(&response) else {
        return Vec::new();
    };
    let ResponseResult::PaneList { panes } = response.result else {
        return Vec::new();
    };
    panes
        .into_iter()
        .flat_map(|pane| [pane.cwd, pane.foreground_cwd])
        .flatten()
        .map(PathBuf::from)
        .collect()
}

/// The runtime the bounded part of a lookup runs on.
///
/// The API server's connections are plain threads started before the app's
/// runtime exists, so a lookup brings its own rather than borrowing one it
/// cannot be sure is there. Two workers is the same number as the
/// concurrency bound, and it is built once, on the first lookup a process
/// ever serves.
fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(CONCURRENT_LOOKUPS)
                // As many blocking threads as there are turns to be had, so
                // the pool cannot grow past the concurrency bound however
                // this file is later edited.
                .max_blocking_threads(CONCURRENT_LOOKUPS)
                .thread_name("herdr-place-lookup")
                .enable_all()
                .build()
                .ok()
        })
        .as_ref()
}

fn permits() -> &'static Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(Semaphore::new(CONCURRENT_LOOKUPS)))
}

/// Run one lookup to completion from a blocking thread, whole.
///
/// The deadline starts here and covers everything: waiting for a turn,
/// asking the app, asking tmux, and the scan.
pub(super) fn blocking_lookup(
    query: &str,
    limit: usize,
    deadline: Duration,
    sources: Sources,
) -> Vec<Place> {
    let Some(runtime) = runtime() else {
        return Vec::new();
    };
    runtime.block_on(within_deadline(
        deadline,
        lookup(query.to_string(), limit, sources),
    ))
}

/// Hold one lookup to its deadline. A desk that cannot answer in time has
/// nothing to say, which is the same answer a desk with no match gives.
async fn within_deadline(
    deadline: Duration,
    work: impl std::future::Future<Output = Vec<Place>>,
) -> Vec<Place> {
    tokio::time::timeout(deadline, work)
        .await
        .unwrap_or_default()
}

async fn lookup(query: String, limit: usize, sources: Sources) -> Vec<Place> {
    // A queue rather than a refusal: a caller that arrives third waits its
    // turn instead of adding to the load, and gives up when the deadline
    // above says so.
    let Ok(permit) = Arc::clone(permits()).acquire_owned().await else {
        return Vec::new();
    };
    let tmux = if sources.consult_tmux {
        place_lookup::tmux_pane_dirs().await
    } else {
        Vec::new()
    };
    scan(permit, query, limit, sources, tmux).await
}

/// The blocking half of a lookup: ask the app, read the saved layout and
/// the home sources, match, rank.
///
/// The turn travels into the closure rather than staying with the request
/// that started it. A blocking task cannot be cancelled, so a request that
/// gives up at its deadline leaves this scan running — and the place it
/// was counted in stays taken until the scan itself is over, which is what
/// keeps the number of scans in flight at the bound instead of the number
/// of requests still waiting.
async fn scan(
    permit: tokio::sync::OwnedSemaphorePermit,
    query: String,
    limit: usize,
    sources: Sources,
    tmux: Vec<PathBuf>,
) -> Vec<Place> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut workspaces = (sources.live)();
        if let Some(persisted) = sources.persisted.as_deref() {
            workspaces.extend(place_lookup::persisted_layout_dirs(persisted));
        }
        let desk = Desk {
            workspaces,
            home: sources.home,
            tmux,
        };
        place_lookup::lookup(&desk, &query, limit)
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentStatus, ErrorResponse, PaneInfo, PlaceSource};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    fn pane(cwd: &str, foreground_cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: "w1:p1".into(),
            terminal_id: "t1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            focused: true,
            cwd: Some(cwd.into()),
            foreground_cwd: foreground_cwd.map(str::to_string),
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Idle,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll: None,
            mouse_tracking: false,
            alternate_screen: false,
            agent_status_changed_at: None,
            pinned: false,
            revision: 0,
        }
    }

    /// A throwaway home holding real dotfiles and real directories.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "herdr-api-place-lookup-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("fixture home");
            Self { root }
        }

        fn dir(&self, relative: &str) -> String {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).expect("fixture directory");
            path.to_str().expect("utf-8 fixture path").to_string()
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture parent");
            }
            std::fs::write(path, contents).expect("fixture file");
        }

        /// Sources that name this fixture and nothing on the real desk.
        fn sources(&self, live: Vec<PathBuf>) -> Sources {
            self.sources_from(move || live)
        }

        /// The same, with the live directories read by a closure the test
        /// controls, so a stalled or counted scan can be staged.
        fn sources_from(&self, live: impl FnOnce() -> Vec<PathBuf> + Send + 'static) -> Sources {
            Sources {
                live: Box::new(live),
                home: Some(self.root.clone()),
                persisted: Some(self.root.join("session.json")),
                consult_tmux: false,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_blank_query_never_reaches_the_app() {
        // The receiver is dropped, so any dispatch to the app would fail:
        // a refused request must be refused before it costs the app
        // anything.
        let (tx, _) = mpsc::unbounded_channel();
        for query in ["", "   ", "\t\n"] {
            let response = handle_lookup(
                "req_blank".into(),
                &ServerLookupPlaceParams {
                    query: query.into(),
                    limit: None,
                },
                &tx,
            );
            let parsed: ErrorResponse = serde_json::from_str(&response)
                .unwrap_or_else(|_| panic!("expected an error response, got: {response}"));
            assert_eq!(parsed.error.code, "invalid_params");
        }

        let response = handle_lookup(
            "req_zero".into(),
            &ServerLookupPlaceParams {
                query: "rocket".into(),
                limit: Some(0),
            },
            &tx,
        );
        let parsed: ErrorResponse = serde_json::from_str(&response)
            .unwrap_or_else(|_| panic!("expected an error response, got: {response}"));
        assert_eq!(parsed.error.code, "invalid_params");
    }

    #[test]
    fn a_query_is_read_without_its_surrounding_space() {
        let params = ServerLookupPlaceParams {
            query: "  rocket \n".into(),
            limit: None,
        };
        assert_eq!(read_params(&params), Ok(("rocket", MAX_PLACES)));
    }

    #[test]
    fn a_limit_of_zero_is_refused_and_a_large_one_is_capped() {
        let refused = ServerLookupPlaceParams {
            query: "rocket".into(),
            limit: Some(0),
        };
        assert_eq!(read_params(&refused), Err("limit must be at least 1"));

        let capped = ServerLookupPlaceParams {
            query: "rocket".into(),
            limit: Some(500),
        };
        assert_eq!(read_params(&capped), Ok(("rocket", MAX_PLACES)));

        let smaller = ServerLookupPlaceParams {
            query: "rocket".into(),
            limit: Some(3),
        };
        assert_eq!(read_params(&smaller), Ok(("rocket", 3)));
    }

    #[test]
    fn the_app_is_asked_for_its_live_pane_directories_and_nothing_else() {
        let (tx, mut rx) = mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        // Directories that do not exist: the app's part of a lookup reads
        // state it already holds, so it must hand these back untouched
        // rather than stat anything.
        let responder = std::thread::spawn(move || {
            let mut methods = Vec::new();
            while let Some(message) = rx.blocking_recv() {
                methods.push(crate::api::server::api_method_name_for_test(
                    &message.request.method,
                ));
                let response = serde_json::to_string(&SuccessResponse {
                    id: message.request.id.clone(),
                    result: ResponseResult::PaneList {
                        panes: vec![
                            pane("/no/such/place/rocket", Some("/no/such/place/rocket-app")),
                            pane("/no/such/place/other", None),
                        ],
                    },
                })
                .unwrap();
                let _ = message.respond_to.send(response);
            }
            methods
        });

        let dirs = live_pane_dirs(&tx.clone());
        drop(tx);
        let methods = responder.join().expect("responder");

        assert_eq!(methods, vec!["pane.list"]);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/no/such/place/rocket"),
                PathBuf::from("/no/such/place/rocket-app"),
                PathBuf::from("/no/such/place/other"),
            ]
        );
    }

    #[test]
    fn a_lookup_answers_from_the_sources_it_was_handed() {
        let fixture = Fixture::new();
        let live = fixture.dir("code/zephyr-live");
        let saved = fixture.dir("code/zephyr-saved");
        let pane_dir = fixture.dir("code/zephyr-pane");
        let z = fixture.dir("code/zephyr-z");
        fixture.write(".z", &format!("{z}|40|1700000000\n"));
        fixture.write(
            "session.json",
            &serde_json::json!({
                "version": 3,
                "workspaces": [{
                    "id": "wtest",
                    "identity_cwd": saved,
                    "tabs": [{
                        "layout": { "Pane": 0 },
                        "panes": {
                            "0": { "cwd": saved },
                            "1": { "cwd": pane_dir }
                        },
                        "zoomed": false,
                        "focused": 0,
                        "root_pane": 0
                    }],
                    "active_tab": 0
                }],
                "active": 0,
                "selected": 0
            })
            .to_string(),
        );

        let places = blocking_lookup(
            "zephyr",
            MAX_PLACES,
            LOOKUP_DEADLINE,
            fixture.sources(vec![PathBuf::from(&live)]),
        );

        let paths: Vec<&str> = places.iter().map(|place| place.path.as_str()).collect();
        assert_eq!(paths, vec![live, saved, pane_dir, z]);
        assert_eq!(
            places.iter().map(|place| place.source).collect::<Vec<_>>(),
            vec![
                PlaceSource::Workspace,
                PlaceSource::Workspace,
                PlaceSource::Workspace,
                PlaceSource::Z,
            ]
        );
    }

    #[test]
    fn a_lookup_that_runs_out_of_time_answers_nothing() {
        let fixture = Fixture::new();
        let live = fixture.dir("code/omega-live");
        let found = || {
            vec![Place {
                path: live.clone(),
                source: PlaceSource::Workspace,
            }]
        };
        let runtime = runtime().expect("a lookup runtime");

        // Work that finishes inside its deadline is answered.
        assert_eq!(
            runtime.block_on(within_deadline(LOOKUP_DEADLINE, async { found() })),
            found()
        );

        // Work that outruns it is not, and the answer is the same empty
        // list a desk with no match gives.
        assert!(runtime
            .block_on(within_deadline(Duration::from_millis(10), async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                found()
            }))
            .is_empty());

        // And the real lookup does answer under the real deadline, so the
        // deadline is not quietly swallowing every result.
        assert_eq!(
            blocking_lookup(
                "omega",
                MAX_PLACES,
                LOOKUP_DEADLINE,
                fixture.sources(vec![PathBuf::from(&live)])
            )
            .len(),
            1
        );
    }

    #[test]
    fn the_success_response_is_the_shape_a_client_reads() {
        let fixture = Fixture::new();
        let live = fixture.dir("code/iris");

        let response = respond(
            "req_place".into(),
            "iris",
            MAX_PLACES,
            fixture.sources(vec![PathBuf::from(&live)]),
        );

        assert_eq!(
            response,
            format!(
                r#"{{"id":"req_place","result":{{"type":"server_lookup_place","places":[{{"path":"{live}","source":"workspace"}}]}}}}"#
            )
        );
    }

    #[test]
    fn a_scan_that_outlives_its_request_keeps_its_turn() {
        let fixture = Fixture::new();
        let live = fixture.dir("code/sable");
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(std::sync::Mutex::new(held));

        // Two requests whose scans will not finish until this test lets
        // them, each given a deadline it is bound to miss.
        let abandoned: Vec<_> = (0..CONCURRENT_LOOKUPS)
            .map(|_| {
                let started = Arc::clone(&started);
                let finished = Arc::clone(&finished);
                let held = Arc::clone(&held);
                let sources = fixture.sources_from(move || {
                    started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Bounded, so a regression that waits this scan out
                    // shows up as a slow answer rather than a hung test.
                    let _ = held
                        .lock()
                        .expect("gate")
                        .recv_timeout(Duration::from_secs(5));
                    finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Vec::new()
                });
                std::thread::spawn(move || {
                    blocking_lookup("sable", MAX_PLACES, Duration::from_millis(200), sources)
                })
            })
            .collect();

        // Wait until both scans are genuinely running.
        let waiting_until = std::time::Instant::now() + Duration::from_secs(10);
        while started.load(std::sync::atomic::Ordering::SeqCst) < CONCURRENT_LOOKUPS {
            assert!(
                std::time::Instant::now() < waiting_until,
                "the stuck scans never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Both requests give up at their deadline and answer nothing,
        // while their scans are still running.
        for thread in abandoned {
            assert!(thread.join().expect("abandoned request").is_empty());
        }

        // The turns belong to the scans, not to the requests that walked
        // away from them, so nothing is free while those scans run.
        assert_eq!(
            permits().available_permits(),
            0,
            "an abandoned request handed its turn back while its scan was still running"
        );

        // A third request therefore finds no turn free. It answers empty
        // inside its own deadline, and no third scan is ever started — not
        // while the two are stuck, and not once they are released either.
        let counted = Arc::clone(&started);
        let began = std::time::Instant::now();
        let places = blocking_lookup(
            "sable",
            MAX_PLACES,
            Duration::from_millis(200),
            fixture.sources_from(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![PathBuf::from(&live)]
            }),
        );
        let waited = began.elapsed();

        assert!(places.is_empty(), "places: {places:?}");
        assert!(
            waited < Duration::from_secs(1),
            "the third request waited {waited:?}, far past its own deadline"
        );
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            CONCURRENT_LOOKUPS,
            "a third scan started while two were still running"
        );

        for _ in 0..CONCURRENT_LOOKUPS {
            release.send(()).expect("release the stuck scans");
        }

        // Let the released scans finish and hand their turns back. A scan
        // the third request had queued behind them would run now, so the
        // count is read again after the pool has been free for a while.
        let freed_by = std::time::Instant::now() + Duration::from_secs(10);
        while finished.load(std::sync::atomic::Ordering::SeqCst) < CONCURRENT_LOOKUPS
            || permits().available_permits() < CONCURRENT_LOOKUPS
        {
            assert!(
                std::time::Instant::now() < freed_by,
                "a finished scan never gave its turn back"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            CONCURRENT_LOOKUPS,
            "a third scan ran after the two before it were released"
        );
    }

    #[test]
    fn a_stalled_app_spends_the_deadline_and_no_more() {
        let fixture = Fixture::new();
        let (release, held) = std::sync::mpsc::channel::<()>();

        // The app never answers. The request gives up at its deadline
        // rather than waiting out the ask.
        let began = std::time::Instant::now();
        let places = blocking_lookup(
            "quartz",
            MAX_PLACES,
            Duration::from_millis(200),
            fixture.sources_from(move || {
                // Bounded for the same reason: a request that waits the
                // app out is a slow answer here, not a hung test.
                let _ = held.recv_timeout(Duration::from_secs(3));
                Vec::new()
            }),
        );
        let waited = began.elapsed();

        assert!(places.is_empty(), "places: {places:?}");
        assert!(
            waited < Duration::from_secs(1),
            "a stalled app held the request for {waited:?}"
        );

        drop(release);
    }

    #[test]
    fn only_two_lookups_hold_the_gate_at_once() {
        let first = Arc::clone(permits())
            .try_acquire_owned()
            .expect("a first lookup may start");
        let second = Arc::clone(permits())
            .try_acquire_owned()
            .expect("a second lookup may start");
        assert!(
            Arc::clone(permits()).try_acquire_owned().is_err(),
            "a third lookup must wait for one of the two in flight"
        );
        drop(first);
        assert!(
            Arc::clone(permits()).try_acquire_owned().is_ok(),
            "a freed permit lets the next lookup through"
        );
        drop(second);
    }
}
