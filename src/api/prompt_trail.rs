//! Serving `pane.prompt_trail`.
//!
//! Reading a session record means opening a file, walking it and parsing
//! it, which is not work the app thread — driving PTYs, rendering, and
//! every other queued request — should be doing on a client's behalf. So
//! the request is answered here on the connection thread, exactly as the
//! place lookup is. The only thing the app is asked for is the pane's own
//! identity and the session it reports, which it already holds in memory,
//! through the `pane.get` it already answers.
//!
//! One deadline covers the whole request: the wait for a turn, the ask to
//! the app, and the read. Inside it the ask carries its own shorter bound,
//! so a silent app spends that rather than the request. Two reads run at a
//! time per server, and the turn is held by the read rather than by the
//! request waiting on it, because a blocking read cannot be cancelled.
//!
//! A pane with no trail is not a failure. No session, a harness this does
//! not read, a record that is not on this desk or cannot be read within
//! its bounds: each is answered as a trail of `null` with the reason, and
//! the client reads the pane's screen instead. Only an unknown pane and an
//! unreachable app are errors.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use crate::api::schema::{
    AgentSessionInfo, ErrorResponse, Method, PaneTarget, PromptTrail, PromptTrailAgent,
    PromptTrailReason, Request, ResponseResult, SuccessResponse,
};
use crate::api::ApiRequestSender;

/// How long a whole trail request may take before it gives up.
const TRAIL_DEADLINE: Duration = Duration::from_secs(5);

/// Trail reads allowed to run at once in this process.
const CONCURRENT_TRAILS: usize = 2;

/// How long the app gets to name the pane. Shorter than the whole
/// request's deadline, so a silent app is the inner bound rather than the
/// thing that spends the request.
const PANE_TIMEOUT: Duration = Duration::from_secs(2);

/// Where one trail is to be read from.
///
/// Production fills this from the running server and the real home; a test
/// hands over a fixture, which is what keeps the tests off the machine's
/// own desk.
pub(super) struct Sources {
    /// Asks the app which pane this is and what session it reports, as the
    /// raw response the app answered with. Held as something still to do,
    /// not something already done, because asking the app blocks and every
    /// part of a request belongs inside the same deadline and the same
    /// turn.
    pub pane: Box<dyn FnOnce() -> String + Send>,
    /// Home directory the records are filed under.
    pub home: Option<PathBuf>,
    /// Reads one record into a trail. Production walks the file; a test
    /// hands over a reader it controls, so a read that outlives the
    /// deadline can be staged without a record the size of the bound.
    pub record: RecordReader,
}

/// Which pane a request turned out to be about.
///
/// Published out of the read as soon as the app names it, so a read that
/// then outruns the deadline can still answer for that pane rather than
/// failing the request. A pane with an unreadable record is an ordinary
/// answer; a request that never learned which pane it was about is not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneIdentity {
    pane_id: String,
    terminal_id: String,
}

type SeenPane = Arc<std::sync::Mutex<Option<PaneIdentity>>>;

/// Reads one record into a trail.
type RecordReader = Box<
    dyn FnOnce(&std::path::Path, PromptTrailAgent) -> Result<PromptTrail, PromptTrailReason> + Send,
>;

/// What one request has to say, before it is encoded.
enum Answer {
    /// The pane, and its trail or the reason it has none.
    Trail {
        pane_id: String,
        terminal_id: String,
        trail: Option<PromptTrail>,
        reason: Option<PromptTrailReason>,
    },
    /// The request could not be answered at all: an unknown pane, or an
    /// app that never spoke.
    Refused { code: String, message: String },
}

/// Answer `pane.prompt_trail` for one request.
pub(super) fn handle_prompt_trail(
    request_id: String,
    params: &PaneTarget,
    api_tx: &ApiRequestSender,
) -> String {
    if params.pane_id.trim().is_empty() {
        return crate::api::server::error_response_json(
            request_id,
            "invalid_params",
            "pane_id must not be empty".into(),
        );
    }

    let api_tx = api_tx.clone();
    let pane_id = params.pane_id.clone();
    let sources = Sources {
        pane: Box::new(move || ask_for_pane(&pane_id, &api_tx)),
        home: crate::worktree::home_dir(),
        record: Box::new(crate::prompt_trail::read),
    };

    respond(request_id, sources)
}

