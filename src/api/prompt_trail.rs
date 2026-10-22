//! Serving `pane.prompt_trail`, `pane.turn_index` and `pane.preface`
//! through one bounded read.
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
    AgentSessionInfo, ErrorResponse, Method, PanePrefaceParams, PaneReadParams, PaneTarget,
    Preface, PrefaceReason, PromptTrail, PromptTrailAgent, PromptTrailReason, ReadFormat,
    ReadSource, Request, ResponseResult, SuccessResponse, TurnIndex,
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
pub(super) struct Sources<T: RecordOutput = PromptTrail> {
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
    pub record: RecordReader<T>,
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
type RecordReader<T = PromptTrail> = Box<
    dyn FnOnce(&std::path::Path, PromptTrailAgent) -> Result<T, <T as RecordOutput>::Reason> + Send,
>;

/// What one request has to say, before it is encoded.
enum Answer<T: RecordOutput = PromptTrail> {
    /// The pane, and its trail or the reason it has none.
    Trail {
        pane_id: String,
        terminal_id: String,
        trail: Option<T>,
        reason: Option<T::Reason>,
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

fn respond<T: RecordOutput>(request_id: String, sources: Sources<T>) -> String {
    encode(request_id, blocking_trail(TRAIL_DEADLINE, sources))
}

fn encode<T: RecordOutput>(request_id: String, answer: Answer<T>) -> String {
    let result = match answer {
        Answer::Trail {
            pane_id,
            terminal_id,
            trail,
            reason,
        } => T::response(pane_id, terminal_id, trail, reason),
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
fn blocking_trail<T: RecordOutput>(deadline: Duration, sources: Sources<T>) -> Answer<T> {
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
async fn within_deadline<T: RecordOutput>(
    deadline: Duration,
    work: impl std::future::Future<Output = Answer<T>>,
    seen: &SeenPane,
) -> Answer<T> {
    match tokio::time::timeout(deadline, work).await {
        Ok(answer) => answer,
        Err(_) => match seen.lock().ok().and_then(|pane| pane.clone()) {
            Some(pane) => Answer::Trail {
                pane_id: pane.pane_id,
                terminal_id: pane.terminal_id,
                trail: None,
                reason: Some(PromptTrailReason::Unreadable.into()),
            },
            None => unavailable("timed out reading the prompt trail"),
        },
    }
}

fn unavailable<T: RecordOutput>(message: &str) -> Answer<T> {
    Answer::Refused {
        code: "server_unavailable".into(),
        message: message.into(),
    }
}

async fn trail<T: RecordOutput>(sources: Sources<T>, seen: SeenPane) -> Answer<T> {
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
async fn read<T: RecordOutput>(
    permit: tokio::sync::OwnedSemaphorePermit,
    sources: Sources<T>,
    seen: SeenPane,
) -> Answer<T> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Sources { pane, home, record } = sources;
        answer_from(&pane(), home.as_deref(), record, &seen)
    })
    .await
    .unwrap_or_else(|_| unavailable("the prompt trail read did not finish"))
}

/// Turn the app's answer about a pane into this request's answer.
fn answer_from<T: RecordOutput>(
    pane_response: &str,
    home: Option<&std::path::Path>,
    record: impl FnOnce(&std::path::Path, PromptTrailAgent) -> Result<T, T::Reason>,
    seen: &SeenPane,
) -> Answer<T> {
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
fn trail_for<T: RecordOutput>(
    session: Option<&AgentSessionInfo>,
    home: Option<&std::path::Path>,
    record: impl FnOnce(&std::path::Path, PromptTrailAgent) -> Result<T, T::Reason>,
) -> Result<T, T::Reason> {
    let session = session.ok_or(PromptTrailReason::NoSession)?;
    let agent = crate::prompt_trail::trail_agent(&session.agent)
        .ok_or(PromptTrailReason::UnsupportedAgent)?;
    if !T::supports(agent) {
        return Err(PromptTrailReason::UnsupportedAgent.into());
    }
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

/// Output-specific policy, sharing lookup, deadline, runtime and permits.
pub(super) trait RecordOutput: Send + 'static {
    /// Why a pane has none of this output: the prompt trail's reasons, or
    /// more of an output's own.
    type Reason: From<PromptTrailReason> + Send + 'static;
    fn supports(agent: PromptTrailAgent) -> bool;
    fn response(
        pane_id: String,
        terminal_id: String,
        value: Option<Self>,
        reason: Option<Self::Reason>,
    ) -> ResponseResult
    where
        Self: Sized;
}

impl RecordOutput for PromptTrail {
    type Reason = PromptTrailReason;
    fn supports(_: PromptTrailAgent) -> bool {
        true
    }
    fn response(
        pane_id: String,
        terminal_id: String,
        trail: Option<Self>,
        reason: Option<PromptTrailReason>,
    ) -> ResponseResult {
        ResponseResult::PromptTrail {
            pane_id,
            terminal_id,
            trail,
            reason,
        }
    }
}

impl RecordOutput for TurnIndex {
    type Reason = PromptTrailReason;
    fn supports(agent: PromptTrailAgent) -> bool {
        agent == PromptTrailAgent::Pi
    }
    fn response(
        pane_id: String,
        terminal_id: String,
        index: Option<Self>,
        reason: Option<PromptTrailReason>,
    ) -> ResponseResult {
        ResponseResult::TurnIndex {
            pane_id,
            terminal_id,
            index,
            reason,
        }
    }
}

pub(super) fn handle_turn_index(
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
    respond(
        request_id,
        Sources {
            pane: Box::new(move || ask_for_pane(&pane_id, &api_tx)),
            home: crate::worktree::home_dir(),
            record: Box::new(|path, _| crate::turn_index::read(path)),
        },
    )
}

impl RecordOutput for Preface {
    type Reason = PrefaceReason;
    fn supports(agent: PromptTrailAgent) -> bool {
        crate::preface::reader_for(agent).is_some()
    }
    fn response(
        pane_id: String,
        terminal_id: String,
        preface: Option<Self>,
        reason: Option<PrefaceReason>,
    ) -> ResponseResult {
        crate::logging::api_preface_answered(
            reason.map(prefacereason_code),
            preface.as_ref().map_or(0, |preface| preface.turns.len()),
            preface.as_ref().map_or(0, |preface| preface.omitted),
        );
        ResponseResult::Preface {
            pane_id,
            terminal_id,
            preface,
            reason,
        }
    }
}

/// The wire name of a reason, which is all a log ever says about a preface
/// besides how many turns it carried.
fn prefacereason_code(reason: PrefaceReason) -> &'static str {
    match reason {
        PrefaceReason::NoSession => "no_session",
        PrefaceReason::UnsupportedAgent => "unsupported_agent",
        PrefaceReason::NoRecord => "no_record",
        PrefaceReason::Unreadable => "unreadable",
        PrefaceReason::SeamNotFound => "seam_not_found",
        PrefaceReason::NothingAbove => "nothing_above",
    }
}

/// Lines of the client's held screen a preface request may anchor with.
const MAX_TOP_LINES: usize = 64;

/// Answer `pane.preface` for one request.
pub(super) fn handle_preface(
    request_id: String,
    params: &PanePrefaceParams,
    api_tx: &ApiRequestSender,
) -> String {
    if params.pane_id.trim().is_empty() {
        return crate::api::server::error_response_json(
            request_id,
            "invalid_params",
            "pane_id must not be empty".into(),
        );
    }
    if params.top_offset.is_some() != params.top_lines.is_some() {
        return crate::api::server::error_response_json(
            request_id,
            "invalid_params",
            "top_offset and top_lines must both be set or both be null".into(),
        );
    }
    if params
        .top_lines
        .as_ref()
        .is_some_and(|lines| !(1..=MAX_TOP_LINES).contains(&lines.len()))
    {
        return crate::api::server::error_response_json(
            request_id,
            "invalid_params",
            format!("top_lines must hold 1 to {MAX_TOP_LINES} lines"),
        );
    }
    respond(
        request_id,
        preface_sources(params, api_tx, crate::worktree::home_dir()),
    )
}

/// Where one preface is read from: the app for the pane and its screen,
/// and the records filed under `home`.
fn preface_sources(
    params: &PanePrefaceParams,
    api_tx: &ApiRequestSender,
    home: Option<PathBuf>,
) -> Sources<Preface> {
    let pane_api_tx = api_tx.clone();
    let read_api_tx = api_tx.clone();
    let pane_id = params.pane_id.clone();
    let read_pane_id = params.pane_id.clone();
    let params = params.clone();
    Sources {
        pane: Box::new(move || ask_for_pane(&pane_id, &pane_api_tx)),
        home,
        record: Box::new(move |path, agent| {
            preface_from(path, agent, &params, |min_lines| {
                ask_for_span(&read_pane_id, min_lines, &read_api_tx)
            })
        }),
    }
}

/// The preface one record and one screen give.
fn preface_from(
    path: &std::path::Path,
    agent: PromptTrailAgent,
    params: &PanePrefaceParams,
    span: impl FnOnce(usize) -> Option<crate::preface::seam::Span>,
) -> Result<Preface, PrefaceReason> {
    let read = crate::preface::read(path, agent)?;
    let markers = crate::preface::reader_for(agent)
        .map(|reader| reader.prompt_markers())
        .unwrap_or_default();
    let fallback_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    crate::preface::seam::preface(
        crate::preface::agent_name(agent),
        fallback_session_id,
        &read,
        crate::preface::seam::Top {
            offset: params.top_offset,
            lines: params.top_lines.as_deref(),
        },
        params.max_turns,
        markers,
        span,
    )
}

/// Ask the app for the pane's recent history as one span: the offset pages
/// a client reads, read by the app from one state of the terminal. Anything
/// but such a read is no screen to place a seam on.
fn ask_for_span(
    pane_id: &str,
    min_lines: usize,
    api_tx: &ApiRequestSender,
) -> Option<crate::preface::seam::Span> {
    let response = crate::api::server::dispatch_to_app_with_timeout(
        Request {
            id: "internal:preface:read".into(),
            method: Method::PaneRead(PaneReadParams {
                pane_id: pane_id.to_string(),
                source: ReadSource::RecentUnwrapped,
                lines: Some(crate::preface::seam::PAGE_LINES),
                format: ReadFormat::Text,
                strip_ansi: true,
                intent: crate::api::schema::ReadIntent::Passive,
                span: Some(crate::api::schema::ReadSpan {
                    min_lines,
                    max_lines: crate::preface::seam::MAX_SCREEN_LINES,
                }),
                offset_from_bottom: None,
            }),
        },
        api_tx,
        Some(PANE_TIMEOUT),
    );
    match serde_json::from_str::<SuccessResponse>(&response)
        .ok()?
        .result
    {
        ResponseResult::PaneRead { read } => Some(crate::preface::seam::Span {
            text: read.text,
            has_more: read.has_more?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRefKind;
    use crate::api::schema::{AgentStatus, PaneInfo, PromptTrailAgent, TurnEnd, TurnStart};
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
            ended_agent: None,
            started_by: None,
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
            completion_seq: None,
            agent_status_changed_at: None,
            pinned: false,
            flag: None,
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
    #[test]
    fn turn_index_handler_resolves_the_pane_and_reads_its_record() {
        let fixture = Fixture::new();
        let path = fixture.write("record.jsonl", concat!(
            "{\"type\":\"session\",\"id\":\"session\",\"version\":3}\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-03-04T09:15:00Z\",\"message\":{\"role\":\"user\",\"content\":\"Explain the relay\"}}\n"
        ));
        let (tx, mut rx) = mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let message = rx.blocking_recv().expect("pane request");
            assert!(matches!(message.request.method, Method::PaneGet(_)));
            message
                .respond_to
                .send(pane_response(pane(Some(session(
                    "pi",
                    AgentSessionRefKind::Path,
                    path.to_str().unwrap(),
                )))))
                .unwrap();
        });
        let response = handle_turn_index(
            "request".into(),
            &PaneTarget {
                pane_id: "wG4:p1".into(),
            },
            &tx,
        );
        drop(tx);
        responder.join().unwrap();
        let result = answered(&response);
        match result {
            ResponseResult::TurnIndex {
                pane_id,
                terminal_id,
                index,
                reason,
            } => {
                assert_eq!(pane_id, "wG4:p1");
                assert_eq!(terminal_id, "term_beacon");
                assert_eq!(reason, None);
                assert_eq!(
                    index.unwrap().turns[0].prompt.as_deref(),
                    Some("Explain the relay")
                );
            }
            other => panic!("expected turn index, got {other:?}"),
        }
    }
    fn index_sources(fixture: &Fixture, response: String) -> Sources<TurnIndex> {
        Sources {
            pane: Box::new(move || response),
            home: Some(fixture.root.clone()),
            record: Box::new(|path, _| crate::turn_index::read(path)),
        }
    }

    #[test]
    fn turn_index_handler_reuses_refusals_and_rejects_other_agents_before_lookup() {
        let fixture = Fixture::new();
        for (session, expected) in [
            (None, PromptTrailReason::NoSession),
            (
                Some(session("claude", AgentSessionRefKind::Id, "absent")),
                PromptTrailReason::UnsupportedAgent,
            ),
            (
                Some(session("codex", AgentSessionRefKind::Id, "absent")),
                PromptTrailReason::UnsupportedAgent,
            ),
            (
                Some(session(
                    "pi",
                    AgentSessionRefKind::Path,
                    fixture.root.join("absent.jsonl").to_str().unwrap(),
                )),
                PromptTrailReason::NoRecord,
            ),
        ] {
            let response = respond(
                "request".into(),
                index_sources(&fixture, pane_response(pane(session))),
            );
            match answered(&response) {
                ResponseResult::TurnIndex { index, reason, .. } => {
                    assert_eq!(index, None);
                    assert_eq!(reason, Some(expected));
                }
                other => panic!("expected turn index, got {other:?}"),
            }
        }
        let refusal = crate::api::server::error_response_json(
            "internal".into(),
            "pane_not_found",
            "unknown pane".into(),
        );
        let response = respond("request".into(), index_sources(&fixture, refusal));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "pane_not_found");
        assert_eq!(error.id, "request");
        let (tx, _) = mpsc::unbounded_channel();
        let error: ErrorResponse = serde_json::from_str(&handle_turn_index(
            "blank".into(),
            &PaneTarget {
                pane_id: " ".into(),
            },
            &tx,
        ))
        .unwrap();
        assert_eq!(error.error.code, "invalid_params");
    }

    #[test]
    fn turn_index_handler_uses_reported_record_and_reports_unreadable() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "reported.jsonl",
            "{\"type\":\"session\",\"id\":\"reported\"}\n",
        );
        let mut identity = session("pi", AgentSessionRefKind::Path, "absent.jsonl");
        identity.record_path = Some(path.to_str().unwrap().into());
        let response = respond(
            "request".into(),
            index_sources(&fixture, pane_response(pane(Some(identity.clone())))),
        );
        match answered(&response) {
            ResponseResult::TurnIndex { index, reason, .. } => {
                let index = index.unwrap();
                assert_eq!(index.session_id, "reported");
                assert_eq!(index.leaf_id, None);
                assert!(index.turns.is_empty());
                assert_eq!(reason, None);
            }
            other => panic!("expected turn index, got {other:?}"),
        }
        fixture.write("reported.jsonl", "not a record\n");
        let response = respond(
            "request".into(),
            index_sources(&fixture, pane_response(pane(Some(identity)))),
        );
        assert!(matches!(
            answered(&response),
            ResponseResult::TurnIndex {
                index: None,
                reason: Some(PromptTrailReason::Unreadable),
                ..
            }
        ));
    }

    #[test]
    fn turn_index_shares_prompt_trail_permits_and_deadline() {
        let fixture = Fixture::new();
        let first = permits().clone().try_acquire_owned().unwrap();
        let second = permits().clone().try_acquire_owned().unwrap();
        let sources = Sources::<TurnIndex> {
            pane: Box::new(|| panic!("a read started without a shared permit")),
            home: None,
            record: Box::new(|_, _| panic!("record read without a permit")),
        };
        let start = std::time::Instant::now();
        let answer = blocking_trail(Duration::from_millis(100), sources);
        assert!(matches!(answer, Answer::Refused { .. }));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(first);
        drop(second);
        let path = fixture.write("record.jsonl", "");
        let response = pane_response(pane(Some(session(
            "pi",
            AgentSessionRefKind::Path,
            path.to_str().unwrap(),
        ))));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (finished, completion) = std::sync::mpsc::channel::<()>();
        let mut sources = index_sources(&fixture, response);
        sources.record = Box::new(move |_, _| {
            let _ = held.recv_timeout(Duration::from_secs(3));
            finished.send(()).unwrap();
            Err(PromptTrailReason::NoRecord)
        });
        let start = std::time::Instant::now();
        let response = encode(
            "request".into(),
            blocking_trail(Duration::from_millis(100), sources),
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            answered(&response),
            ResponseResult::TurnIndex {
                index: None,
                reason: Some(PromptTrailReason::Unreadable),
                ..
            }
        ));
        assert_eq!(permits().available_permits(), 1);
        release.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    // ---- pane.preface ----

    const PREFACE_SESSION: &str = "0f6e0c2a-5b1d-4c8e-9a7f-3e2d1c0b9a88";

    /// A screen the fake app pages, the way `recent_unwrapped` offset
    /// reads page one: `rows` top first, and lines appended at the bottom
    /// once a given number of reads has been served.
    struct FakeScreen {
        rows: Vec<String>,
        grow: Option<(usize, Vec<String>)>,
    }

    impl FakeScreen {
        fn new(rows: Vec<String>) -> Self {
            Self { rows, grow: None }
        }

        /// The offset a client counts for the row at `index` from the top.
        fn offset_of(&self, index: usize) -> u64 {
            (self.rows.len() - 1 - index) as u64
        }

        /// The newest lines of the screen as one span, from its state now.
        fn span(&self, span: crate::api::schema::ReadSpan) -> crate::api::schema::PaneReadResult {
            let total = self.rows.len();
            let take = total.min(span.max_lines.min(span.min_lines.saturating_add(1)));
            let top = total - take;
            crate::api::schema::PaneReadResult {
                pane_id: "wG4:p1".into(),
                workspace_id: "wG4".into(),
                tab_id: "wG4:t1".into(),
                source: ReadSource::RecentUnwrapped,
                format: ReadFormat::Text,
                text: self.rows[top..].join("\n"),
                revision: 0,
                truncated: top > 0,
                effective_offset: Some(0),
                has_more: Some(top > 0),
            }
        }

        fn page(&self, offset: u64, lines: u32) -> crate::api::schema::PaneReadResult {
            let total = self.rows.len();
            let landed = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(total.saturating_sub(1));
            let bottom = total - landed;
            let top = bottom.saturating_sub(lines.min(1000) as usize);
            crate::api::schema::PaneReadResult {
                pane_id: "wG4:p1".into(),
                workspace_id: "wG4".into(),
                tab_id: "wG4:t1".into(),
                source: ReadSource::RecentUnwrapped,
                format: ReadFormat::Text,
                text: self.rows[top..bottom].join("\n") + "\n",
                revision: 0,
                truncated: top > 0,
                effective_offset: Some(landed as u64),
                has_more: Some(top > 0),
            }
        }
    }

    /// An app that knows one pane and its screen, and says which methods it
    /// was asked.
    fn fake_app(
        pane: PaneInfo,
        screen: FakeScreen,
    ) -> (ApiRequestSender, std::thread::JoinHandle<Vec<String>>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let mut screen = screen;
            let mut asked = Vec::new();
            let mut reads = 0;
            while let Some(message) = rx.blocking_recv() {
                let response = match &message.request.method {
                    Method::PaneGet(_) => {
                        asked.push("pane.get".to_string());
                        pane_response(pane.clone())
                    }
                    Method::PaneRead(params) => {
                        assert_eq!(params.source, ReadSource::RecentUnwrapped);
                        assert_eq!(params.intent, crate::api::schema::ReadIntent::Passive);
                        let read = match params.span {
                            // One span is read from one state of the screen,
                            // as the app reads it under one hold of the
                            // terminal.
                            Some(span) => {
                                asked.push("pane.read:span".to_string());
                                screen.span(span)
                            }
                            None => {
                                asked.push(format!(
                                    "pane.read@{}",
                                    params.offset_from_bottom.expect("an offset read")
                                ));
                                screen.page(
                                    params.offset_from_bottom.unwrap_or(0),
                                    params.lines.unwrap_or(80),
                                )
                            }
                        };
                        reads += 1;
                        if screen
                            .grow
                            .as_ref()
                            .is_some_and(|(after, _)| *after == reads)
                        {
                            let (_, grown) = screen.grow.take().expect("growth");
                            screen.rows.extend(grown);
                        }
                        serde_json::to_string(&SuccessResponse {
                            id: message.request.id.clone(),
                            result: ResponseResult::PaneRead { read },
                        })
                        .expect("a read response")
                    }
                    other => panic!(
                        "the preface asked the app for {}",
                        crate::api::server::api_method_name_for_test(other)
                    ),
                };
                let _ = message.respond_to.send(response);
            }
            asked
        });
        (tx, responder)
    }

