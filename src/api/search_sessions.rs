//! Serve session search using a data-only active-session snapshot from the app.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use crate::api::schema::{
    Method, Request, ResponseResult, ServerSearchSessionsParams, SuccessResponse,
};
use crate::api::ApiRequestSender;
use crate::session_search::{ActiveSession, Catalog, Desk, SearchResult};

const CONCURRENT_SEARCHES: usize = 2;

struct Sources {
    live: Box<dyn FnOnce() -> String + Send>,
    catalog: Arc<Catalog>,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
}

fn catalog() -> Arc<Catalog> {
    static CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| {
        let catalog = Arc::new(Catalog::new(Desk {
            home: crate::worktree::home_dir(),
            processes: Box::new(crate::platform::session_process_snapshot),
            registry_retry_sleep: Box::new(std::thread::sleep),
        }));
        let worker = Arc::clone(&catalog);
        if let Err(error) = std::thread::Builder::new()
            .name("herdr-session-catalog".into())
            .spawn(move || loop {
                worker.refresh();
                std::thread::sleep(Duration::from_secs(30));
            })
        {
            tracing::warn!(%error, "session catalog worker could not start");
        }
        catalog
    }))
}

pub(super) fn handle_search(
    request_id: String,
    params: &ServerSearchSessionsParams,
    api_tx: &ApiRequestSender,
) -> String {
    let api_tx = api_tx.clone();
    respond(
        request_id,
        params,
        Sources {
            live: Box::new(move || live_sessions(&api_tx)),
            catalog: catalog(),
            now: Arc::new(Instant::now),
        },
    )
}

fn live_sessions(api_tx: &ApiRequestSender) -> String {
    let timeout = Duration::from_millis(500);
    crate::api::server::dispatch_to_app_with_timeout(
        Request {
            id: "internal:search_sessions:panes".into(),
            method: Method::InternalActiveSessions {
                deadline: Instant::now() + timeout,
            },
        },
        api_tx,
        Some(timeout),
    )
}

/// The queue consumer calls this before general API work. Expired requests do
/// no snapshot work; a live request only clones active keys from pure state.
pub(crate) fn handle_active_session_snapshot(
    request: &Request,
    state: &crate::app::AppState,
) -> Option<String> {
    let Method::InternalActiveSessions { deadline } = &request.method else {
        return None;
    };
    let sessions =
        (Instant::now() < *deadline).then(|| crate::session_search::active_sessions(state));
    Some(serde_json::to_string(&sessions).unwrap_or_else(|_| "null".into()))
}

fn respond(request_id: String, params: &ServerSearchSessionsParams, sources: Sources) -> String {
    respond_with_budget(request_id, params, sources, Duration::from_secs(2))
}

fn respond_with_budget(
    request_id: String,
    params: &ServerSearchSessionsParams,
    sources: Sources,
    budget: Duration,
) -> String {
    let found = match blocking_search(params.clone(), sources, budget) {
        Ok(found) => found,
        Err(message) => {
            return crate::api::server::error_response_json(
                request_id,
                "server_unavailable",
                message.into(),
            )
        }
    };
    serde_json::to_string(&SuccessResponse {
        id: request_id.clone(),
        result: ResponseResult::ServerSearchSessions {
            sessions: found.sessions,
            scanned: found.scanned,
            partial: found.partial,
        },
    })
    .unwrap_or_else(|_| {
        crate::api::server::error_response_json(
            request_id,
            "internal_error",
            "failed to encode session search".into(),
        )
    })
}

fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(CONCURRENT_SEARCHES)
                .max_blocking_threads(CONCURRENT_SEARCHES)
                .thread_name("herdr-session-search")
                .enable_all()
                .build()
                .ok()
        })
        .as_ref()
}

fn permits() -> &'static Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(Semaphore::new(CONCURRENT_SEARCHES)))
}

fn blocking_search(
    params: ServerSearchSessionsParams,
    sources: Sources,
    budget: Duration,
) -> Result<SearchResult, &'static str> {
    let runtime = runtime().ok_or("no runtime to search sessions on")?;
    let progress = Arc::new(Mutex::new(SearchResult {
        partial: true,
        ..SearchResult::default()
    }));
    let worker_progress = Arc::clone(&progress);
    let deadline = (sources.now)() + budget;
    runtime.block_on(async {
        let work = async move {
            let permit = Arc::clone(permits())
                .acquire_owned()
                .await
                .map_err(|_| "session search is unavailable")?;
            tokio::task::spawn_blocking(move || {
                // Blocking work keeps its permit after the caller times out.
                let _permit = permit;
                let panes = match serde_json::from_str::<Vec<ActiveSession>>(&(sources.live)()) {
                    Ok(panes) => panes,
                    _ => return Err("could not read live sessions"),
                };
                sources
                    .catalog
                    .search(
                        &params,
                        &panes,
                        deadline,
                        sources.now.as_ref(),
                        &worker_progress,
                    )
                    .map_err(|error| {
                        tracing::warn!(%error, "could not read running sessions");
                        "could not read running sessions"
                    })?;
                Ok(())
            })
            .await
            .map_err(|_| "session search worker failed")?
        };
        match tokio::time::timeout(budget, work).await {
            Ok(result) => result?,
            Err(_) => {
                if let Ok(mut result) = progress.lock() {
                    result.partial = true;
                }
            }
        }
        progress
            .lock()
            .map(|result| result.clone())
            .map_err(|_| "session search is unavailable")
    })
}

#[cfg(test)]
mod tests;
