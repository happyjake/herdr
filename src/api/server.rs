use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use tracing::{debug, error, info, warn};

#[cfg(all(test, unix))]
use std::fs;

use crate::api::schema::{
    ErrorBody, ErrorResponse, Method, Request, ResponseResult, ServerCapabilities, SuccessResponse,
};
use crate::api::subscriptions::ActiveSubscription;
use crate::api::wait::{prompt_agent, wait_for_agent, wait_for_event, wait_for_output};
use crate::api::{request_changes_ui, socket_path, ApiRequestMessage, ApiRequestSender, EventHub};
use crate::ipc::{
    bind_local_listener, is_connection_closed_error, local_stream_peer_closed,
    poll_local_stream_read, remove_socket_file_if_owned, set_local_stream_polling,
    socket_file_identity, LocalStream, LocalStreamRead, SocketFileIdentity,
};

mod pane_graphics_stream;

const SOCKET_PERMISSION_MODE: u32 = 0o600;
pub(super) const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const APP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const MAX_INITIAL_REQUEST_BYTES: usize = 1024 * 1024;

/// One accepted API client connection, independent of how its bytes travel.
///
/// The Unix socket frames messages as JSON lines; the WebSocket transport
/// frames them as one JSON message per text frame. Everything above framing
/// (request dispatch, subscription streaming, wait loops) is shared through
/// this trait.
pub(super) trait ApiTransport {
    /// Write one JSON API message to the client.
    fn write_message(&mut self, message: &str) -> std::io::Result<()>;

    /// Non-blocking pump, called between stream ticks: service whatever the
    /// client sent while a stream was running and report whether the peer is
    /// still there.
    ///
    /// What "whatever the client sent" means is the transport's call. The
    /// Unix socket cannot multiplex — its stream *is* the connection — so any
    /// readable payload counts as the client forfeiting it. A WebSocket is a
    /// message channel, so it answers interleaved requests in place and keeps
    /// the stream alive.
    fn pump_inbound(&mut self) -> std::io::Result<PeerState>;
}

/// Whether a connection's peer is still around after a [`ApiTransport::pump_inbound`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerState {
    Alive,
    Gone,
}

impl ApiTransport for LocalStream {
    fn write_message(&mut self, message: &str) -> std::io::Result<()> {
        write_text_line(self, message)
    }

    fn pump_inbound(&mut self) -> std::io::Result<PeerState> {
        if local_stream_peer_closed(self)? {
            Ok(PeerState::Gone)
        } else {
            Ok(PeerState::Alive)
        }
    }
}

pub struct ServerHandle {
    _thread: std::thread::JoinHandle<()>,
    path: PathBuf,
    identity: SocketFileIdentity,
    running: Arc<AtomicBool>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);

        if let Err(err) = self.remove_socket_file_if_owned() {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %self.path.display(), err = %err, "failed to remove api socket on shutdown");
            }
        }
    }
}

impl ServerHandle {
    pub(crate) fn remove_socket_file_if_owned(&self) -> std::io::Result<()> {
        remove_socket_file_if_owned(&self.path, &self.identity)
    }
}

pub(crate) fn start_server_with_stop_control(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    server_stop: Arc<AtomicBool>,
    server_name: crate::api::SharedServerName,
    server_reach: crate::api::SharedServerReach,
    advertised_endpoint: crate::api::SharedAdvertisedEndpoint,
) -> std::io::Result<ServerHandle> {
    start_server_inner(
        api_tx,
        event_hub,
        crate::api::credentials::process_registry(),
        Some(crate::api::server_capabilities()),
        Some(server_stop),
        server_name,
        server_reach,
        advertised_endpoint,
    )
}

pub fn start_server_with_capabilities(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    credentials: crate::api::SharedCredentialRegistry,
    capabilities: Option<ServerCapabilities>,
    server_name: crate::api::SharedServerName,
    server_reach: crate::api::SharedServerReach,
    advertised_endpoint: crate::api::SharedAdvertisedEndpoint,
) -> std::io::Result<ServerHandle> {
    start_server_inner(
        api_tx,
        event_hub,
        credentials,
        capabilities,
        None,
        server_name,
        server_reach,
        advertised_endpoint,
    )
}

fn start_server_inner(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    credentials: crate::api::SharedCredentialRegistry,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<Arc<AtomicBool>>,
    server_name: crate::api::SharedServerName,
    server_reach: crate::api::SharedServerReach,
    advertised_endpoint: crate::api::SharedAdvertisedEndpoint,
) -> std::io::Result<ServerHandle> {
    let path = socket_path();
    prepare_socket_path(&path)?;

    let listener = bind_local_listener(&path)?;
    restrict_socket_permissions(&path)?;
    let identity = socket_file_identity(&path)?;
    info!(path = %path.display(), "api server listening");
    crate::api::attachment::spawn_ttl_sweeper();

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let api_tx = api_tx.clone();
                    let event_hub = event_hub.clone();
                    let capabilities = capabilities.clone();
                    let server_stop = server_stop.clone();
                    let server_name = server_name.clone();
                    let server_reach = server_reach.clone();
                    let advertised_endpoint = advertised_endpoint.clone();
                    // Owning the socket file is the credential here: the
                    // local caller acts with managing authority unless it
                    // names one of the registry's credentials explicitly.
                    let credentials = crate::api::credentials::CredentialContext::local_socket(
                        credentials.clone(),
                    );
                    let connection_running = Arc::clone(&listener_running);
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection_with_stop(
                            stream,
                            &api_tx,
                            &event_hub,
                            &connection_running,
                            capabilities,
                            server_stop.as_ref(),
                            &server_name,
                            &server_reach,
                            &advertised_endpoint,
                            &credentials,
                        ) {
                            warn!(err = %err, "api connection failed");
                        }
                    });
                }
                Err(err) => {
                    error!(err = %err, "api listener accept failed");
                    break;
                }
            }
        }
        debug!("api server thread exiting");
    });

    Ok(ServerHandle {
        _thread: thread,
        path,
        identity,
        running,
    })
}

fn prepare_socket_path(path: &Path) -> std::io::Result<()> {
    crate::ipc::prepare_socket_path(path, |path| {
        format!(
            "herdr is already running (socket busy at {})",
            path.display()
        )
    })
}