fn respond(request_id: String, sources: Sources) -> String {
    encode(request_id, blocking_trail(TRAIL_DEADLINE, sources))
}

fn encode(request_id: String, answer: Answer) -> String {
    let result = match answer {
        Answer::Trail {
            pane_id,
            terminal_id,
            trail,
            reason,
        } => ResponseResult::PromptTrail {
            pane_id,
            terminal_id,
            trail,
            reason,
        },
        Answer::Refused { code, message } => {
            return crate::api::server::error_response_json(request_id, &code, message)
        }
    };
    serde_json::to_string(&SuccessResponse {
        id: request_id.clone(),
        result,
    })
    .unwrap_or_else(|_| {
        crate::api::server::error_response_json(
            request_id,
            "internal_error",
            "failed to encode response".into(),
        )
    })
}

/// Ask the app which pane this is. This is the only part of a trail the
/// app thread ever runs, and it reads state already in memory.
fn ask_for_pane(pane_id: &str, api_tx: &ApiRequestSender) -> String {
    crate::api::server::dispatch_to_app_with_timeout(
        Request {
            id: "internal:prompt_trail:pane".into(),
            method: Method::PaneGet(PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(PANE_TIMEOUT),
    )
}

/// The runtime the bounded part of a trail runs on.
///
/// The API server's connections are plain threads started before the app's
/// runtime exists, so a trail brings its own rather than borrowing one it
/// cannot be sure is there. Two workers is the same number as the
/// concurrency bound, and it is built once, on the first trail a process
/// ever serves.
fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(CONCURRENT_TRAILS)
                // As many blocking threads as there are turns to be had, so
                // the pool cannot grow past the concurrency bound however
                // this file is later edited.
                .max_blocking_threads(CONCURRENT_TRAILS)
                .thread_name("herdr-prompt-trail")
                .enable_all()
                .build()
                .ok()
        })
        .as_ref()
}

fn permits() -> &'static Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(Semaphore::new(CONCURRENT_TRAILS)))
}

/// Run one trail to completion from a blocking thread, whole.
///
/// The deadline starts here and covers everything: waiting for a turn,
/// asking the app, and reading the record.
fn blocking_trail(deadline: Duration, sources: Sources) -> Answer {
    let Some(runtime) = runtime() else {
        return unavailable("no runtime to read a prompt trail on");
    };
    let seen: SeenPane = Arc::new(std::sync::Mutex::new(None));
    runtime.block_on(within_deadline(
        deadline,
        trail(sources, Arc::clone(&seen)),
        &seen,
    ))
}

/// Hold one trail to its deadline.
///
/// What running out of time means depends on how far the request got. Once
/// the app has named the pane, a read still going is a record this server
/// could not read in the time it allows itself, which is one of the
/// ordinary reasons a pane has no trail. Before that, the request never
/// learned which pane it was about, and there is nothing to answer for.
async fn within_deadline(
    deadline: Duration,
    work: impl std::future::Future<Output = Answer>,
    seen: &SeenPane,
) -> Answer {
    match tokio::time::timeout(deadline, work).await {
        Ok(answer) => answer,
        Err(_) => match seen.lock().ok().and_then(|pane| pane.clone()) {
            Some(pane) => Answer::Trail {
                pane_id: pane.pane_id,
                terminal_id: pane.terminal_id,
                trail: None,
                reason: Some(PromptTrailReason::Unreadable),
            },
            None => unavailable("timed out reading the prompt trail"),
        },
    }
}

fn unavailable(message: &str) -> Answer {
    Answer::Refused {
        code: "server_unavailable".into(),
        message: message.into(),
    }
}

