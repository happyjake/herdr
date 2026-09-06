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
//! Three bounds keep a slow or hostile desk from turning into a stuck
//! request: tmux gets its own short deadline, the whole gather runs under
//! an overall deadline that answers nothing when it fires, and only two
//! lookups run at a time per server process so a client cannot multiply the
//! cost by reconnecting.

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

/// How long the app gets to hand back its live pane directories.
const LIVE_DIRS_TIMEOUT: Duration = Duration::from_secs(5);

/// Where one lookup's memory is to be read from.
///
/// Production fills this from the running server and the real home; a test
/// hands over a fixture, which is what keeps the tests off the machine's
/// own desk.
pub(super) struct Sources {
    /// Directories the server's live panes stand in, already asked of the
    /// app.
    pub live: Vec<PathBuf>,
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

    // The sources are read inside the gate, so the app request this
    // lookup makes is bounded by the same two-at-a-time rule as the
    // filesystem work behind it.
    let places = blocking_lookup(query, limit, LOOKUP_DEADLINE, || Sources {
        live: live_pane_dirs(api_tx),
        home: crate::worktree::home_dir(),
        persisted: Some(crate::persist::session_path()),
        consult_tmux: true,
    });

    encode(request_id, places)
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

/// Run one lookup to completion from a blocking thread.
///
/// `sources` is read after the gate is passed, so whatever it costs to
/// collect counts against the same bound as the lookup itself.
pub(super) fn blocking_lookup(
    query: &str,
    limit: usize,
    deadline: Duration,
    sources: impl FnOnce() -> Sources,
) -> Vec<Place> {
    let Some(runtime) = runtime() else {
        return Vec::new();
    };
    // A queue rather than a refusal: a caller that arrives third waits its
    // turn instead of adding to the load.
    let Ok(_permit) = runtime.block_on(Arc::clone(permits()).acquire_owned()) else {
        return Vec::new();
    };
    let sources = sources();
    runtime.block_on(within_deadline(
        deadline,
        gather(query.to_string(), limit, sources),
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

async fn gather(query: String, limit: usize, sources: Sources) -> Vec<Place> {
    let tmux = if sources.consult_tmux {
        place_lookup::tmux_pane_dirs().await
    } else {
        Vec::new()
    };

    tokio::task::spawn_blocking(move || {
        let mut workspaces = sources.live;
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
            Sources {
                live,
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

        let dirs = live_pane_dirs(&tx);
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

        let places = blocking_lookup("zephyr", MAX_PLACES, LOOKUP_DEADLINE, || {
            fixture.sources(vec![PathBuf::from(&live)])
        });

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
            blocking_lookup("omega", MAX_PLACES, LOOKUP_DEADLINE, || fixture
                .sources(vec![PathBuf::from(&live)]))
            .len(),
            1
        );
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