fn restrict_socket_permissions(path: &Path) -> std::io::Result<()> {
    crate::ipc::restrict_socket_permissions(path, SOCKET_PERMISSION_MODE)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn handle_connection(
    stream: LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    advertised_endpoint: &crate::api::SharedAdvertisedEndpoint,
    credentials: &crate::api::credentials::CredentialContext,
) -> std::io::Result<()> {
    handle_connection_with_stop(
        stream,
        api_tx,
        event_hub,
        running,
        capabilities,
        None,
        server_name,
        server_reach,
        advertised_endpoint,
        credentials,
    )
}

fn handle_connection_with_stop(
    mut stream: LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    advertised_endpoint: &crate::api::SharedAdvertisedEndpoint,
    credentials: &crate::api::credentials::CredentialContext,
) -> std::io::Result<()> {
    if let Err(err) = stream.set_send_timeout(Some(STREAM_WRITE_TIMEOUT)) {
        debug!(err = %err, "api connection write timeout unavailable");
    }

    let Some(line) = read_initial_request_line(&mut stream)? else {
        return Ok(());
    };

    let Some(request) = parse_api_request(&mut stream, &line)? else {
        return Ok(());
    };

    // pane.graphics.stream converts this connection into a binary frame
    // channel, so it needs the owned local stream and stays outside the
    // transport-generic dispatch core.
    let method = api_method_name(&request.method);
    let changes_ui = request_changes_ui(&request);
    match request.method {
        Method::PaneGraphicsStream(params) => {
            let request_id = request.id;
            crate::logging::api_request_started(&request_id, method, changes_ui);
            let result =
                pane_graphics_stream::serve(stream, request_id.clone(), params, api_tx, running);
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    "stream_closed",
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
        method_body => handle_parsed_request(
            Request {
                id: request.id,
                method: method_body,
            },
            &mut stream,
            api_tx,
            event_hub,
            running,
            capabilities,
            server_stop,
            server_name,
            server_reach,
            advertised_endpoint,
            credentials,
        ),
    }
}

/// Parse one API request from raw message text. Empty text is skipped
/// silently; malformed JSON gets an `invalid_request` error message written
/// to the client. Both yield `Ok(None)` — how the connection continues
/// afterwards is the transport's call.
pub(super) fn parse_api_request<T: ApiTransport>(
    transport: &mut T,
    text: &str,
) -> std::io::Result<Option<Request>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }

    match serde_json::from_str::<Request>(text) {
        Ok(request) => Ok(Some(request)),
        Err(err) => {
            write_json_message_allow_disconnect(
                transport,
                &ErrorResponse {
                    id: String::new(),
                    error: ErrorBody {
                        code: "invalid_request".into(),
                        message: format!("invalid request: {err}"),
                    },
                },
            )?;
            Ok(None)
        }
    }
}