async fn trail(sources: Sources, seen: SeenPane) -> Answer {
    // A queue rather than a refusal: a caller that arrives third waits its
    // turn instead of adding to the load, and gives up when the deadline
    // above says so.
    let Ok(permit) = Arc::clone(permits()).acquire_owned().await else {
        return unavailable("no turn to read a prompt trail in");
    };
    read(permit, sources, seen).await
}

/// The blocking half of a trail: ask the app, find the record, read it.
///
/// The turn travels into the closure rather than staying with the request
/// that started it. A blocking task cannot be cancelled, so a request that
/// gives up at its deadline leaves this read running — and the place it
/// was counted in stays taken until the read itself is over, which is what
/// keeps the number of reads in flight at the bound instead of the number
/// of requests still waiting.
async fn read(
    permit: tokio::sync::OwnedSemaphorePermit,
    sources: Sources,
    seen: SeenPane,
) -> Answer {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Sources { pane, home, record } = sources;
        answer_from(&pane(), home.as_deref(), record, &seen)
    })
    .await
    .unwrap_or_else(|_| unavailable("the prompt trail read did not finish"))
}

/// Turn the app's answer about a pane into this request's answer.
fn answer_from(
    pane_response: &str,
    home: Option<&std::path::Path>,
    record: impl FnOnce(&std::path::Path, PromptTrailAgent) -> Result<PromptTrail, PromptTrailReason>,
    seen: &SeenPane,
) -> Answer {
    let pane = match serde_json::from_str::<SuccessResponse>(pane_response) {
        Ok(response) => match response.result {
            ResponseResult::PaneInfo { pane } => pane,
            _ => return unavailable("the app answered something other than a pane"),
        },
        // The app's own refusal is this request's refusal, unchanged: an
        // unknown pane reads the same here as it does anywhere else.
        Err(_) => {
            return match serde_json::from_str::<ErrorResponse>(pane_response) {
                Ok(response) => Answer::Refused {
                    code: response.error.code,
                    message: response.error.message,
                },
                Err(_) => unavailable("the app answered nothing a pane could be read from"),
            }
        }
    };

    // Said before the record is opened, so a read that outruns the deadline
    // still has a pane to answer for.
    if let Ok(mut identity) = seen.lock() {
        *identity = Some(PaneIdentity {
            pane_id: pane.pane_id.clone(),
            terminal_id: pane.terminal_id.clone(),
        });
    }

    let (trail, reason) = match trail_for(pane.agent_session.as_ref(), home, record) {
        Ok(trail) => (Some(trail), None),
        Err(reason) => (None, Some(reason)),
    };
    Answer::Trail {
        pane_id: pane.pane_id,
        terminal_id: pane.terminal_id,
        trail,
        reason,
    }
}