    fn claude_pane() -> PaneInfo {
        pane(Some(session(
            "claude",
            AgentSessionRefKind::Id,
            PREFACE_SESSION,
        )))
    }

    fn planted_record(fixture: &Fixture) -> PathBuf {
        fixture.write(
            &format!(".claude/projects/-invented-harbour/{PREFACE_SESSION}.jsonl"),
            include_str!("../preface/fixtures/claude-record.jsonl"),
        )
    }

    /// Rows the way the harness draws a prompt: its glyph, the person's
    /// words wrapped at the harness's own width, continuation indented.
    fn prompt_rows(text: &str, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for line in text.lines() {
            let mut row = String::new();
            for word in line.split(' ') {
                if !row.is_empty() && row.len() + 1 + word.len() > width {
                    rows.push(std::mem::take(&mut row));
                }
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(word);
            }
            rows.push(row);
        }
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| {
                if index == 0 {
                    format!("\u{276f} {row}")
                } else {
                    format!("  {row}")
                }
            })
            .collect()
    }

    fn agent_rows(lines: &[&str]) -> Vec<String> {
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    format!("\u{23fa} {line}")
                } else {
                    format!("  {line}")
                }
            })
            .chain(std::iter::once(String::new()))
            .collect()
    }

    fn work_rows(tool: &str) -> Vec<String> {
        vec![
            format!("\u{23fa} {tool}(invented command)"),
            format!("  \u{23bf}  invented {tool} output"),
            String::new(),
        ]
    }

    /// The rows the fixture record's turn at `index` leaves on a screen.
    fn turn_rows(index: usize) -> Vec<String> {
        let blocks: Vec<Vec<String>> = match index {
            0 => vec![
                prompt_rows("trace why the ferry timetable importer drops the last sailing of the day\nkeep the fix small", 40),
                work_rows("Bash"),
                agent_rows(&["Checking the importer first."]),
                work_rows("Read"),
                agent_rows(&[
                    "The importer drops the last sailing because the window ends at midnight.",
                    "",
                    "| sailing | time |",
                    "|---|---|",
                    "| last | 23:40 |",
                ]),
                agent_rows(&["Fixed by making the window inclusive: https://example.invalid/pull/7"]),
            ],
            1 => vec![
                prompt_rows("/review the harbour schedule parser\nand its tests", 60),
                work_rows("Grep"),
                vec!["  \u{23bf}  Interrupted \u{b7} What should Claude do instead?".into()],
            ],
            2 => vec![
                prompt_rows("summarise the tide tables for the north pier", 60),
                vec!["  \u{23bf}  API Error: invented failure".into(), String::new()],
            ],
            3 => vec![agent_rows(&["The background export finished cleanly."])],
            4 => vec![prompt_rows("now chunk the timetable by pier", 60), work_rows("Edit")],
            5 => vec![
                prompt_rows("also keep the old pier names", 60),
                agent_rows(&["Chunked by pier, and the old pier names are kept."]),
            ],
            6 => vec![agent_rows(&["The export queue is empty."])],
            7 => vec![
                prompt_rows("ship the pier map\nwith the legend", 60),
                agent_rows(&["Section 1. The pier map ships with its legend.", "", "Section 2. And so on."]),
            ],
            8 => vec![prompt_rows("and print it for the harbour office", 60), work_rows("Bash")],
            _ => unreachable!(),
        };
        blocks.concat()
    }

    /// The harness's input box and footer, always at the bottom.
    fn footer_rows() -> Vec<String> {
        vec![
            "\u{2500}".repeat(40),
            "\u{276f} ".into(),
            "\u{2500}".repeat(40),
            "  \u{23f5}\u{23f5} auto mode on".into(),
        ]
    }

    /// A screen holding the rows of the given turns, under some rows of
    /// older tool output the screen kept and no turn to place them in.
    fn screen_of(turns: std::ops::Range<usize>, older_rows: usize) -> Vec<String> {
        (0..older_rows)
            .map(|row| format!("  older tool output {row}"))
            .chain(turns.flat_map(turn_rows))
            .chain(footer_rows())
            .collect()
    }

    fn preface_params(top: Option<(u64, &str)>, max_turns: u32) -> PanePrefaceParams {
        PanePrefaceParams {
            pane_id: "wG4:p1".into(),
            top_offset: top.map(|(offset, _)| offset),
            top_lines: top.map(|(_, line)| vec![line.to_string()]),
            max_turns,
        }
    }

    /// The run a client sends: its first held lines, from `held_from` down,
    /// up to as many as it anchors with.
    fn held_run(rows: &[String], held_from: usize) -> Vec<String> {
        rows[held_from..].iter().take(16).cloned().collect()
    }

    /// One `pane.preface` through the handler, against the fake app.
    fn ask_preface(
        fixture: &Fixture,
        pane: PaneInfo,
        screen: FakeScreen,
        params: &PanePrefaceParams,
    ) -> (ResponseResult, Vec<String>) {
        let (tx, responder) = fake_app(pane, screen);
        let response = respond(
            "req_preface".into(),
            preface_sources(params, &tx, Some(fixture.root.clone())),
        );
        drop(tx);
        let asked = responder.join().expect("the fake app");
        (answered(&response), asked)
    }

    fn preface_of(result: ResponseResult) -> Preface {
        match result {
            ResponseResult::Preface {
                pane_id,
                terminal_id,
                preface,
                reason,
            } => {
                assert_eq!(pane_id, "wG4:p1");
                assert_eq!(terminal_id, "term_beacon");
                assert_eq!(reason, None, "a preface and a reason both answered");
                preface.expect("a preface")
            }
            other => panic!("expected a preface, got: {other:?}"),
        }
    }

    fn preface_reason(result: ResponseResult) -> PrefaceReason {
        match result {
            ResponseResult::Preface {
                preface, reason, ..
            } => {
                assert_eq!(preface, None, "a preface and a reason both answered");
                reason.expect("a reason")
            }
            other => panic!("expected a preface, got: {other:?}"),
        }
    }

    fn prompts_of(preface: &Preface) -> Vec<Option<&str>> {
        preface
            .turns
            .iter()
            .map(|turn| turn.prompt.as_deref())
            .collect()
    }

    /// The row index on `rows` of the first one equal to `row`.
    fn row_index(rows: &[String], row: &str) -> usize {
        rows.iter()
            .position(|candidate| candidate == row)
            .unwrap_or_else(|| panic!("no row {row:?}"))
    }

    #[test]
    fn preface_quotes_every_turn_above_the_oldest_held_prompt_and_a_cut_turn_whole() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        // The screen still has turns 2 to 8; the client holds it from a row
        // of turn 4's tool output down, so turn 4's prompt is above what it
        // holds and turn 5's is the oldest prompt it has.
        let rows = screen_of(2..9, 12);
        let held_from = row_index(&rows, "  \u{23bf}  invented Edit output");
        assert!(held_from > row_index(&rows, "\u{276f} now chunk the timetable by pier"));
        let screen = FakeScreen::new(rows.clone());
        let top = (screen.offset_of(held_from), rows[held_from].as_str());
        let (result, asked) = ask_preface(
            &fixture,
            claude_pane(),
            screen,
            &preface_params(Some(top), 50),
        );
        let preface = preface_of(result);

        assert_eq!(preface.agent, "claude");
        assert_eq!(preface.session_id, PREFACE_SESSION);
        assert!(!preface.revision.is_empty());
        assert_eq!(preface.omitted, 0);
        assert_eq!(
            prompts_of(&preface),
            vec![
                Some("trace why the ferry timetable importer drops the last sailing of the day\nkeep the fix small"),
                Some("/review the harbour schedule parser\nand its tests"),
                Some("summarise the tide tables for the north pier"),
                None,
                Some("now chunk the timetable by pier"),
            ]
        );
        let ends: Vec<_> = preface.turns.iter().map(|turn| turn.end).collect();
        assert_eq!(
            ends,
            vec![
                TurnEnd::Answered,
                TurnEnd::Aborted,
                TurnEnd::Error,
                TurnEnd::Answered,
                // Cut by the seam and quoted whole; the record left it open
                // when a prompt typed mid-run took over, and an older open
                // turn is kept.
                TurnEnd::Open,
            ]
        );
        assert_eq!(preface.turns[3].start, TurnStart::Task);
        assert!(preface.turns[0]
            .answer
            .as_deref()
            .is_some_and(|answer| answer.ends_with("https://example.invalid/pull/7")));
        assert!(preface.turns.windows(2).all(|pair| pair[0].at < pair[1].at));
        // The screen was read once, as one span from one state of it.
        assert_eq!(asked, vec!["pane.get", "pane.read:span"]);
    }

    #[test]
    fn preface_quotes_an_answered_turn_the_seam_cuts_with_its_prompt() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        let rows = screen_of(0..9, 0);
        // The client's oldest line is the middle of turn 0's answer.
        let held_from = row_index(&rows, "  | sailing | time |");
        let screen = FakeScreen::new(rows.clone());
        let top = (screen.offset_of(held_from), rows[held_from].as_str());
        let preface = preface_of(
            ask_preface(
                &fixture,
                claude_pane(),
                screen,
                &preface_params(Some(top), 50),
            )
            .0,
        );
        assert_eq!(preface.turns.len(), 1);
        assert_eq!(
            preface.turns[0].prompt.as_deref(),
            Some("trace why the ferry timetable importer drops the last sailing of the day\nkeep the fix small")
        );
        assert!(preface.turns[0]
            .answer
            .as_deref()
            .is_some_and(|answer| answer.starts_with("The importer drops the last sailing")));
    }

    #[test]
    fn preface_re_anchors_a_top_offset_the_tail_has_grown_past() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        let rows = screen_of(0..8, 0);
        // The client's oldest line is the last line of turn 0's answer, just
        // above turn 1's prompt: it holds turn 1 and everything after.
        let held_from = row_index(
            &rows,
            "\u{23fa} Fixed by making the window inclusive: https://example.invalid/pull/7",
        );
        let offset = FakeScreen::new(rows.clone()).offset_of(held_from);
        let run = held_run(&rows, held_from);
        assert_eq!(run.len(), 16);

        // Since the client read it, the agent printed more: every line the
        // client counted is now further from the bottom.
        let mut grown = rows.clone();
        let footer = grown.split_off(grown.len() - footer_rows().len());
        grown.extend(turn_rows(8));
        grown.extend((0..40).map(|row| format!("  newer tool output {row}")));
        grown.extend(footer);
        let preface = preface_of(
            ask_preface(
                &fixture,
                claude_pane(),
                FakeScreen::new(grown),
                &PanePrefaceParams {
                    top_lines: Some(run),
                    ..preface_params(Some((offset, "")), 50)
                },
            )
            .0,
        );
        // Read at the stale offset, the anchor would land below turn 1's
        // and turn 2's prompts and quote them as though the client had lost
        // them.
        assert_eq!(
            prompts_of(&preface),
            vec![Some("trace why the ferry timetable importer drops the last sailing of the day\nkeep the fix small")]
        );
    }

    #[test]
    fn preface_reads_a_screen_taller_than_a_page_as_one_span_from_one_state() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        // Turns 0 and 1 above two and a half pages of tool output, and the
        // client's oldest line inside it, more than a page up.
        let rows: Vec<String> = (0..2)
            .flat_map(turn_rows)
            .chain((0..2_500).map(|row| format!("  older tool output {row}")))
            .chain((2..9).flat_map(turn_rows))
            .chain(footer_rows())
            .collect();
        let held_from = row_index(&rows, "  older tool output 1200");
        let mut screen = FakeScreen::new(rows.clone());
        let top = (screen.offset_of(held_from), rows[held_from].as_str());
        assert!(top.0 > u64::from(crate::preface::seam::PAGE_LINES));
        // The agent prints right after the first read; a second read would
        // see a screen that moved.
        screen.grow = Some((1, vec!["  a line printed mid-read".into()]));
        let (result, asked) = ask_preface(
            &fixture,
            claude_pane(),
            screen,
            &preface_params(Some(top), 50),
        );
        let preface = preface_of(result);
        assert_eq!(preface.turns.len(), 2);
        assert_eq!(
            preface.turns[1].prompt.as_deref(),
            Some("/review the harbour schedule parser\nand its tests")
        );
        assert_eq!(
            asked,
            vec!["pane.get", "pane.read:span"],
            "the screen is read once, from one state of it"
        );
    }

    fn claude_line(value: serde_json::Value) -> String {
        value.to_string() + "\n"
    }

    /// A record of typed prompts, each answered.
    fn answered_record(prompts: &[&str]) -> String {
        prompts
            .iter()
            .enumerate()
            .map(|(index, prompt)| {
                let at = format!("2026-03-04T09:{index:02}:00Z");
                claude_line(serde_json::json!({
                    "type": "user", "uuid": format!("u{index}"), "timestamp": at,
                    "sessionId": PREFACE_SESSION, "isSidechain": false,
                    "origin": { "kind": "human" },
                    "message": { "role": "user", "content": prompt },
                })) + &claude_line(serde_json::json!({
                    "type": "assistant", "timestamp": at, "isSidechain": false,
                    "message": { "id": format!("m{index}"), "role": "assistant",
                        "content": [{ "type": "text", "text": format!("answer {index}") }],
                        "stop_reason": "end_turn" },
                }))
            })
            .collect()
    }

    /// Pages read across a move can hold one prompt row twice when the line
    /// two pages share is blank either way. Read as one span, the screen
    /// holds it once, and the earlier turn that repeats its words stays in
    /// the preface.
    #[test]
    fn preface_never_loses_a_repeated_prompt_to_a_screen_that_moved_mid_read() {
        let fixture = Fixture::new();
        fixture.write(
            &format!(".claude/projects/-invented-harbour/{PREFACE_SESSION}.jsonl"),
            &answered_record(&["first task", "continue", "continue"]),
        );
        // Blank history with only the newest prompt on it, 998 lines up:
        // two lines printed after a first page would carry it into the
        // second page too.
        let mut rows: Vec<String> = vec![String::new(); 2_500];
        let total = rows.len() + 1;
        rows[total - 1 - 998] = "\u{276f} continue".into();
        rows.push("\u{276f} ".into());
        let mut screen = FakeScreen::new(rows);
        screen.grow = Some((1, vec![String::new(), String::new()]));
        let (result, asked) =
            ask_preface(&fixture, claude_pane(), screen, &preface_params(None, 50));
        let preface = preface_of(result);
        assert_eq!(
            prompts_of(&preface),
            vec![Some("first task"), Some("continue")]
        );
        assert_eq!(preface.omitted, 0);
        assert_eq!(asked, vec!["pane.get", "pane.read:span"]);
    }

    #[test]
    fn preface_with_no_top_offset_takes_the_screens_top_as_the_clients() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        // A screen that forgot everything before turn 5.
        let (result, _) = ask_preface(
            &fixture,
            claude_pane(),
            FakeScreen::new(screen_of(5..9, 3)),
            &preface_params(None, 50),
        );
        let preface = preface_of(result);
        assert_eq!(preface.turns.len(), 5);
        assert_eq!(
            preface.turns[4].prompt.as_deref(),
            Some("now chunk the timetable by pier")
        );
    }

    #[test]
    fn preface_keeps_the_newest_max_turns_prompted_turns_and_counts_the_rest() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        let (result, _) = ask_preface(
            &fixture,
            claude_pane(),
            FakeScreen::new(screen_of(5..9, 0)),
            &preface_params(None, 2),
        );
        let preface = preface_of(result);
        // The two newest prompted turns, and the task turn between them.
        assert_eq!(
            prompts_of(&preface),
            vec![
                Some("summarise the tide tables for the north pier"),
                None,
                Some("now chunk the timetable by pier"),
            ]
        );
        assert_eq!(preface.omitted, 2);
    }

    #[test]
    fn preface_with_nothing_above_the_screen_says_so() {
        let fixture = Fixture::new();
        planted_record(&fixture);
        let (result, _) = ask_preface(
            &fixture,
            claude_pane(),
            FakeScreen::new(screen_of(0..9, 0)),
            &preface_params(None, 50),
        );
        assert_eq!(preface_reason(result), PrefaceReason::NothingAbove);
    }

    #[test]
    fn preface_that_cannot_place_its_seam_answers_none_rather_than_a_guess() {
        let fixture = Fixture::new();
        planted_record(&fixture);

        // The client's oldest line is on no row of this screen.
        let (result, _) = ask_preface(
            &fixture,
            claude_pane(),
            FakeScreen::new(screen_of(3..9, 0)),
            &preface_params(Some((5, "a line this screen never showed")), 50),
        );
        assert_eq!(preface_reason(result), PrefaceReason::SeamNotFound);

        // The line is there, but none of the record's prompts are below it.
        let rows: Vec<String> = (0..30)
            .map(|row| format!("  unrelated output {row}"))
            .collect();
        let screen = FakeScreen::new(rows.clone());
        let top = (screen.offset_of(4), rows[4].as_str());
        let (result, _) = ask_preface(
            &fixture,
            claude_pane(),
            screen,
            &preface_params(Some(top), 50),
        );
        assert_eq!(preface_reason(result), PrefaceReason::SeamNotFound);
    }

    #[test]
    fn preface_reasons_reuse_the_prompt_trails_refusals() {
        let fixture = Fixture::new();
        let screen = || FakeScreen::new(screen_of(5..9, 0));

        assert_eq!(
            preface_reason(
                ask_preface(&fixture, pane(None), screen(), &preface_params(None, 50)).0
            ),
            PrefaceReason::NoSession
        );
        // Harnesses this server has no preface reader for, until it does.
        for (agent, kind, value) in [
            ("codex", AgentSessionRefKind::Id, "0199f3c2b7d84a1e9c05"),
            ("pi", AgentSessionRefKind::Path, "/invented/pi.jsonl"),
            ("droid", AgentSessionRefKind::Id, "droid-session"),
        ] {
            let (result, asked) = ask_preface(
                &fixture,
                pane(Some(session(agent, kind, value))),
                screen(),
                &preface_params(None, 50),
            );
            assert_eq!(
                preface_reason(result),
                PrefaceReason::UnsupportedAgent,
                "{agent}"
            );
            assert_eq!(
                asked,
                vec!["pane.get"],
                "the screen is never read for {agent}"
            );
        }
        assert_eq!(
            preface_reason(
                ask_preface(&fixture, claude_pane(), screen(), &preface_params(None, 50)).0
            ),
            PrefaceReason::NoRecord
        );
        fixture.write(
            &format!(".claude/projects/-invented-harbour/{PREFACE_SESSION}.jsonl"),
            "not a record\n",
        );
        assert_eq!(
            preface_reason(
                ask_preface(&fixture, claude_pane(), screen(), &preface_params(None, 50)).0
            ),
            PrefaceReason::Unreadable
        );
    }

    #[test]
    fn preface_params_are_checked_before_the_app_is_asked() {
        // The receiver is dropped, so any dispatch to the app would fail.
        let (tx, _) = mpsc::unbounded_channel();
        for params in [
            PanePrefaceParams {
                pane_id: " ".into(),
                ..preface_params(None, 50)
            },
            PanePrefaceParams {
                top_lines: Some(vec!["a line".into()]),
                ..preface_params(None, 50)
            },
            PanePrefaceParams {
                top_lines: None,
                ..preface_params(Some((3, "a line")), 50)
            },
            // A run is never empty, and holds at most 64 lines.
            PanePrefaceParams {
                top_lines: Some(Vec::new()),
                ..preface_params(Some((3, "a line")), 50)
            },
            PanePrefaceParams {
                top_lines: Some(vec!["a line".into(); MAX_TOP_LINES + 1]),
                ..preface_params(Some((3, "a line")), 50)
            },
        ] {
            let response = handle_preface("req".into(), &params, &tx);
            let parsed: ErrorResponse = serde_json::from_str(&response)
                .unwrap_or_else(|_| panic!("expected an error response, got: {response}"));
            assert_eq!(parsed.error.code, "invalid_params");
        }
    }

    #[test]
    fn preface_response_is_the_shape_a_client_reads() {
        let fixture = Fixture::new();
        let (tx, responder) = fake_app(pane(None), FakeScreen::new(Vec::new()));
        let response = respond(
            "req_preface".into(),
            preface_sources(&preface_params(None, 50), &tx, Some(fixture.root.clone())),
        );
        drop(tx);
        responder.join().unwrap();
        assert_eq!(
            response,
            r#"{"id":"req_preface","result":{"type":"preface","pane_id":"wG4:p1","terminal_id":"term_beacon","preface":null,"reason":"no_session"}}"#
        );
    }
}