pub(super) fn handle_parsed_request<T: ApiTransport>(
    request: Request,
    transport: &mut T,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    advertised_endpoint: &crate::api::SharedAdvertisedEndpoint,
    credentials: &crate::api::credentials::CredentialContext,
) -> std::io::Result<()> {
    let request_id = request.id.clone();
    let method = api_method_name(&request.method);
    let changes_ui = request_changes_ui(&request);
    crate::logging::api_request_started(&request_id, method, changes_ui);

    // A credential revoked while this connection was open loses its access
    // here, on its very next request, rather than at its next handshake.
    if let Some(refusal) = credentials.revocation_refusal(&request_id) {
        return write_and_log_response(transport, &refusal, &request_id, method, changes_ui);
    }

    match request.method {
        Method::PaneGraphicsStream(_) => {
            // Served at the connection layer for the local socket; other
            // transports cannot hand their raw stream over for frame data.
            write_and_log_response(
                transport,
                &graphics_stream_unsupported_response(&request_id),
                &request_id,
                method,
                changes_ui,
            )
        }
        Method::EventsSubscribe(params) => {
            let result = stream_subscriptions(
                transport,
                request_id.clone(),
                params,
                api_tx,
                event_hub,
                running,
            );
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    "stream_closed",
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
        Method::EventsWait(params) => {
            let response = wait_for_event(
                request_id.clone(),
                params,
                transport,
                api_tx,
                event_hub,
                running,
            )?;
            finish_wait_response(transport, response, &request_id, method, changes_ui)
        }
        Method::AgentPrompt(params) => {
            let response = prompt_agent(
                request_id.clone(),
                params,
                transport,
                api_tx,
                event_hub,
                running,
            )?;
            finish_wait_response(transport, response, &request_id, method, changes_ui)
        }
        Method::AgentWait(params) => {
            let response = wait_for_agent(
                request_id.clone(),
                params,
                transport,
                api_tx,
                event_hub,
                running,
            )?;
            finish_wait_response(transport, response, &request_id, method, changes_ui)
        }
        Method::PaneWaitForOutput(params) => {
            let response = wait_for_output(request_id.clone(), params, transport, api_tx, running)?;
            finish_wait_response(transport, response, &request_id, method, changes_ui)
        }
        method_body => serve_simple_request(
            Request {
                id: request_id,
                method: method_body,
            },
            transport,
            api_tx,
            capabilities,
            server_stop,
            server_name,
            server_reach,
            advertised_endpoint,
            credentials,
        ),
    }
}

/// Write one response and record how it went. Every non-streaming answer ends
/// here, so a request is never logged as completed without being written.
fn write_and_log_response<T: ApiTransport>(
    transport: &mut T,
    response: &str,
    request_id: &str,
    method: &'static str,
    changes_ui: bool,
) -> std::io::Result<()> {
    let result = write_message_allow_disconnect(transport, response);
    match &result {
        Ok(()) => crate::logging::api_request_completed(
            request_id,
            method,
            api_response_outcome(response),
            changes_ui,
        ),
        Err(err) => crate::logging::api_request_failed(request_id, method, &err.to_string()),
    }
    result
}

fn graphics_stream_unsupported_response(request_id: &str) -> String {
    error_response_json(
        request_id.to_string(),
        "unsupported_transport",
        "pane.graphics.stream requires a dedicated local socket connection".into(),
    )
}

/// Dispatch one non-streaming request and write its response.
fn serve_simple_request<T: ApiTransport>(
    request: Request,
    transport: &mut T,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    advertised_endpoint: &crate::api::SharedAdvertisedEndpoint,
    credentials: &crate::api::credentials::CredentialContext,
) -> std::io::Result<()> {
    let request_id = request.id.clone();
    let method = api_method_name(&request.method);
    let changes_ui = request_changes_ui(&request);
    let (response_write_tx, response_write_rx) = std::sync::mpsc::channel();
    let response = handle_request(
        request,
        api_tx,
        capabilities,
        server_stop,
        Some(response_write_rx),
        server_name,
        server_reach,
        advertised_endpoint,
        credentials,
    );
    let result = write_and_log_response(transport, &response, &request_id, method, changes_ui);
    let _ = response_write_tx.send(());
    result
}

/// Serve a request that arrived while this connection was already streaming.
///
/// Only transports that can multiplex call this. The connection thread is
/// committed to the stream it is already running, so a request that would
/// start a second stream is refused out loud rather than queued or dropped —
/// silence is what made a swallowed request indistinguishable from a healthy
/// idle connection.
pub(super) fn serve_interleaved_request<T: ApiTransport>(
    transport: &mut T,
    text: &str,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    advertised_endpoint: &crate::api::SharedAdvertisedEndpoint,
    credentials: &crate::api::credentials::CredentialContext,
) -> std::io::Result<()> {
    let Some(request) = parse_api_request(transport, text)? else {
        return Ok(());
    };

    let request_id = request.id.clone();
    let method = api_method_name(&request.method);
    let changes_ui = request_changes_ui(&request);
    crate::logging::api_request_started(&request_id, method, changes_ui);

    if let Some(refusal) = credentials.revocation_refusal(&request_id) {
        return write_and_log_response(transport, &refusal, &request_id, method, changes_ui);
    }

    // A transport that multiplexes is by definition not a dedicated local
    // socket, so this answer matches what the same request gets between
    // requests rather than inventing a timing-dependent second one.
    if matches!(request.method, Method::PaneGraphicsStream(_)) {
        let response = graphics_stream_unsupported_response(&request_id);
        return write_and_log_response(transport, &response, &request_id, method, changes_ui);
    }

    if method_starts_a_stream(&request.method) {
        let response = error_response_json(
            request_id.clone(),
            "stream_busy",
            format!(
                "{method} needs its own connection; this connection is already serving a stream"
            ),
        );
        return write_and_log_response(transport, &response, &request_id, method, changes_ui);
    }

    serve_simple_request(
        request,
        transport,
        api_tx,
        capabilities,
        server_stop,
        server_name,
        server_reach,
        advertised_endpoint,
        credentials,
    )
}

/// Methods that take over the connection until the client goes away, and so
/// cannot be served alongside a stream that is already running on it.
fn method_starts_a_stream(method: &Method) -> bool {
    matches!(
        method,
        Method::EventsSubscribe(_)
            | Method::EventsWait(_)
            | Method::AgentPrompt(_)
            | Method::AgentWait(_)
            | Method::PaneWaitForOutput(_)
            | Method::PaneGraphicsStream(_)
    )
}

fn finish_wait_response<T: ApiTransport>(
    transport: &mut T,
    response: Option<String>,
    request_id: &str,
    method: &'static str,
    changes_ui: bool,
) -> std::io::Result<()> {
    let Some(response) = response else {
        crate::logging::api_request_completed(
            request_id,
            method,
            "client_disconnected",
            changes_ui,
        );
        return Ok(());
    };
    let result = write_message_allow_disconnect(transport, &response);
    match &result {
        Ok(()) => crate::logging::api_request_completed(
            request_id,
            method,
            api_response_outcome(&response),
            changes_ui,
        ),
        Err(err) => crate::logging::api_request_failed(request_id, method, &err.to_string()),
    }
    result
}

fn handle_request(
    request: Request,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    advertised_endpoint: &crate::api::SharedAdvertisedEndpoint,
    credentials: &crate::api::credentials::CredentialContext,
) -> String {
    // Declarations and runtime facts are read per request, so config
    // reloads and the running process are reflected on every transport.
    if matches!(&request.method, Method::Ping(_)) {
        return serde_json::to_string(&SuccessResponse {
            id: request.id,
            result: ResponseResult::Pong {
                version: crate::build_info::version(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities,
                name: Some(server_name.current()),
                reach: server_reach.current(),
                advertised_endpoint: advertised_endpoint.current(),
                session: crate::session::active_name_for_api_socket(),
                exe: crate::api::server_executable::path_for_pong(),
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
                .to_string()
        });
    }

    if matches!(&request.method, Method::ServerStop(_)) {
        if let Some(server_stop) = server_stop {
            server_stop.store(true, Ordering::Release);
            return serde_json::to_string(&SuccessResponse {
                id: request.id,
                result: ResponseResult::Ok {},
            })
            .unwrap_or_else(|_| "{}".to_string());
        }
    } else if server_stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
        return error_response_json(
            request.id,
            "server_unavailable",
            "server is shutting down".into(),
        );
    }

    match request.method {
        // Pure file I/O on the connection thread: no app state involved, so
        // the method behaves identically over every transport and mode.
        Method::AttachmentCreate(params) => {
            crate::api::attachment::handle_create(request.id, &params)
        }
        // The credential registry is runtime state beside the session, not
        // app state, so it is served here on the connection thread — the one
        // place both transports pass through, which is what makes the verbs
        // identical over the Unix socket and the WebSocket.
        Method::CredentialMint(params) => credentials.serve_mint(request.id, &params),
        Method::CredentialList(params) => {
            credentials.serve_list(request.id, params.acting_token.as_deref())
        }
        Method::CredentialRevoke(params) => credentials.serve_revoke(request.id, &params),
        Method::CredentialRevokeAll(params) => {
            credentials.serve_revoke_all(request.id, params.acting_token.as_deref())
        }
        method => dispatch_to_app(
            Request {
                id: request.id,
                method,
            },
            api_tx,
            None,
            response_write_complete,
            None,
        ),
    }
}

fn api_method_name(method: &Method) -> &'static str {
    match method {
        Method::Ping(_) => "ping",
        Method::ServerStop(_) => "server.stop",
        Method::ServerLiveHandoff(_) => "server.live_handoff",
        Method::ServerReloadConfig(_) => "server.reload_config",
        Method::ServerAgentManifests(_) => "server.agent_manifests",
        Method::ServerReloadAgentManifests(_) => "server.reload_agent_manifests",
        Method::AttachmentCreate(_) => "attachment.create",
        Method::CredentialMint(_) => "credential.mint",
        Method::CredentialList(_) => "credential.list",
        Method::CredentialRevoke(_) => "credential.revoke",
        Method::CredentialRevokeAll(_) => "credential.revoke_all",
        Method::NotificationShow(_) => "notification.show",
        Method::ClientWindowTitleSet(_) => "client.window_title.set",
        Method::ClientWindowTitleClear(_) => "client.window_title.clear",
        Method::SessionSnapshot(_) => "session.snapshot",
        Method::WorkspaceCreate(_) => "workspace.create",
        Method::WorkspaceList(_) => "workspace.list",
        Method::WorkspaceGet(_) => "workspace.get",
        Method::WorkspaceFocus(_) => "workspace.focus",
        Method::WorkspaceRename(_) => "workspace.rename",
        Method::WorkspaceMove(_) => "workspace.move",
        Method::WorkspaceMoveBlock(_) => "workspace.move_block",
        Method::WorkspaceReportMetadata(_) => "workspace.report_metadata",
        Method::WorkspaceClose(_) => "workspace.close",
        Method::WorktreeList(_) => "worktree.list",
        Method::WorktreeCreate(_) => "worktree.create",
        Method::WorktreeOpen(_) => "worktree.open",
        Method::WorktreeRemove(_) => "worktree.remove",
        Method::TabCreate(_) => "tab.create",
        Method::TabList(_) => "tab.list",
        Method::TabGet(_) => "tab.get",
        Method::TabFocus(_) => "tab.focus",
        Method::TabRename(_) => "tab.rename",
        Method::TabMove(_) => "tab.move",
        Method::TabClose(_) => "tab.close",
        Method::AgentList(_) => "agent.list",
        Method::AgentGet(_) => "agent.get",
        Method::AgentRead(_) => "agent.read",
        Method::AgentExplain(_) => "agent.explain",
        Method::AgentSendKeys(_) => "agent.send_keys",
        Method::AgentRename(_) => "agent.rename",
        Method::AgentViewSet(_) => "agent.view.set",
        Method::AgentViewClear(_) => "agent.view.clear",
        Method::AgentFocus(_) => "agent.focus",
        Method::AgentStart(_) => "agent.start",
        Method::AgentPrompt(_) => "agent.prompt",
        Method::AgentWait(_) => "agent.wait",
        Method::PaneSplit(_) => "pane.split",
        Method::PaneSwap(_) => "pane.swap",
        Method::PaneMove(_) => "pane.move",
        Method::PaneZoom(_) => "pane.zoom",
        Method::PaneLayout(_) => "pane.layout",
        Method::PaneProcessInfo(_) => "pane.process_info",
        Method::LayoutExport(_) => "layout.export",
        Method::LayoutApply(_) => "layout.apply",
        Method::LayoutSetSplitRatio(_) => "layout.set_split_ratio",
        Method::PaneNeighbor(_) => "pane.neighbor",
        Method::PaneEdges(_) => "pane.edges",
        Method::PaneFocusDirection(_) => "pane.focus_direction",
        Method::PaneResize(_) => "pane.resize",
        Method::PaneList(_) => "pane.list",
        Method::PaneCurrent(_) => "pane.current",
        Method::PaneGet(_) => "pane.get",
        Method::PaneFocus(_) => "pane.focus",
        Method::PaneInputSet(_) => "pane.input.set",
        Method::PaneRename(_) => "pane.rename",
        Method::PaneSendText(_) => "pane.send_text",
        Method::PaneSendKeys(_) => "pane.send_keys",
        Method::PaneSendInput(_) => "pane.send_input",
        Method::PaneSendMouse(_) => "pane.send_mouse",
        Method::PaneRead(_) => "pane.read",
        Method::PaneGraphicsSet(_) => "pane.graphics.set",
        Method::PaneGraphicsClear(_) => "pane.graphics.clear",
        Method::PaneGraphicsInfo(_) => "pane.graphics.info",
        Method::PaneGraphicsStream(_) => "pane.graphics.stream",
        Method::PaneGraphicsStreamSet(_) => "pane.graphics.stream.set",
        Method::PaneGraphicsStreamDirect(_) => "pane.graphics.stream.direct",
        Method::PaneGraphicsStreamOpen(_) => "pane.graphics.stream.open",
        Method::PaneGraphicsStreamClose(_) => "pane.graphics.stream.close",
        Method::PaneReportAgent(_) => "pane.report_agent",
        Method::PaneReportAgentSession(_) => "pane.report_agent_session",
        Method::PaneReportMetadata(_) => "pane.report_metadata",
        Method::PaneClearAgentAuthority(_) => "pane.clear_agent_authority",
        Method::PaneReleaseAgent(_) => "pane.release_agent",
        Method::PaneClose(_) => "pane.close",
        Method::PopupClose(_) => "popup.close",
        Method::EventsSubscribe(_) => "events.subscribe",
        Method::EventsWait(_) => "events.wait",
        Method::PaneWaitForOutput(_) => "pane.wait_for_output",
        Method::IntegrationInstall(_) => "integration.install",
        Method::IntegrationUninstall(_) => "integration.uninstall",
        Method::PluginLink(_) => "plugin.link",
        Method::PluginList(_) => "plugin.list",
        Method::PluginUnlink(_) => "plugin.unlink",
        Method::PluginEnable(_) => "plugin.enable",
        Method::PluginDisable(_) => "plugin.disable",
        Method::PluginActionList(_) => "plugin.action.list",
        Method::PluginActionInvoke(_) => "plugin.action.invoke",
        Method::PluginLogList(_) => "plugin.log.list",
        Method::PluginPaneOpen(_) => "plugin.pane.open",
        Method::PluginPaneFocus(_) => "plugin.pane.focus",
        Method::PluginPaneClose(_) => "plugin.pane.close",
    }
}

fn api_response_outcome(response: &str) -> &'static str {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return "error";
    };

    match value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(|code| code.as_str())
    {
        Some("timeout") => "timeout",
        Some(_) => "error",
        None => "ok",
    }
}