/// The trail one pane's reported session leads to, or why there is none.
fn trail_for(
    session: Option<&AgentSessionInfo>,
    home: Option<&std::path::Path>,
    record: impl FnOnce(&std::path::Path, PromptTrailAgent) -> Result<PromptTrail, PromptTrailReason>,
) -> Result<PromptTrail, PromptTrailReason> {
    let session = session.ok_or(PromptTrailReason::NoSession)?;
    let agent = crate::prompt_trail::trail_agent(&session.agent)
        .ok_or(PromptTrailReason::UnsupportedAgent)?;
    let path = crate::prompt_trail::record_path(
        home,
        agent,
        session.kind,
        &session.value,
        session.record_path.as_deref(),
    )
    .ok_or(PromptTrailReason::NoRecord)?;
    record(&path, agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRefKind;
    use crate::api::schema::{AgentStatus, PaneInfo, PromptTrailAgent};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    /// A throwaway home holding real records, so what a trail finds is
    /// answered by a filesystem rather than by a stub, and never by the
    /// desk the test is running on.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "herdr-api-prompt-trail-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("fixture home");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture parent");
            }
            std::fs::write(&path, contents).expect("fixture record");
            path
        }

        /// Sources answering with one already-made pane response.
        fn sources(&self, pane_response: String) -> Sources {
            self.sources_from(move || pane_response)
        }

        /// The same, with the app's answer produced by a closure the test
        /// controls, so a stalled or counted ask can be staged.
        fn sources_from(&self, pane: impl FnOnce() -> String + Send + 'static) -> Sources {
            Sources {
                pane: Box::new(pane),
                home: Some(self.root.clone()),
                record: Box::new(crate::prompt_trail::read),
            }
        }

        /// The same again, with the record read by a closure the test
        /// controls, so a read that outlives the deadline can be staged
        /// without a record the size of the bound.
        fn sources_reading(
            &self,
            pane_response: String,
            record: impl FnOnce(&std::path::Path, PromptTrailAgent) -> Result<PromptTrail, PromptTrailReason>
                + Send
                + 'static,
        ) -> Sources {
            Sources {
                pane: Box::new(move || pane_response),
                home: Some(self.root.clone()),
                record: Box::new(record),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn pane(session: Option<AgentSessionInfo>) -> PaneInfo {
        PaneInfo {
            pane_id: "wG4:p1".into(),
            terminal_id: "term_beacon".into(),
            workspace_id: "wG4".into(),
            tab_id: "wG4:t1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Working,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: session,
            scroll: None,
            mouse_tracking: false,
            alternate_screen: false,
            agent_status_changed_at: None,
            pinned: false,
            label_source: None,
            label_at: None,
            revision: 4,
        }
    }

    fn session(agent: &str, kind: AgentSessionRefKind, value: &str) -> AgentSessionInfo {
        AgentSessionInfo {
            source: format!("herdr:{agent}"),
            agent: agent.into(),
            kind,
            value: value.into(),
            record_path: None,
        }
    }

    fn pane_response(pane: PaneInfo) -> String {
        serde_json::to_string(&SuccessResponse {
            id: "internal:prompt_trail:pane".into(),
            result: ResponseResult::PaneInfo { pane },
        })
        .expect("a pane response")
    }

    fn answered(response: &str) -> ResponseResult {
        serde_json::from_str::<SuccessResponse>(response)
            .unwrap_or_else(|_| panic!("expected a success response, got: {response}"))
            .result
    }

    fn reason_of(response: &str) -> Option<PromptTrailReason> {
        match answered(response) {
            ResponseResult::PromptTrail { trail, reason, .. } => {
                assert_eq!(trail, None, "a trail and a reason both answered");
                reason
            }
            other => panic!("expected a prompt trail, got: {other:?}"),
        }
    }

    fn trail_of(response: &str) -> PromptTrail {
        match answered(response) {
            ResponseResult::PromptTrail { trail, reason, .. } => {
                assert_eq!(reason, None, "a trail and a reason both answered");
                trail.expect("a trail")
            }
            other => panic!("expected a prompt trail, got: {other:?}"),
        }
    }

    const CLAUDE_SESSION: &str = "5f2a9c11-0b44-4d8e-9a10-6c3b7e5d1f22";

    fn claude_record() -> String {
        [
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": "why does the beacon relay drop frames" },
                "timestamp": "2026-03-04T09:15:00.120Z",
            }),
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "tool_result", "content": "3" }] },
                "timestamp": "2026-03-04T09:16:00.000Z",
            }),
            serde_json::json!({
                "type": "ai-title",
                "aiTitle": "Beacon relay frame drops",
                "timestamp": "2026-03-04T09:17:00.000Z",
            }),
        ]
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }

    #[test]
    fn a_blank_pane_id_never_reaches_the_app() {
        // The receiver is dropped, so any dispatch to the app would fail: a
        // refused request must be refused before it costs the app anything.
        let (tx, _) = mpsc::unbounded_channel();
        for pane_id in ["", "   ", "\t\n"] {
            let response = handle_prompt_trail(
                "req_blank".into(),
                &PaneTarget {
                    pane_id: pane_id.into(),
                },
                &tx,
            );
            let parsed: ErrorResponse = serde_json::from_str(&response)
                .unwrap_or_else(|_| panic!("expected an error response, got: {response}"));
            assert_eq!(parsed.error.code, "invalid_params");
        }
    }

    #[test]
    fn the_app_is_asked_which_pane_this_is_and_nothing_else() {
        let (tx, mut rx) = mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let mut methods = Vec::new();
            while let Some(message) = rx.blocking_recv() {
                methods.push(crate::api::server::api_method_name_for_test(
                    &message.request.method,
                ));
                let _ = message.respond_to.send(pane_response(pane(None)));
            }
            methods
        });

        let asked = ask_for_pane("wG4:p1", &tx.clone());
        drop(tx);
        let methods = responder.join().expect("responder");

        assert_eq!(methods, vec!["pane.get"]);
        assert!(
            matches!(answered(&asked), ResponseResult::PaneInfo { .. }),
            "the app answered: {asked}"
        );
    }

    #[test]
    fn a_pane_the_app_does_not_know_is_refused_in_its_own_words() {
        let fixture = Fixture::new();
        let refusal = crate::api::server::error_response_json(
            "internal:prompt_trail:pane".into(),
            "pane_not_found",
            "pane wG4:p9 not found".into(),
        );

        let response = respond("req_trail".into(), fixture.sources(refusal));

        let parsed: ErrorResponse = serde_json::from_str(&response)
            .unwrap_or_else(|_| panic!("expected an error response, got: {response}"));
        assert_eq!(parsed.id, "req_trail");
        assert_eq!(parsed.error.code, "pane_not_found");
        assert_eq!(parsed.error.message, "pane wG4:p9 not found");
    }

    #[test]
    fn a_pane_with_no_trail_says_why_rather_than_failing() {
        let fixture = Fixture::new();

        // A plain shell, or a pane whose harness never reported a session.
        assert_eq!(
            reason_of(&respond(
                "req_shell".into(),
                fixture.sources(pane_response(pane(None)))
            )),
            Some(PromptTrailReason::NoSession)
        );

        // A harness whose records this server does not read.
        assert_eq!(
            reason_of(&respond(
                "req_other".into(),
                fixture.sources(pane_response(pane(Some(session(
                    "droid",
                    AgentSessionRefKind::Id,
                    "droid-session"
                )))))
            )),
            Some(PromptTrailReason::UnsupportedAgent)
        );

        // A session whose record was never written here.
        assert_eq!(
            reason_of(&respond(
                "req_absent".into(),
                fixture.sources(pane_response(pane(Some(session(
                    "claude",
                    AgentSessionRefKind::Id,
                    CLAUDE_SESSION
                )))))
            )),
            Some(PromptTrailReason::NoRecord)
        );

        // A record that is there but past the bound this reads within.
        let oversize = fixture.write(
            &format!(".claude/projects/-invented-beacon/{CLAUDE_SESSION}.jsonl"),
            "",
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(&oversize)
            .expect("fixture record")
            .set_len(crate::prompt_trail::MAX_RECORD_BYTES + 1)
            .expect("a record past the bound");
        assert_eq!(
            reason_of(&respond(
                "req_huge".into(),
                fixture.sources(pane_response(pane(Some(session(
                    "claude",
                    AgentSessionRefKind::Id,
                    CLAUDE_SESSION
                )))))
            )),
            Some(PromptTrailReason::Unreadable)
        );
    }

    #[test]
    fn the_record_a_pane_reports_is_read_into_its_trail() {
        let fixture = Fixture::new();
        fixture.write(
            &format!(".claude/projects/-invented-beacon/{CLAUDE_SESSION}.jsonl"),
            &claude_record(),
        );

        let response = respond(
            "req_trail".into(),
            fixture.sources(pane_response(pane(Some(session(
                "claude",
                AgentSessionRefKind::Id,
                CLAUDE_SESSION,
            ))))),
        );

        let trail = trail_of(&response);
        assert_eq!(trail.agent, PromptTrailAgent::Claude);
        assert_eq!(trail.count, 1);
        assert_eq!(
            trail.first.as_ref().map(|first| first.text.as_str()),
            Some("why does the beacon relay drop frames")
        );
        assert_eq!(trail.title.as_deref(), Some("Beacon relay frame drops"));
        assert_eq!(trail.newest_at, Some(1772615700));

        match answered(&response) {
            ResponseResult::PromptTrail {
                pane_id,
                terminal_id,
                ..
            } => {
                assert_eq!(pane_id, "wG4:p1");
                assert_eq!(terminal_id, "term_beacon");
            }
            other => panic!("expected a prompt trail, got: {other:?}"),
        }
    }

    #[test]
    fn a_record_the_harness_named_is_read_from_where_it_said() {
        let fixture = Fixture::new();
        // The derivation would find this one, and must not be consulted.
        fixture.write(
            &format!(".claude/projects/-invented-beacon/{CLAUDE_SESSION}.jsonl"),
            &claude_record(),
        );
        let reported = fixture.write("elsewhere/moved.jsonl", &{
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": "read me from where the harness said" },
                "timestamp": "2026-03-04T09:15:00Z",
            })
            .to_string()
                + "\n"
        });

        let mut reported_session = session("claude", AgentSessionRefKind::Id, CLAUDE_SESSION);
        reported_session.record_path = reported.to_str().map(str::to_string);

        let trail = trail_of(&respond(
            "req_reported".into(),
            fixture.sources(pane_response(pane(Some(reported_session)))),
        ));

        assert_eq!(
            trail.first.as_ref().map(|first| first.text.as_str()),
            Some("read me from where the harness said")
        );
    }

    #[test]
    fn the_success_response_is_the_shape_a_client_reads() {
        let fixture = Fixture::new();

        let response = respond(
            "req_trail".into(),
            fixture.sources(pane_response(pane(None))),
        );

        assert_eq!(
            response,
            r#"{"id":"req_trail","result":{"type":"prompt_trail","pane_id":"wG4:p1","terminal_id":"term_beacon","trail":null,"reason":"no_session"}}"#
        );
    }

    #[test]
    fn a_stalled_app_spends_the_deadline_and_no_more() {
        let fixture = Fixture::new();
        let (release, held) = std::sync::mpsc::channel::<()>();

        // The app never answers. The request gives up at its deadline
        // rather than waiting out the ask.
        let began = std::time::Instant::now();
        let answer = blocking_trail(
            Duration::from_millis(200),
            fixture.sources_from(move || {
                // Bounded for the same reason: a request that waits the app
                // out is a slow answer here, not a hung test.
                let _ = held.recv_timeout(Duration::from_secs(3));
                String::new()
            }),
        );
        let waited = began.elapsed();

        assert!(
            matches!(&answer, Answer::Refused { code, .. } if code == "server_unavailable"),
            "a stalled app was answered as a trail"
        );
        assert!(
            waited < Duration::from_secs(1),
            "a stalled app held the request for {waited:?}"
        );

        drop(release);
    }

    /// Once the app has named the pane, a record this server could not read
    /// in the time it allows itself is one of the ordinary reasons a pane
    /// has no trail — not a transport failure the client has to guess at.
    #[test]
    fn a_read_that_outlives_the_deadline_says_the_record_was_unreadable() {
        let fixture = Fixture::new();
        fixture.write(
            &format!(".claude/projects/-invented-beacon/{CLAUDE_SESSION}.jsonl"),
            &claude_record(),
        );
        let (release, held) = std::sync::mpsc::channel::<()>();

        let began = std::time::Instant::now();
        let answer = blocking_trail(
            Duration::from_millis(200),
            fixture.sources_reading(
                pane_response(pane(Some(session(
                    "claude",
                    AgentSessionRefKind::Id,
                    CLAUDE_SESSION,
                )))),
                move |_, _| {
                    // Bounded, so a regression that waits this read out is a
                    // slow answer here rather than a hung test.
                    let _ = held.recv_timeout(Duration::from_secs(3));
                    Err(PromptTrailReason::NoRecord)
                },
            ),
        );
        let waited = began.elapsed();

        match answer {
            Answer::Trail {
                pane_id,
                terminal_id,
                trail,
                reason,
            } => {
                assert_eq!(pane_id, "wG4:p1");
                assert_eq!(terminal_id, "term_beacon");
                assert_eq!(trail, None);
                assert_eq!(reason, Some(PromptTrailReason::Unreadable));
            }
            Answer::Refused { code, message } => {
                panic!("a read that ran long was refused as {code}: {message}")
            }
        }
        assert!(
            waited < Duration::from_secs(1),
            "the request waited {waited:?}, far past its own deadline"
        );

        drop(release);
    }

    #[test]
    fn a_read_that_outlives_its_request_keeps_its_turn() {
        let fixture = Fixture::new();
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(std::sync::Mutex::new(held));

        // Two requests whose reads will not finish until this test lets
        // them, each given a deadline it is bound to miss.
        let abandoned: Vec<_> = (0..CONCURRENT_TRAILS)
            .map(|_| {
                let started = Arc::clone(&started);
                let finished = Arc::clone(&finished);
                let held = Arc::clone(&held);
                let sources = fixture.sources_from(move || {
                    started.fetch_add(1, Ordering::SeqCst);
                    let _ = held
                        .lock()
                        .expect("gate")
                        .recv_timeout(Duration::from_secs(5));
                    finished.fetch_add(1, Ordering::SeqCst);
                    String::new()
                });
                std::thread::spawn(move || blocking_trail(Duration::from_millis(200), sources))
            })
            .collect();

        let waiting_until = std::time::Instant::now() + Duration::from_secs(10);
        while started.load(Ordering::SeqCst) < CONCURRENT_TRAILS {
            assert!(
                std::time::Instant::now() < waiting_until,
                "the stuck reads never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        for thread in abandoned {
            assert!(
                matches!(
                    thread.join().expect("abandoned request"),
                    Answer::Refused { .. }
                ),
                "an abandoned request answered a trail"
            );
        }

        // The turns belong to the reads, not to the requests that walked
        // away from them, so nothing is free while those reads run.
        assert_eq!(
            permits().available_permits(),
            0,
            "an abandoned request handed its turn back while its read was still running"
        );

        // A third request therefore finds no turn free, and no third read is
        // ever started.
        let counted = Arc::clone(&started);
        let began = std::time::Instant::now();
        let answer = blocking_trail(
            Duration::from_millis(200),
            fixture.sources_from(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                pane_response(pane(None))
            }),
        );
        let waited = began.elapsed();

        assert!(matches!(answer, Answer::Refused { .. }));
        assert!(
            waited < Duration::from_secs(1),
            "the third request waited {waited:?}, far past its own deadline"
        );
        assert_eq!(
            started.load(Ordering::SeqCst),
            CONCURRENT_TRAILS,
            "a third read started while two were still running"
        );

        for _ in 0..CONCURRENT_TRAILS {
            release.send(()).expect("release the stuck reads");
        }

        let freed_by = std::time::Instant::now() + Duration::from_secs(10);
        while finished.load(Ordering::SeqCst) < CONCURRENT_TRAILS
            || permits().available_permits() < CONCURRENT_TRAILS
        {
            assert!(
                std::time::Instant::now() < freed_by,
                "a finished read never gave its turn back"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            started.load(Ordering::SeqCst),
            CONCURRENT_TRAILS,
            "a third read ran after the two before it were released"
        );
    }

    #[test]
    fn only_two_trails_hold_the_gate_at_once() {
        let first = Arc::clone(permits())
            .try_acquire_owned()
            .expect("a first trail may start");
        let second = Arc::clone(permits())
            .try_acquire_owned()
            .expect("a second trail may start");
        assert!(
            Arc::clone(permits()).try_acquire_owned().is_err(),
            "a third trail must wait for one of the two in flight"
        );
        drop(first);
        assert!(
            Arc::clone(permits()).try_acquire_owned().is_ok(),
            "a freed permit lets the next trail through"
        );
        drop(second);
    }
}
