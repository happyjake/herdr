//! Serve session search on the connection thread, asking the app only for panes.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use crate::api::schema::{
    Method, PaneListParams, Request, ResponseResult, ServerSearchSessionsParams, SuccessResponse,
};
use crate::api::ApiRequestSender;
use crate::session_search::{Catalog, Desk, SearchResult};

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
            live: Box::new(move || {
                crate::api::server::dispatch_to_app_with_timeout(
                    Request {
                        id: "internal:search_sessions:panes".into(),
                        method: Method::PaneList(PaneListParams { workspace_id: None }),
                    },
                    &api_tx,
                    Some(Duration::from_millis(500)),
                )
            }),
            catalog: catalog(),
            now: Arc::new(Instant::now),
        },
    )
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
                let panes = match serde_json::from_str::<SuccessResponse>(&(sources.live)()) {
                    Ok(SuccessResponse {
                        result: ResponseResult::PaneList { panes },
                        ..
                    }) => panes,
                    _ => return Err("could not read live sessions"),
                };
                sources.catalog.search(
                    &params,
                    &panes,
                    deadline,
                    sources.now.as_ref(),
                    &worker_progress,
                );
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