fn read_initial_request_line(stream: &mut LocalStream) -> std::io::Result<Option<String>> {
    read_initial_request_line_with_timeout(stream, INITIAL_REQUEST_TIMEOUT)
}

fn read_initial_request_line_with_timeout(
    stream: &mut LocalStream,
    timeout: Duration,
) -> std::io::Result<Option<String>> {
    read_initial_request_line_with_limits(stream, timeout, MAX_INITIAL_REQUEST_BYTES)
}

fn read_initial_request_line_with_limits(
    stream: &mut LocalStream,
    timeout: Duration,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    set_local_stream_polling(stream, true)?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];

    let result = loop {
        let read = match poll_local_stream_read(stream, &mut byte) {
            Ok(read) => read,
            Err(err) => break Err(err),
        };
        match read {
            LocalStreamRead::Closed => break Ok(None),
            LocalStreamRead::Data => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    break String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
                }
                if bytes.len() > max_bytes {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "api request line is too large",
                    ));
                }
            }
            LocalStreamRead::Pending => {
                if Instant::now() >= deadline {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out reading api request",
                    ));
                }
                std::thread::sleep(CONNECTION_POLL_INTERVAL);
            }
        }
    };
    set_local_stream_polling(stream, false)?;
    result
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc::{self, Receiver};

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "herdr-api-{name}-{}-{}.sock",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, path)
    }

    fn spawn_connection(
        server: LocalStream,
    ) -> (Receiver<std::io::Result<()>>, std::thread::JoinHandle<()>) {
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
            let result = handle_connection(
                server,
                &api_tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
            );
            done_tx.send(result).unwrap();
        });
        (done_rx, thread)
    }

    #[test]
    fn windows_delayed_partial_initial_request_returns_pong() {
        let (mut client, server, path) = local_stream_pair("delayed-request");
        let (done_rx, server_thread) = spawn_connection(server);

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            done_rx.try_recv().is_err(),
            "idle connected client must not be treated as closed"
        );

        client
            .write_all(br#"{"id":"delayed","method":"ping","params":{}}"#)
            .unwrap();
        client.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            done_rx.try_recv().is_err(),
            "partial request must wait for its newline"
        );
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let mut response = String::new();
        BufReader::new(&mut client)
            .read_line(&mut response)
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "delayed");
        assert_eq!(response["result"]["type"], "pong");

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_disconnected_initial_request_returns_promptly() {
        let (client, server, path) = local_stream_pair("disconnected-request");
        let (done_rx, server_thread) = spawn_connection(server);

        drop(client);

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnected connection handler must finish promptly")
            .unwrap();
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_idle_initial_request_honors_timeout() {
        let (_client, mut server, path) = local_stream_pair("request-timeout");

        let err = read_initial_request_line_with_timeout(&mut server, Duration::from_millis(50))
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_initial_request_enforces_size_limit() {
        let (mut client, mut server, path) = local_stream_pair("request-size-limit");
        client.write_all(b"12345").unwrap();
        client.flush().unwrap();

        let err = read_initial_request_line_with_limits(&mut server, Duration::from_secs(1), 4)
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "api request line is too large");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_initial_request_rejects_invalid_utf8() {
        let (mut client, mut server, path) = local_stream_pair("request-invalid-utf8");
        client.write_all(&[0xff, b'\n']).unwrap();
        client.flush().unwrap();

        let err = read_initial_request_line_with_timeout(&mut server, Duration::from_secs(1))
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(path);
    }
}

fn stream_subscriptions<T: ApiTransport>(
    transport: &mut T,
    request_id: String,
    params: crate::api::schema::EventsSubscribeParams,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    // Every subscription starts from the live sequence (upstream #1270).
    // `params.live_only` remains an accepted request field: clients that
    // opted out of ring replay when replay existed now get this default,
    // and the flag must keep parsing so their subscribes keep working.
    let event_start_sequence = event_hub.current_sequence();
    let mut subscriptions = Vec::with_capacity(params.subscriptions.len());
    let _ = params.live_only;
    for (index, subscription) in params.subscriptions.into_iter().enumerate() {
        let active = match ActiveSubscription::new(
            subscription,
            &request_id,
            index,
            api_tx,
            event_hub,
            event_start_sequence,
        ) {
            Ok(active) => active,
            Err(response) => {
                if let Err(err) = write_json_message(transport, &response) {
                    if is_connection_closed_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
                return Ok(());
            }
        };
        subscriptions.push(active);
    }

    if let Err(err) = write_json_message(
        transport,
        &SuccessResponse {
            id: request_id,
            result: ResponseResult::SubscriptionStarted {},
        },
    ) {
        if is_connection_closed_error(&err) {
            return Ok(());
        }
        return Err(err);
    }

    loop {
        if should_stop_connection(transport, running)? {
            return Ok(());
        }

        for subscription in &mut subscriptions {
            if let Some(event) = subscription.poll(api_tx, event_hub) {
                if let Err(err) = write_json_message(transport, &event) {
                    if is_connection_closed_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
            }
        }
        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

fn write_text_line(stream: &mut LocalStream, value: &str) -> std::io::Result<()> {
    stream.write_all(value.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn write_message_allow_disconnect<T: ApiTransport>(
    transport: &mut T,
    value: &str,
) -> std::io::Result<()> {
    match transport.write_message(value) {
        Err(err) if is_connection_closed_error(&err) => Ok(()),
        result => result,
    }
}

fn write_json_message<T: ApiTransport, V: serde::Serialize>(
    transport: &mut T,
    value: &V,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    transport.write_message(&encoded)
}

fn write_json_message_allow_disconnect<T: ApiTransport, V: serde::Serialize>(
    transport: &mut T,
    value: &V,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_message_allow_disconnect(transport, &encoded)
}

/// One tick of a streaming loop's connection housekeeping.
///
/// Not a pure predicate: this also services whatever the client sent while
/// the stream was running, which on a multiplexing transport means answering
/// interleaved requests. The name is kept for its call sites, which read as
/// the stop check they gate.
pub(super) fn should_stop_connection<T: ApiTransport>(
    transport: &mut T,
    running: &Arc<AtomicBool>,
) -> std::io::Result<bool> {
    if !running.load(Ordering::Relaxed) {
        return Ok(true);
    }

    Ok(transport.pump_inbound()? == PeerState::Gone)
}

pub(super) fn dispatch_to_app_with_timeout(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
) -> String {
    dispatch_to_app(request, api_tx, timeout, None, None)
}

pub(super) fn dispatch_stream_open(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Duration,
    active: Arc<AtomicBool>,
) -> String {
    dispatch_to_app(request, api_tx, Some(timeout), None, Some(active))
}

pub(super) fn dispatch_stream_frame(
    request: Request,
    api_tx: &ApiRequestSender,
    active: Arc<AtomicBool>,
) -> String {
    dispatch_to_app(
        request,
        api_tx,
        Some(crate::app::pane_graphics::DIRECT_OUTER_TIMEOUT),
        None,
        Some(active),
    )
}

fn dispatch_to_app(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    stream_active: Option<Arc<AtomicBool>>,
) -> String {
    let request_id = request.id.clone();
    let request_active = stream_active.clone();
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    if let Err(err) = api_tx.send(ApiRequestMessage {
        request,
        respond_to,
        response_write_complete,
        stream_active,
    }) {
        if let Some(active) = request_active {
            active.store(false, Ordering::Release);
        }
        return error_response_json(
            request_id,
            "server_unavailable",
            format!("failed to dispatch request: {err}"),
        );
    }

    let response = match timeout {
        Some(timeout) => response_rx.recv_timeout(timeout).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for app response after {} ms",
                    timeout.as_millis()
                ),
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "app response channel closed",
            ),
        }),
        None => response_rx
            .recv()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err)),
    };

    match response {
        Ok(response) => response,
        Err(err) => {
            if let Some(active) = request_active {
                active.store(false, Ordering::Release);
            }
            error_response_json(
                request_id,
                "server_unavailable",
                format!("request handling failed: {err}"),
            )
        }
    }
}

pub(super) fn error_response_json(id: String, code: &str, message: String) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"failed to encode error response"}}"#
            .to_string()
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::mpsc;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn undeclared_server_reach() -> crate::api::SharedServerReach {
        crate::api::SharedServerReach::from_config(&crate::config::WebSocketApiConfig::default())
    }

    fn undeclared_advertised_endpoint() -> crate::api::SharedAdvertisedEndpoint {
        crate::api::SharedAdvertisedEndpoint::from_config(
            &crate::config::WebSocketApiConfig::default(),
        )
    }

    /// A local-socket caller against a registry of its own, so credential
    /// state never leaks between tests or into the developer's session dir.
    fn test_credentials() -> crate::api::credentials::CredentialContext {
        crate::api::credentials::CredentialContext::local_socket(
            crate::api::credentials::SharedCredentialRegistry::open(
                crate::api::credentials::test_registry_path("socket"),
            ),
        )
    }

    fn unique_test_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // sun_path caps unix socket paths at ~104 bytes and the macOS temp
        // dir already spends half of that, so keep the unique suffix to the
        // sub-second nanos instead of the full 19-digit timestamp.
        let suffix = nanos % 1_000_000_000;
        std::env::temp_dir().join(format!("herdr-{name}-{}-{suffix}", std::process::id()))
    }

    fn read_line(stream: &mut LocalStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, PathBuf) {
        let path = unique_test_path(name);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, path)
    }

    /// The websocket transport learned to serve requests that arrive while a
    /// stream runs. The Unix socket must not: its stream *is* the connection,
    /// there is no framing to multiplex over, so a client that keeps writing
    /// after starting a stream still forfeits it.
    #[test]
    fn unix_stream_still_forfeits_the_connection_on_payload_during_a_stream() {
        let (mut client, mut server, path) = local_stream_pair("pump-forfeit");

        assert_eq!(server.pump_inbound().unwrap(), PeerState::Alive);

        client
            .write_all(b"{\"id\":\"late\",\"method\":\"ping\"}\n")
            .unwrap();
        client.flush().unwrap();

        let mut forfeited = false;
        for _ in 0..50 {
            if server.pump_inbound().unwrap() == PeerState::Gone {
                forfeited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(forfeited, "payload mid-stream must end the unix connection");

        let _ = std::fs::remove_file(path);
    }

    fn pane_info(
        pane_id: &str,
        agent_status: crate::api::schema::AgentStatus,
    ) -> crate::api::schema::PaneInfo {
        crate::api::schema::PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: "term_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: Some("pi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll: None,
            mouse_tracking: false,
            alternate_screen: false,
            agent_status_changed_at: Some(1_700_000_000),
            revision: 0,
        }
    }

    fn spawn_pane_get_responder(
        agent_status: crate::api::schema::AgentStatus,
    ) -> (ApiRequestSender, std::thread::JoinHandle<()>) {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            while let Some(msg) = api_rx.blocking_recv() {
                match msg.request.method {
                    Method::PaneGet(_) => msg
                        .respond_to
                        .send(
                            serde_json::to_string(&SuccessResponse {
                                id: msg.request.id,
                                result: ResponseResult::PaneInfo {
                                    pane: pane_info("pane_1", agent_status),
                                },
                            })
                            .unwrap(),
                        )
                        .unwrap(),
                    Method::EventsWait(_) => msg
                        .respond_to
                        .send(error_response_json(
                            msg.request.id,
                            "unexpected_dispatch",
                            "events.wait should be handled by the api server".into(),
                        ))
                        .unwrap(),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        });
        (api_tx, responder)
    }

    fn workspace_created_event(workspace_id: &str) -> crate::api::schema::EventEnvelope {
        crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceCreated,
            data: crate::api::schema::EventData::WorkspaceCreated {
                workspace: crate::api::schema::WorkspaceInfo {
                    workspace_id: workspace_id.into(),
                    number: 1,
                    label: workspace_id.into(),
                    focused: false,
                    pane_count: 0,
                    tab_count: 0,
                    active_tab_id: String::new(),
                    agent_status: crate::api::schema::AgentStatus::Unknown,
                    tokens: Default::default(),
                    worktree: None,
                },
            },
        }
    }

    fn layout_updated_event(tab_id: &str) -> crate::api::schema::EventEnvelope {
        crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::LayoutUpdated,
            data: crate::api::schema::EventData::LayoutUpdated {
                layout: crate::api::schema::PaneLayoutSnapshot {
                    workspace_id: "ws_1".into(),
                    tab_id: tab_id.into(),
                    zoomed: false,
                    area: crate::api::schema::PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    focused_pane_id: "pane_1".into(),
                    panes: Vec::new(),
                    splits: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn socket_path_prefers_explicit_env_override() {
        let _guard = env_lock().lock().unwrap();
        let unique = format!("/tmp/herdr-test-{}.sock", std::process::id());
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &unique);
        assert_eq!(socket_path(), PathBuf::from(&unique));
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn socket_path_defaults_to_config_dir_even_when_xdg_runtime_dir_is_set() {
        let _guard = env_lock().lock().unwrap();
        let config_home = unique_test_path("socket-default-config-home");
        let runtime_dir = unique_test_path("socket-default-runtime");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);

        let expected = config_home
            .join(crate::config::app_dir_name())
            .join("herdr.sock");
        assert_eq!(socket_path(), expected);

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    #[test]
    fn socket_path_uses_named_session_dir() {
        let _guard = env_lock().lock().unwrap();
        let config_home = unique_test_path("socket-named-config-home");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var(crate::session::SESSION_ENV_VAR, "work");
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        let expected = config_home
            .join(crate::config::app_dir_name())
            .join("sessions")
            .join("work")
            .join("herdr.sock");
        assert_eq!(socket_path(), expected);

        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn restrict_socket_permissions_sets_user_only_mode() {
        let dir = unique_test_path("socket-perms");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        restrict_socket_permissions(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_PERMISSION_MODE);

        drop(_listener);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_response_outcome_uses_top_level_error_shape() {
        let ok_with_error_text = r#"{"id":"req","result":{"read":{"text":"user said \"error\": \"timeout\"","revision":1}}}"#;
        assert_eq!(api_response_outcome(ok_with_error_text), "ok");

        let timeout = r#"{"id":"req","error":{"code":"timeout","message":"timed out waiting for output match"}}"#;
        assert_eq!(api_response_outcome(timeout), "timeout");

        let generic_error =
            r#"{"id":"req","error":{"code":"server_unavailable","message":"boom"}}"#;
        assert_eq!(api_response_outcome(generic_error), "error");
    }

    #[test]
    fn ping_request_returns_pong_with_the_declared_server_name() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let response = handle_request(
            Request {
                id: "req_1".into(),
                method: Method::Ping(crate::api::schema::PingParams::default()),
            },
            &tx,
            Some(ServerCapabilities {
                live_handoff: true,
                detached_server_daemon: true,
                send_affirm: true,
                stream_multiplex: true,
                credential_registry: true,
            }),
            None,
            None,
            &crate::api::SharedServerName::new("the-mini".to_string()),
            &undeclared_server_reach(),
            &undeclared_advertised_endpoint(),
            &test_credentials(),
        );

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.id, "req_1");
        match parsed.result {
            ResponseResult::Pong { name, .. } => assert_eq!(name.as_deref(), Some("the-mini")),
            other => panic!("expected pong, got {other:?}"),
        }
    }

    #[test]
    fn pong_reflects_a_renamed_server_without_restarting_anything() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let server_name = crate::api::SharedServerName::new("before".to_string());
        let server_reach = undeclared_server_reach();
        let advertised_endpoint = undeclared_advertised_endpoint();
        let ping = |id: &str| {
            handle_request(
                Request {
                    id: id.into(),
                    method: Method::Ping(crate::api::schema::PingParams::default()),
                },
                &tx,
                None,
                None,
                None,
                &server_name,
                &server_reach,
                &advertised_endpoint,
                &test_credentials(),
            )
        };

        assert!(ping("req_before").contains(r#""name":"before""#));

        // The same reload path token rotation uses (see app config reload).
        let changed = server_name.apply_reloaded_config(&crate::config::WebSocketApiConfig {
            name: Some("after".to_string()),
            ..crate::config::WebSocketApiConfig::default()
        });
        assert!(changed);

        assert!(ping("req_after").contains(r#""name":"after""#));
    }

    #[test]
    fn server_stop_control_bypasses_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server_name = crate::api::SharedServerName::new("test".to_string());
        let server_reach = undeclared_server_reach();
        let advertised_endpoint = undeclared_advertised_endpoint();
        let credentials = test_credentials();
        let response = handle_request(
            Request {
                id: "priority_stop".into(),
                method: Method::ServerStop(crate::api::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
            None,
            &server_name,
            &server_reach,
            &advertised_endpoint,
            &credentials,
        );

        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "priority_stop");
        assert_eq!(response["result"]["type"], "ok");
        assert!(stop.load(Ordering::Acquire));

        let rejected = handle_request(
            Request {
                id: "after_stop".into(),
                method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
            None,
            &server_name,
            &server_reach,
            &advertised_endpoint,
            &credentials,
        );
        let rejected: serde_json::Value = serde_json::from_str(&rejected).unwrap();
        assert_eq!(rejected["error"]["code"], "server_unavailable");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pong_publishes_the_advertised_endpoint_only_once_one_is_declared() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let server_name = crate::api::SharedServerName::new("the-mini".to_string());
        let server_reach = undeclared_server_reach();
        let advertised_endpoint = undeclared_advertised_endpoint();
        let ping = |id: &str| {
            handle_request(
                Request {
                    id: id.into(),
                    method: Method::Ping(crate::api::schema::PingParams::default()),
                },
                &tx,
                None,
                None,
                None,
                &server_name,
                &server_reach,
                &advertised_endpoint,
                &test_credentials(),
            )
        };

        // Nothing declared: the field is absent from the message, rather than
        // present and empty or synthesized from wherever the request arrived.
        let undeclared = ping("req_undeclared");
        assert!(
            !undeclared.contains("advertised_endpoint"),
            "a server that declares no endpoint must publish no field: {undeclared}"
        );

        // Declared: exactly the canonical form a pairing payload carries, so
        // a client reading either one dials the same url.
        let changed =
            advertised_endpoint.apply_reloaded_config(&crate::config::WebSocketApiConfig {
                advertised_endpoint: Some("  WSS://a-host.example.net:8443/  ".to_string()),
                ..crate::config::WebSocketApiConfig::default()
            });
        assert!(changed);

        let declared = ping("req_declared");
        assert!(
            declared.contains(r#""advertised_endpoint":"wss://a-host.example.net:8443""#),
            "{declared}"
        );

        // The addition is additive: the fields a client already reads keep
        // their names, their values, and the protocol version.
        assert!(declared.contains(r#""type":"pong""#), "{declared}");
        assert!(declared.contains(r#""name":"the-mini""#), "{declared}");
        assert!(
            declared.contains(&format!(
                r#""protocol":{}"#,
                crate::protocol::PROTOCOL_VERSION
            )),
            "{declared}"
        );

        // Withdrawn again by the same reload path, and the field goes with it.
        assert!(advertised_endpoint
            .apply_reloaded_config(&crate::config::WebSocketApiConfig::default()));
        let withdrawn = ping("req_withdrawn");
        assert!(!withdrawn.contains("advertised_endpoint"), "{withdrawn}");
    }

    #[test]
    fn attachment_create_is_answered_in_process_with_distinct_error_codes() {
        use base64::Engine as _;

        // The channel receiver is dropped up front: if attachment.create ever
        // dispatched to the app, handle_request would fail, so these
        // assertions also pin that the method is served on the connection
        // thread — identically for every transport and server mode.
        let (tx, _) = mpsc::unbounded_channel();
        let server_name = crate::api::SharedServerName::new("test".to_string());
        let server_reach = undeclared_server_reach();
        let advertised_endpoint = undeclared_advertised_endpoint();
        let create = |id: &str, bytes_b64: String| {
            handle_request(
                Request {
                    id: id.into(),
                    method: Method::AttachmentCreate(crate::api::schema::AttachmentCreateParams {
                        bytes_b64,
                    }),
                },
                &tx,
                None,
                None,
                None,
                &server_name,
                &server_reach,
                &advertised_endpoint,
                &test_credentials(),
            )
        };
        let error_code = |response: &str| {
            serde_json::from_str::<ErrorResponse>(response)
                .map(|parsed| parsed.error.code)
                .unwrap_or_else(|_| panic!("expected error response, got: {response}"))
        };

        let mut oversize = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        oversize.resize(crate::api::attachment::MAX_ATTACHMENT_BYTES + 1, 0);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&oversize);
        assert_eq!(
            error_code(&create("req_big", encoded)),
            "attachment_too_large"
        );

        let text = base64::engine::general_purpose::STANDARD.encode(b"not an image");
        assert_eq!(
            error_code(&create("req_text", text)),
            "attachment_unsupported_format"
        );

        assert_eq!(
            error_code(&create("req_bad_b64", "%%%".into())),
            "invalid_params"
        );

        let png = base64::engine::general_purpose::STANDARD
            .encode([0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3]);
        let response = create("req_ok", png);
        let parsed: SuccessResponse = serde_json::from_str(&response)
            .unwrap_or_else(|_| panic!("expected success response, got: {response}"));
        match parsed.result {
            ResponseResult::AttachmentCreated { path, expires_at } => {
                let path = PathBuf::from(path);
                assert!(path.is_file(), "success response must name a readable file");
                assert!(expires_at > 0);
                let _ = fs::remove_file(path);
            }
            other => panic!("expected attachment_created, got {other:?}"),
        }
    }

    #[test]
    fn request_dispatches_to_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = Request {
            id: "req_2".into(),
            method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
        };

        let request_for_thread = request.clone();
        let thread = std::thread::spawn(move || {
            handle_request(
                request_for_thread,
                &tx,
                None,
                None,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            )
        });

        let msg = rx.blocking_recv().unwrap();
        assert_eq!(msg.request.id, "req_2");
        msg.respond_to
            .send(
                serde_json::to_string(&SuccessResponse {
                    id: "req_2".into(),
                    result: ResponseResult::Ok {},
                })
                .unwrap(),
            )
            .unwrap();

        let response = thread.join().unwrap();
        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.id, "req_2");
    }

    #[test]
    fn dispatched_request_reports_response_write_completion() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel();
        let (mut client, server, _path) = local_stream_pair("write-ack");
        client
            .write_all(br#"{"id":"req_write","method":"workspace.list","params":{}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let server_thread = std::thread::spawn(move || {
            handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            )
        });

        let msg = api_rx.blocking_recv().unwrap();
        let response_write_complete = msg
            .response_write_complete
            .expect("socket-dispatched requests include write completion");
        msg.respond_to
            .send(
                serde_json::to_string(&SuccessResponse {
                    id: msg.request.id,
                    result: ResponseResult::Ok {},
                })
                .unwrap(),
            )
            .unwrap();

        response_write_complete
            .recv_timeout(Duration::from_secs(1))
            .expect("response write completion");
        let response: SuccessResponse = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response.id, "req_write");
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn events_wait_agent_status_returns_initial_match() {
        let (api_tx, responder) =
            spawn_pane_get_responder(crate::api::schema::AgentStatus::Blocked);

        let (mut client, server, _path) = local_stream_pair("api-events-wait-initial");
        client
            .write_all(br#"{"id":"wait_1","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":1000}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let event_hub = EventHub::default();
        handle_connection(
            server,
            &api_tx,
            &event_hub,
            &running,
            None,
            &crate::api::SharedServerName::new("test".to_string()),
            &undeclared_server_reach(),
            &undeclared_advertised_endpoint(),
            &test_credentials(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "wait_1");
        assert_eq!(response["result"]["type"], "wait_matched");
        assert_eq!(
            response["result"]["event"]["data"]["agent_status"],
            "blocked"
        );
        drop(api_tx);
        responder.join().unwrap();
    }

    #[test]
    fn events_wait_agent_status_times_out_server_side() {
        let (api_tx, responder) =
            spawn_pane_get_responder(crate::api::schema::AgentStatus::Unknown);

        let (mut client, server, _path) = local_stream_pair("api-events-wait-timeout");
        client
            .write_all(br#"{"id":"wait_2","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":30}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let event_hub = EventHub::default();
        handle_connection(
            server,
            &api_tx,
            &event_hub,
            &running,
            None,
            &crate::api::SharedServerName::new("test".to_string()),
            &undeclared_server_reach(),
            &undeclared_advertised_endpoint(),
            &test_credentials(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "wait_2");
        assert_eq!(response["error"]["code"], "timeout");
        assert_eq!(
            response["error"]["message"],
            "timed out waiting for event match"
        );
        drop(api_tx);
        responder.join().unwrap();
    }

    #[test]
    fn events_wait_agent_status_returns_not_found_when_pane_closes() {
        let event_hub = EventHub::default();
        let responder_event_hub = event_hub.clone();
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let mut pane_get_count = 0;
            while let Some(msg) = api_rx.blocking_recv() {
                let Method::PaneGet(_) = msg.request.method else {
                    panic!("unexpected request: {:?}", msg.request.method);
                };
                pane_get_count += 1;
                let response = if pane_get_count == 1 {
                    serde_json::to_string(&SuccessResponse {
                        id: msg.request.id,
                        result: ResponseResult::PaneInfo {
                            pane: pane_info("pane_1", crate::api::schema::AgentStatus::Unknown),
                        },
                    })
                    .unwrap()
                } else {
                    if pane_get_count == 2 {
                        responder_event_hub.push(crate::api::schema::EventEnvelope {
                            event: crate::api::schema::EventKind::PaneClosed,
                            data: crate::api::schema::EventData::PaneClosed {
                                pane_id: "pane_1".into(),
                                workspace_id: "ws_1".into(),
                            },
                        });
                    }
                    error_response_json(
                        msg.request.id,
                        "pane_not_found",
                        "pane pane_1 not found".into(),
                    )
                };
                msg.respond_to.send(response).unwrap();
            }
        });

        let (mut client, server, _path) = local_stream_pair("wait-close");
        client
            .write_all(br#"{"id":"wait_close","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"done"},"timeout_ms":500}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        handle_connection(
            server,
            &api_tx,
            &event_hub,
            &running,
            None,
            &crate::api::SharedServerName::new("test".to_string()),
            &undeclared_server_reach(),
            &undeclared_advertised_endpoint(),
            &test_credentials(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "wait_close");
        assert_eq!(response["error"]["code"], "pane_not_found");
        assert_eq!(response["error"]["message"], "pane pane_1 not found");
        drop(api_tx);
        responder.join().unwrap();
    }

    #[test]
    fn wait_for_output_stops_when_client_disconnects() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
        let responder = std::thread::spawn(move || {
            let mut notified = false;
            while let Some(msg) = api_rx.blocking_recv() {
                assert!(matches!(msg.request.method, Method::PaneRead(_)));
                if !notified {
                    first_read_tx.send(()).unwrap();
                    notified = true;
                }
                msg.respond_to
                    .send(
                        serde_json::to_string(&SuccessResponse {
                            id: msg.request.id,
                            result: ResponseResult::PaneRead {
                                read: crate::api::schema::PaneReadResult {
                                    pane_id: "pane_1".into(),
                                    workspace_id: "ws_1".into(),
                                    tab_id: "tab_1".into(),
                                    source: crate::api::schema::ReadSource::RecentUnwrapped,
                                    format: crate::api::schema::ReadFormat::Text,
                                    text: String::new(),
                                    revision: 0,
                                    truncated: false,
                                    effective_offset: None,
                                    has_more: None,
                                },
                            },
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
        });

        let (mut client, server, _path) = local_stream_pair("api-wait-disconnect");
        client
            .write_all(br#"{"id":"req_wait","method":"pane.wait_for_output","params":{"pane_id":"pane_1","source":"recent","match":{"type":"substring","value":"never"}}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            );
            done_tx.send(result).unwrap();
        });

        first_read_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(client);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());

        server_thread.join().unwrap();
        drop(running);
        responder.join().unwrap();
    }

    #[test]
    fn subscriptions_stop_when_client_disconnects() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-disconnect");
        client
            .write_all(
                br#"{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            );
            done_tx.send(result).unwrap();
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        drop(client);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }

    #[test]
    fn live_only_subscribe_skips_ring_replay_and_streams_post_subscribe_events() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-live-only");
        client
            .set_recv_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .write_all(
                br#"{"id":"sub_live","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}],"live_only":true}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        event_hub.push(workspace_created_event("ring-before-subscribe"));
        let server_event_hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &server_event_hub,
                &server_running,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            );
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ack: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        event_hub.push(workspace_created_event("live-after-subscribe"));
        line.clear();
        reader.read_line(&mut line).unwrap();
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["event"], "workspace_created");
        assert_eq!(
            event["data"]["workspace"]["workspace_id"],
            "live-after-subscribe"
        );

        drop(reader);
        drop(client);
        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }

    #[test]
    fn live_only_subscribe_skips_ring_replay_for_layout_updated() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-live-only-layout");
        client
            .set_recv_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .write_all(
                br#"{"id":"sub_live_layout","method":"events.subscribe","params":{"subscriptions":[{"type":"layout.updated"}],"live_only":true}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        event_hub.push(layout_updated_event("tab-ring-before-subscribe"));
        let server_event_hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &server_event_hub,
                &server_running,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            );
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ack: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        event_hub.push(layout_updated_event("tab-live-after-subscribe"));
        line.clear();
        reader.read_line(&mut line).unwrap();
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["event"], "layout_updated");
        assert_eq!(
            event["data"]["layout"]["tab_id"],
            "tab-live-after-subscribe"
        );

        drop(reader);
        drop(client);
        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }

    #[test]
    fn subscriptions_stop_when_server_shuts_down() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-shutdown");
        client
            .write_all(
                br#"{"id":"sub_2","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &crate::api::SharedServerName::new("test".to_string()),
                &undeclared_server_reach(),
                &undeclared_advertised_endpoint(),
                &test_credentials(),
            );
            done_tx.send(result).unwrap();
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        running.store(false, Ordering::Relaxed);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }
}

#[cfg(test)]
mod pane_graphics_request_tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn maximum_public_graphics_request_fits_initial_json_line() {
        let request = Request {
            id: "graphics-max".into(),
            method: Method::PaneGraphicsSet(crate::api::schema::PaneGraphicsSetParams {
                pane_id: "pane_1".into(),
                layer_id: None,
                z_index: 0,
                owner: String::new(),
                format: crate::api::schema::PaneGraphicsFormat::Png,
                image_width: 1,
                image_height: 1,
                data_base64: base64::engine::general_purpose::STANDARD
                    .encode(vec![1_u8; crate::api::schema::PANE_GRAPHICS_SET_MAX_BYTES]),
                data: None,
                placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
            }),
        };
        let encoded = serde_json::to_vec(&request).unwrap();

        assert!(encoded.len() < MAX_INITIAL_REQUEST_BYTES);
    }

    #[test]
    fn duplicate_method_cannot_be_reinterpreted_as_graphics_stream() {
        let encoded = r#"{"id":"duplicate","method":"ping","method":"pane.graphics.stream","params":{"pane_id":"pane_1"}}"#;

        assert!(serde_json::from_str::<Request>(encoded).is_err());
    }
}
