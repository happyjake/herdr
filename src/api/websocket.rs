//! Optional WebSocket transport for the JSON API.
//!
//! This is a transport adapter, not a new API: it terminates WebSocket,
//! requires a bearer token at the handshake, and feeds the exact request
//! dispatch and event subscription paths the Unix socket uses — one JSON
//! message per WS frame. No method gains transport-specific behavior.
//!
//! The listener is off by default and binds only an explicitly configured
//! address. It is never enabled implicitly. The transport is plain ws://;
//! the network layer (a tailnet, loopback) is the transport security.
//!
//! Framing differences from the Unix socket, by design:
//! - one JSON message per text frame instead of one JSON line
//! - a connection may carry sequential requests; the Unix socket serves one
//!   request per connection. Each request still runs through the shared
//!   dispatch path, so payloads are identical modulo framing.
//! - `events.subscribe` streams until the client disconnects, but unlike the
//!   Unix socket it does not monopolize the connection: requests sent while
//!   the stream runs are served in place and the stream keeps going. Only a
//!   second *streaming* request is refused, with `stream_busy`.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};
use tungstenite::handshake::server::{
    ErrorResponse as WsErrorResponse, Request as WsUpgradeRequest, Response as WsUpgradeResponse,
};
use tungstenite::handshake::HandshakeError;
use tungstenite::http::StatusCode;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Message, Utf8Bytes, WebSocket};

use crate::api::schema::ServerCapabilities;
use crate::api::server::{
    handle_parsed_request, parse_api_request, serve_interleaved_request, ApiTransport, PeerState,
    CONNECTION_POLL_INTERVAL, INITIAL_REQUEST_TIMEOUT, MAX_INITIAL_REQUEST_BYTES,
};
use crate::api::{ApiRequestSender, EventHub};
use crate::config::WebSocketApiConfig;
use crate::ipc::is_connection_closed_error;

/// Overall budget for completing the HTTP upgrade, including auth.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-syscall socket timeout during the blocking handshake phase.
const HANDSHAKE_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for flushing one outgoing frame to a slow client.
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Send a protocol ping once a proven WebSocket client has been inbound-idle
/// for this long.
const WS_IDLE_PING_AFTER: Duration = Duration::from_secs(30);
/// Close a proven WebSocket client that stays fully silent this long.
const WS_IDLE_CLOSE_AFTER: Duration = Duration::from_secs(90);
const WS_IDLE_REAP_ERROR: &str = "timed out waiting for websocket pong after idle ping";
/// Sent for a binary frame, whether it arrives between requests or during a
/// stream: the API is one JSON message per text frame.
const BINARY_FRAME_REFUSAL: &str = r#"{"id":"","error":{"code":"invalid_request","message":"invalid request: binary frames are not supported; send one JSON message per text frame"}}"#;
/// How long a bind retries while the previous owner releases the port. A
/// live handoff frees the TCP port only when the old server drops its
/// listener, so the replacement server may briefly race it.
const BIND_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// A validated, ready-to-bind listener configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebSocketApiSpec {
    addr: SocketAddr,
    token: String,
}

/// Resolve the config section into a bindable spec.
///
/// Returns `Ok(None)` when the listener is not configured (the default).
/// Returns an error when the section is configured but unusable — the server
/// must fail loudly instead of silently skipping an explicitly requested
/// listener or starting an unauthenticated one.
pub(crate) fn websocket_api_spec(
    config: &WebSocketApiConfig,
) -> io::Result<Option<WebSocketApiSpec>> {
    let bind = match config.bind.as_deref() {
        None | Some("") => return Ok(None),
        Some(bind) => bind,
    };

    let addr: SocketAddr = bind.parse().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid websocket_api.bind address {bind:?}: {err}"),
        )
    })?;

    let token = match config.token.as_deref() {
        None | Some("") => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "websocket_api.bind is set but websocket_api.token is missing; \
                 refusing to start an unauthenticated listener",
            ));
        }
        Some(token) => token,
    };

    if !valid_token_chars(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "websocket_api.token must contain only ASCII letters, digits, or -._~ \
             so it never needs escaping in an Authorization header or a URL query \
             parameter",
        ));
    }

    Ok(Some(WebSocketApiSpec {
        addr,
        token: token.to_string(),
    }))
}

/// Tokens are restricted to URL-unreserved characters (RFC 3986: ALPHA,
/// DIGIT, `-._~`) so the `token` query parameter compares byte-for-byte
/// whether or not a client percent-encodes, and base64url-shaped tokens fit
/// as-is. Anything needing escaping is rejected at config time instead of
/// mismatching at the handshake.
pub(crate) fn valid_token_chars(token: &str) -> bool {
    token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
}

/// The listener's expected bearer token, shared between the accept loop and
/// the owner of the [`WebSocketServerHandle`]. Every handshake reads the
/// current value, so replacing the token takes effect for the next connection
/// attempt without rebinding the listener; connections authorized before a
/// rotation stay connected, exactly like a Unix socket client that already
/// passed its permission check.
#[derive(Debug, Clone)]
pub struct SharedWebSocketToken {
    token: Arc<RwLock<String>>,
}

impl SharedWebSocketToken {
    pub(crate) fn new(token: String) -> Self {
        Self {
            token: Arc::new(RwLock::new(token)),
        }
    }

    /// Snapshot of the currently accepted token.
    pub(crate) fn current(&self) -> String {
        self.token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn replace(&self, token: String) -> bool {
        let mut current = self.token.write().unwrap_or_else(PoisonError::into_inner);
        if *current == token {
            return false;
        }
        *current = token;
        true
    }

    /// Apply a reloaded `[websocket_api]` section to the live listener.
    ///
    /// Only the token can change without a restart; the bind address is fixed
    /// for the lifetime of the listener. Returns `Ok(changed)`. On error the
    /// current token stays in effect — the listener never runs without a
    /// token and never accepts one that would need URL escaping.
    pub(crate) fn apply_reloaded_config(
        &self,
        config: &WebSocketApiConfig,
    ) -> Result<bool, String> {
        if matches!(config.bind.as_deref(), None | Some("")) {
            return Err(
                "websocket_api.bind was removed; the websocket listener stays bound \
                 and keeps its current token until the server restarts"
                    .to_string(),
            );
        }

        let token = match config.token.as_deref() {
            None | Some("") => {
                return Err("websocket_api.token is missing; the websocket listener \
                            keeps its current token"
                    .to_string());
            }
            Some(token) => token,
        };

        if !valid_token_chars(token) {
            return Err(
                "websocket_api.token must contain only ASCII letters, digits, or -._~; \
                 the websocket listener keeps its current token"
                    .to_string(),
            );
        }

        Ok(self.replace(token.to_string()))
    }
}

/// Why a WebSocket handshake was rejected. Auth runs inside the HTTP upgrade
/// callback, so a rejected handshake never reaches request dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WsAuthError {
    /// No Authorization header and no token query parameter.
    MissingToken,
    /// An Authorization header was present but was not a usable bearer token.
    MalformedAuthorization,
    /// A token was presented and did not match.
    WrongToken,
}

impl WsAuthError {
    fn as_str(self) -> &'static str {
        match self {
            WsAuthError::MissingToken => "missing_token",
            WsAuthError::MalformedAuthorization => "malformed_authorization",
            WsAuthError::WrongToken => "wrong_token",
        }
    }
}

/// Validate handshake credentials against the registry. The Authorization
/// header is authoritative when present; the `token` query parameter exists
/// for clients that cannot set headers on a WebSocket upgrade (browsers). A
/// malformed header fails closed instead of falling back to the query
/// parameter.
///
/// Any live credential is accepted — the pairing's managing credential and
/// every minted limited one alike (ADR-0026) — and so is one this server
/// has revoked, which is admitted only to be told so on the socket. The tier travels with the
/// connection and decides only what the credential may then manage; a
/// limited credential drives the whole pane API exactly like the managing
/// one. An unknown or revoked credential is a `wrong_token` rejection,
/// indistinguishable from any other 401 by design.
pub(crate) fn authorize_ws_request(
    authorization: Option<&str>,
    query: Option<&str>,
    credentials: &crate::api::SharedCredentialRegistry,
) -> Result<crate::api::credentials::HandshakeOutcome, WsAuthError> {
    if let Some(authorization) = authorization {
        let Some(presented) = bearer_token(authorization) else {
            return Err(WsAuthError::MalformedAuthorization);
        };
        return check_token(presented, credentials);
    }

    if let Some(presented) = query_token(query) {
        return check_token(presented, credentials);
    }

    Err(WsAuthError::MissingToken)
}

fn check_token(
    presented: &str,
    credentials: &crate::api::SharedCredentialRegistry,
) -> Result<crate::api::credentials::HandshakeOutcome, WsAuthError> {
    match credentials.authenticate_handshake(presented) {
        // A credential this server revoked is admitted so the verdict can be
        // delivered as JSON; only a token it never issued keeps the opaque
        // 401 a browser cannot read anything out of.
        outcome @ (crate::api::credentials::HandshakeOutcome::Live(_)
        | crate::api::credentials::HandshakeOutcome::Revoked { .. }) => Ok(outcome),
        crate::api::credentials::HandshakeOutcome::Unknown => Err(WsAuthError::WrongToken),
    }
}

fn bearer_token(authorization: &str) -> Option<&str> {
    let mut parts = authorization.trim().splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = parts.next()?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn query_token(query: Option<&str>) -> Option<&str> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .filter(|token| !token.is_empty())
}

// Presented tokens are compared inside the registry, which compares
// fingerprints without an early exit; nothing here needs the raw token.

pub struct WebSocketServerHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    running: Arc<AtomicBool>,
    local_addr: SocketAddr,
    token: SharedWebSocketToken,
}

impl WebSocketServerHandle {
    /// The address actually bound. Differs from the configured address only
    /// when the configured port is 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The live expected-token slot. Config reload uses this to rotate the
    /// bearer token without rebinding the listener.
    pub fn shared_token(&self) -> SharedWebSocketToken {
        self.token.clone()
    }
}

impl Drop for WebSocketServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // The accept loop polls a non-blocking listener, so it observes the
        // flag within one poll interval. Joining makes the drop synchronous:
        // afterwards the TCP port is released, which live handoff relies on.
        // Connection threads stop on the same flag but are not joined.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start the WebSocket API listener if — and only if — it is configured.
/// Never binds anything when `websocket_api.bind` is unset.
pub fn start_websocket_server(
    config: &WebSocketApiConfig,
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    server_name: crate::api::SharedServerName,
    server_reach: crate::api::SharedServerReach,
) -> io::Result<Option<WebSocketServerHandle>> {
    start_websocket_server_with_capabilities(
        config,
        api_tx,
        event_hub,
        crate::api::credentials::process_registry(),
        Some(crate::api::server_capabilities()),
        server_name,
        server_reach,
    )
}

/// Like [`start_websocket_server`], with explicit ping capabilities. Call
/// sites must pass the same capabilities and shared declaration slots as
/// their Unix socket listener so `ping` responses are identical over both
/// transports.
pub fn start_websocket_server_with_capabilities(
    config: &WebSocketApiConfig,
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    credentials: crate::api::SharedCredentialRegistry,
    capabilities: Option<ServerCapabilities>,
    server_name: crate::api::SharedServerName,
    server_reach: crate::api::SharedServerReach,
) -> io::Result<Option<WebSocketServerHandle>> {
    let Some(spec) = websocket_api_spec(config)? else {
        return Ok(None);
    };

    let listener = bind_with_addr_in_use_retry(spec.addr)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    crate::api::attachment::spawn_ttl_sweeper();

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    let token = SharedWebSocketToken::new(spec.token);
    // The registry reads the managing credential through the live slot, so a
    // `herdr pair` rotation applied by a config reload is recognized as a
    // rotation of that one credential without any further plumbing.
    credentials.attach_managing_token(token.clone());

    let thread = std::thread::spawn(move || {
        loop {
            if !listener_running.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((stream, peer)) => {
                    let api_tx = api_tx.clone();
                    let event_hub = event_hub.clone();
                    let capabilities = capabilities.clone();
                    let server_name = server_name.clone();
                    let server_reach = server_reach.clone();
                    let connection_running = Arc::clone(&listener_running);
                    let credentials = credentials.clone();
                    std::thread::spawn(move || {
                        if let Err(err) = handle_ws_connection(
                            stream,
                            &credentials,
                            &api_tx,
                            &event_hub,
                            &connection_running,
                            capabilities,
                            &server_name,
                            &server_reach,
                        ) {
                            warn!(peer = %peer, err = %err, "websocket api connection failed");
                        }
                    });
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    wait_for_connection(&listener, CONNECTION_POLL_INTERVAL);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    error!(err = %err, "websocket api listener accept failed");
                    break;
                }
            }
        }
        debug!("websocket api server thread exiting");
    });

    let handle = WebSocketServerHandle {
        thread: Some(thread),
        running,
        local_addr,
        token,
    };
    info!(addr = %handle.local_addr(), "websocket api server listening");
    Ok(Some(handle))
}

/// Wait until the listener has a connection ready to accept, or until
/// `timeout` elapses — whichever comes first.
///
/// The accept loop needs both halves: it must notice a client immediately,
/// and it must observe the shutdown flag promptly enough that dropping the
/// handle (a live handoff) releases the port. Sleeping between non-blocking
/// accepts gives up the first half, and macOS makes that a real outage: it
/// suspends a background daemon's timers once the machine idles with the lid
/// shut, so the loop stops accepting for minutes at a time. The port keeps
/// completing handshakes in the kernel throughout, so clients connect to a
/// socket that never answers the upgrade and can only report an endless
/// reconnect. Waiting on the socket itself is an I/O wake, which throttling
/// does not defer — the same reason the Unix socket listener, which blocks in
/// `accept`, never had the problem. The timeout now bounds only how long
/// shutdown takes, not how late a client is noticed.
#[cfg(unix)]
fn wait_for_connection(listener: &TcpListener, timeout: Duration) {
    use std::os::fd::AsRawFd;

    let mut poll_fd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // Any outcome — ready, timed out, or interrupted — means the same thing
    // here: go round the loop, re-check the shutdown flag, retry the accept.
    // A spurious wake costs one non-blocking accept, so errors need no
    // handling of their own.
    unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
}

#[cfg(not(unix))]
fn wait_for_connection(_listener: &TcpListener, timeout: Duration) {
    std::thread::sleep(timeout);
}

fn bind_with_addr_in_use_retry(addr: SocketAddr) -> io::Result<TcpListener> {
    let deadline = Instant::now() + BIND_RETRY_TIMEOUT;
    loop {
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                std::thread::sleep(BIND_RETRY_INTERVAL);
            }
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("failed to bind websocket api listener on {addr}: {err}"),
                ));
            }
        }
    }
}

fn handle_ws_connection(
    stream: TcpStream,
    credentials: &crate::api::SharedCredentialRegistry,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
) -> io::Result<()> {
    configure_accepted_ws_stream(&stream)?;
    let peer = stream.peer_addr().ok();
    stream.set_read_timeout(Some(HANDSHAKE_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_IO_TIMEOUT))?;
    let stream = WsTcpStream::new(stream);

    let mut auth_error = None;
    let mut authenticated: Option<crate::api::credentials::HandshakeOutcome> = None;
    let websocket = match accept_websocket(stream, credentials, &mut auth_error, &mut authenticated)
    {
        Ok(websocket) => websocket,
        Err(err) => {
            match auth_error {
                // The HTTP 401 has already been written by the handshake;
                // the request was never parsed as an API request, let alone
                // dispatched. Never log the presented credentials.
                Some(reason) => {
                    warn!(
                        peer = %format_peer(peer),
                        reason = reason.as_str(),
                        "websocket api handshake rejected"
                    );
                }
                None => {
                    debug!(peer = %format_peer(peer), err = %err, "websocket api handshake failed");
                }
            }
            return Ok(());
        }
    };

    // The connection acts as the credential it presented, for its whole
    // life: the tier is never re-read from the payload, and a revoke ends
    // this connection's access rather than only its next handshake.
    let Some(authenticated) = authenticated else {
        debug!(peer = %format_peer(peer), "websocket api handshake produced no credential");
        return Ok(());
    };
    let credential_context = match authenticated {
        crate::api::credentials::HandshakeOutcome::Live(credential) => {
            crate::api::credentials::CredentialContext::connection(credentials.clone(), credential)
        }
        crate::api::credentials::HandshakeOutcome::Revoked { credential_id } => {
            info!(
                peer = %format_peer(peer),
                credential_id = %credential_id,
                "websocket api connection admitted to report a revoked credential"
            );
            crate::api::credentials::CredentialContext::revoked_connection(
                credentials.clone(),
                credential_id,
            )
        }
        // `check_token` turns an unrecognized token into a handshake
        // rejection, so this cannot be reached from the accept path.
        crate::api::credentials::HandshakeOutcome::Unknown => return Ok(()),
    };

    // Handshake done; use bounded blocking reads so sparse frames wake the
    // connection thread immediately while shutdown and liveness checks are
    // still observed within one poll interval.
    let mut transport = WsTransport::new(
        websocket,
        peer,
        Arc::new(WsDispatch {
            api_tx: api_tx.clone(),
            capabilities: capabilities.clone(),
            server_name: server_name.clone(),
            server_reach: server_reach.clone(),
            credentials: credential_context.clone(),
        }),
    );
    configure_established_ws_stream(transport.websocket.get_mut())?;

    let result = ws_request_loop(
        &mut transport,
        api_tx,
        event_hub,
        running,
        capabilities,
        server_name,
        server_reach,
        &credential_context,
    );

    // Best effort: tell well-behaved clients the server is done.
    let deadline = Instant::now() + FRAME_WRITE_TIMEOUT;
    let _ = transport.close_with_deadline(None, deadline);
    let _ = transport.flush_with_deadline(deadline);

    match result {
        Err(err) if is_connection_closed_error(&err) => Ok(()),
        result => result,
    }
}

/// Avoid Nagle/delayed-ACK latency cliffs for sparse API frames.
fn configure_accepted_ws_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)
}

fn configure_established_ws_stream(stream: &mut WsTcpStream) -> io::Result<()> {
    stream.clear_deadlines();
    stream.set_nodelay(true)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONNECTION_POLL_INTERVAL))?;
    stream.set_write_timeout(Some(FRAME_WRITE_TIMEOUT))
}

struct WsTcpStream {
    stream: TcpStream,
    read_deadline: Option<Instant>,
    write_deadline: Option<Instant>,
}

impl WsTcpStream {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_deadline: None,
            write_deadline: None,
        }
    }

    fn clear_deadlines(&mut self) {
        self.read_deadline = None;
        self.write_deadline = None;
    }

    fn begin_read_budget(&mut self, budget: Duration) {
        self.read_deadline = Some(Instant::now() + budget.max(Duration::from_millis(1)));
    }

    fn begin_write_deadline(&mut self, deadline: Instant) {
        self.write_deadline = Some(deadline);
    }

    fn clear_write_deadline(&mut self) -> io::Result<()> {
        self.write_deadline = None;
        self.set_write_timeout(Some(FRAME_WRITE_TIMEOUT))
    }

    fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.stream.set_nodelay(nodelay)
    }

    #[cfg(test)]
    fn nodelay(&self) -> io::Result<bool> {
        self.stream.nodelay()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.stream.set_nonblocking(nonblocking)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    #[cfg(test)]
    fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.stream.read_timeout()
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(timeout)
    }

    #[cfg(test)]
    fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.stream.write_timeout()
    }

    fn timeout_until(deadline: Instant, kind: &'static str) -> io::Result<Duration> {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("websocket {kind} budget expired"),
            ));
        }
        Ok(deadline
            .saturating_duration_since(now)
            .max(Duration::from_millis(1)))
    }
}

impl Read for WsTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.read_deadline {
            self.stream
                .set_read_timeout(Some(Self::timeout_until(deadline, "read")?))?;
        }
        self.stream.read(buf)
    }
}

impl Write for WsTcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(deadline) = self.write_deadline {
            self.stream
                .set_write_timeout(Some(Self::timeout_until(deadline, "write")?))?;
        }
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(deadline) = self.write_deadline {
            self.stream
                .set_write_timeout(Some(Self::timeout_until(deadline, "write")?))?;
        }
        self.stream.flush()
    }
}

fn format_peer(peer: Option<SocketAddr>) -> String {
    peer.map(|addr| addr.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn accept_websocket(
    stream: WsTcpStream,
    credentials: &crate::api::SharedCredentialRegistry,
    auth_error: &mut Option<WsAuthError>,
    authenticated: &mut Option<crate::api::credentials::HandshakeOutcome>,
) -> Result<WebSocket<WsTcpStream>, tungstenite::Error> {
    // The Err type is tungstenite's `ErrorResponse`; the `Callback` trait
    // fixes this signature, so the variant cannot be boxed away.
    #[allow(clippy::result_large_err)]
    let callback = |request: &WsUpgradeRequest, response: WsUpgradeResponse| {
        let authorization = match request
            .headers()
            .get(tungstenite::http::header::AUTHORIZATION)
        {
            Some(value) => match value.to_str() {
                Ok(value) => Some(value),
                Err(_) => {
                    *auth_error = Some(WsAuthError::MalformedAuthorization);
                    return Err(unauthorized_response());
                }
            },
            None => None,
        };

        // Credentials are read at handshake time, not listener start, so a
        // rotation or a revoke is enforced on the very next connection
        // attempt.
        match authorize_ws_request(authorization, request.uri().query(), credentials) {
            Ok(credential) => {
                *authenticated = Some(credential);
                Ok(response)
            }
            Err(reason) => {
                *auth_error = Some(reason);
                Err(unauthorized_response())
            }
        }
    };

    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_INITIAL_REQUEST_BYTES))
        .max_frame_size(Some(MAX_INITIAL_REQUEST_BYTES));

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut pending = match tungstenite::accept_hdr_with_config(stream, callback, Some(config)) {
        Ok(websocket) => return Ok(websocket),
        Err(HandshakeError::Failure(err)) => return Err(err),
        Err(HandshakeError::Interrupted(pending)) => pending,
    };

    loop {
        if Instant::now() >= deadline {
            return Err(tungstenite::Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out during websocket handshake",
            )));
        }
        match pending.handshake() {
            Ok(websocket) => return Ok(websocket),
            Err(HandshakeError::Failure(err)) => return Err(err),
            Err(HandshakeError::Interrupted(next)) => pending = next,
        }
    }
}

fn unauthorized_response() -> WsErrorResponse {
    let mut response = WsErrorResponse::new(Some("unauthorized\n".to_string()));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

fn ws_request_loop(
    transport: &mut WsTransport,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_name: &crate::api::SharedServerName,
    server_reach: &crate::api::SharedServerReach,
    credentials: &crate::api::credentials::CredentialContext,
) -> io::Result<()> {
    // Parity with the Unix socket: a client that completes the handshake
    // gets a bounded window to send its first request. Once the connection
    // has proven itself, protocol ping/pong liveness covers later idle time.
    let mut first_request_deadline = Some(Instant::now() + INITIAL_REQUEST_TIMEOUT);

    loop {
        if !running.load(Ordering::Relaxed) {
            return Ok(());
        }

        match transport.read_message(first_request_deadline) {
            Ok(Message::Text(text)) => {
                transport.mark_inbound();
                first_request_deadline = None;
                let Some(request) = parse_api_request(transport, text.as_str())? else {
                    continue;
                };
                // Read before serving: this is exactly what makes dispatch
                // answer with the refusal below, and the verdict it writes
                // is terminal — a revoked connection is served nothing else.
                let revoked = credentials.is_revoked();
                handle_parsed_request(
                    request,
                    transport,
                    api_tx,
                    event_hub,
                    running,
                    capabilities.clone(),
                    None,
                    server_name,
                    server_reach,
                    credentials,
                )?;
                if revoked {
                    transport.note_revocation_verdict_sent();
                    debug!(
                        peer = %format_peer(transport.peer),
                        credential_id = credentials.credential_id().unwrap_or("unknown"),
                        "closing websocket api connection after its revocation verdict"
                    );
                    return Ok(());
                }
            }
            Ok(Message::Binary(_)) => {
                transport.mark_inbound();
                first_request_deadline = None;
                transport.write_message(BINARY_FRAME_REFUSAL)?;
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                transport.mark_inbound();
                // tungstenite queues the pong reply internally; flush it.
                transport.flush_ignore_would_block()?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Frame(_)) => {}
            Err(err) => match classify_ws_error(err) {
                WsErrorClass::WouldBlock => {
                    if first_request_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out reading api request",
                        ));
                    }
                    // Checked on the idle path, so a request that arrives
                    // after a revoke is still answered with its own refusal
                    // (the code the client keys its wipe path on) before the
                    // connection goes. An idle connection has no request to
                    // answer, so it is told off-id instead — a bare close
                    // would be indistinguishable from transient trouble —
                    // and then ends, within one poll interval of the revoke.
                    if credentials.is_revoked() {
                        transport.send_revocation_verdict_once(credentials)?;
                        debug!(
                            peer = %format_peer(transport.peer),
                            credential_id = credentials.credential_id().unwrap_or("unknown"),
                            "closing websocket api connection: credential revoked"
                        );
                        return Ok(());
                    }
                    if first_request_deadline.is_none() {
                        transport.poll_liveness()?;
                    }
                }
                WsErrorClass::Closed => return Ok(()),
                WsErrorClass::Failed(err) => return Err(err),
            },
        }
    }
}

enum WsErrorClass {
    WouldBlock,
    Closed,
    Failed(io::Error),
}

fn classify_ws_error(err: tungstenite::Error) -> WsErrorClass {
    match err {
        tungstenite::Error::Io(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            WsErrorClass::WouldBlock
        }
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            WsErrorClass::Closed
        }
        tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => WsErrorClass::Closed,
        tungstenite::Error::Io(err) if is_connection_closed_error(&err) => WsErrorClass::Closed,
        tungstenite::Error::Io(err) => WsErrorClass::Failed(err),
        err => WsErrorClass::Failed(io::Error::other(err)),
    }
}

fn first_request_timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out reading api request")
}

fn restore_tungstenite_result<T>(
    result: Result<T, tungstenite::Error>,
    restore: io::Result<()>,
) -> Result<T, tungstenite::Error> {
    match result {
        Ok(value) => {
            restore.map_err(tungstenite::Error::Io)?;
            Ok(value)
        }
        Err(err) => Err(err),
    }
}

fn restore_io_result<T>(result: io::Result<T>, restore: io::Result<()>) -> io::Result<T> {
    match result {
        Ok(value) => {
            restore?;
            Ok(value)
        }
        Err(err) => Err(err),
    }
}

#[derive(Debug, Clone, Copy)]
struct WsLivenessTiming {
    ping_after: Duration,
    close_after: Duration,
}

fn ws_liveness_timing() -> WsLivenessTiming {
    #[cfg(debug_assertions)]
    {
        if let Some(timing) = ws_liveness_timing_from_env() {
            return timing;
        }
    }

    WsLivenessTiming {
        ping_after: WS_IDLE_PING_AFTER,
        close_after: WS_IDLE_CLOSE_AFTER,
    }
}

#[cfg(debug_assertions)]
fn ws_liveness_timing_from_env() -> Option<WsLivenessTiming> {
    let ping_after = std::env::var("HERDR_TEST_WS_IDLE_PING_AFTER_MS")
        .ok()?
        .parse::<u64>()
        .ok()?;
    let close_after = std::env::var("HERDR_TEST_WS_IDLE_CLOSE_AFTER_MS")
        .ok()?
        .parse::<u64>()
        .ok()?;

    if ping_after == 0 || close_after <= ping_after {
        return None;
    }

    Some(WsLivenessTiming {
        ping_after: Duration::from_millis(ping_after),
        close_after: Duration::from_millis(close_after),
    })
}

#[derive(Debug)]
struct WsLiveness {
    timing: WsLivenessTiming,
    last_inbound: Instant,
    ping_sent_at: Option<Instant>,
}

impl WsLiveness {
    fn new(timing: WsLivenessTiming) -> Self {
        Self {
            timing,
            last_inbound: Instant::now(),
            ping_sent_at: None,
        }
    }

    fn mark_inbound(&mut self, now: Instant) {
        self.last_inbound = now;
        self.ping_sent_at = None;
    }

    fn should_ping(&self, now: Instant) -> bool {
        self.ping_sent_at.is_none()
            && now.saturating_duration_since(self.last_inbound) >= self.timing.ping_after
    }

    fn mark_ping_sent(&mut self, now: Instant) {
        self.ping_sent_at = Some(now);
    }

    fn should_reap(&self, now: Instant) -> bool {
        self.ping_sent_at.is_some()
            && now.saturating_duration_since(self.last_inbound) >= self.timing.close_after
    }
}

/// What a websocket connection needs to answer a request on its own, without
/// unwinding back to [`ws_request_loop`] first. Held behind an `Arc` so the
/// transport can hand it to a dispatch call that borrows the transport itself.
struct WsDispatch {
    api_tx: ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_name: crate::api::SharedServerName,
    server_reach: crate::api::SharedServerReach,
    credentials: crate::api::credentials::CredentialContext,
}

struct WsTransport {
    websocket: WebSocket<WsTcpStream>,
    peer: Option<SocketAddr>,
    liveness: WsLiveness,
    dispatch: Arc<WsDispatch>,
    /// Whether this connection has already been told its credential is
    /// revoked. The verdict is terminal, so it is written exactly once
    /// however the connection reaches its end — held stream, idle poll, or
    /// an answered request.
    revocation_verdict_sent: bool,
}

impl WsTransport {
    fn new(
        websocket: WebSocket<WsTcpStream>,
        peer: Option<SocketAddr>,
        dispatch: Arc<WsDispatch>,
    ) -> Self {
        Self {
            websocket,
            peer,
            liveness: WsLiveness::new(ws_liveness_timing()),
            dispatch,
            revocation_verdict_sent: false,
        }
    }

    /// Deliver the terminal `credential_revoked` verdict, at most once.
    fn send_revocation_verdict_once(
        &mut self,
        credentials: &crate::api::credentials::CredentialContext,
    ) -> io::Result<()> {
        if self.revocation_verdict_sent {
            return Ok(());
        }
        self.revocation_verdict_sent = true;
        let verdict = credentials.revocation_verdict();
        match self.write_message(&verdict) {
            Err(err) if is_connection_closed_error(&err) => Ok(()),
            result => result,
        }
    }

    /// Record that dispatch already answered a request with the verdict.
    fn note_revocation_verdict_sent(&mut self) {
        self.revocation_verdict_sent = true;
    }

    fn read_message(
        &mut self,
        first_request_deadline: Option<Instant>,
    ) -> Result<Message, tungstenite::Error> {
        let budget = self
            .read_budget(first_request_deadline)
            .map_err(tungstenite::Error::Io)?;
        self.websocket.get_mut().begin_read_budget(budget);
        let result = self.websocket.read();
        let restore = configure_established_ws_stream(self.websocket.get_mut());
        restore_tungstenite_result(result, restore)
    }

    fn read_budget(&self, first_request_deadline: Option<Instant>) -> io::Result<Duration> {
        let Some(deadline) = first_request_deadline else {
            return Ok(CONNECTION_POLL_INTERVAL);
        };
        let now = Instant::now();
        if now >= deadline {
            return Err(first_request_timeout_error());
        }
        Ok(CONNECTION_POLL_INTERVAL.min(
            deadline
                .saturating_duration_since(now)
                .max(Duration::from_millis(1)),
        ))
    }

    fn mark_inbound(&mut self) {
        self.liveness.mark_inbound(Instant::now());
    }

    fn poll_liveness(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if self.liveness.should_reap(now) {
            let idle_for = now.saturating_duration_since(self.liveness.last_inbound);
            warn!(
                peer = %format_peer(self.peer),
                reason = WS_IDLE_REAP_ERROR,
                idle_ms = idle_for.as_millis(),
                close_after_ms = self.liveness.timing.close_after.as_millis(),
                "websocket api connection reaped after idle ping timeout"
            );
            let deadline = Instant::now() + FRAME_WRITE_TIMEOUT;
            let _ = self.close_with_deadline(
                Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "idle ping timeout".into(),
                }),
                deadline,
            );
            let _ = self.flush_with_deadline(deadline);
            return Err(io::Error::new(io::ErrorKind::TimedOut, WS_IDLE_REAP_ERROR));
        }

        if self.liveness.should_ping(now) {
            self.send_liveness_ping()?;
            self.liveness.mark_ping_sent(now);
            debug!(
                peer = %format_peer(self.peer),
                ping_after_ms = self.liveness.timing.ping_after.as_millis(),
                close_after_ms = self.liveness.timing.close_after.as_millis(),
                "websocket api idle ping sent"
            );
        } else if self.liveness.ping_sent_at.is_some() {
            self.flush_ignore_would_block()?;
        }

        Ok(())
    }

    fn send_liveness_ping(&mut self) -> io::Result<()> {
        match self.send_with_deadline(
            Message::Ping(Vec::new().into()),
            Instant::now() + FRAME_WRITE_TIMEOUT,
        ) {
            Ok(()) => Ok(()),
            Err(err) => match classify_ws_error(err) {
                WsErrorClass::WouldBlock => Ok(()),
                WsErrorClass::Closed => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "websocket connection closed",
                )),
                WsErrorClass::Failed(err) => Err(err),
            },
        }
    }

    fn send_with_deadline(
        &mut self,
        message: Message,
        deadline: Instant,
    ) -> Result<(), tungstenite::Error> {
        self.websocket.get_mut().begin_write_deadline(deadline);
        let result = self.websocket.send(message);
        let restore = self.websocket.get_mut().clear_write_deadline();
        restore_tungstenite_result(result, restore)
    }

    fn close_with_deadline(
        &mut self,
        frame: Option<CloseFrame>,
        deadline: Instant,
    ) -> Result<(), tungstenite::Error> {
        self.websocket.get_mut().begin_write_deadline(deadline);
        let result = self.websocket.close(frame);
        let restore = self.websocket.get_mut().clear_write_deadline();
        restore_tungstenite_result(result, restore)
    }

    fn flush_with_deadline(&mut self, deadline: Instant) -> Result<(), tungstenite::Error> {
        self.websocket.get_mut().begin_write_deadline(deadline);
        let result = self.websocket.flush();
        let restore = self.websocket.get_mut().clear_write_deadline();
        restore_tungstenite_result(result, restore)
    }

    fn flush_ignore_would_block(&mut self) -> io::Result<()> {
        match self.flush_with_deadline(Instant::now() + FRAME_WRITE_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(err) => match classify_ws_error(err) {
                WsErrorClass::WouldBlock => Ok(()),
                WsErrorClass::Closed => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "websocket connection closed",
                )),
                WsErrorClass::Failed(err) => Err(err),
            },
        }
    }
}

impl ApiTransport for WsTransport {
    fn write_message(&mut self, message: &str) -> io::Result<()> {
        let deadline = Instant::now() + FRAME_WRITE_TIMEOUT;
        let mut result = self.send_with_deadline(Message::text(message), deadline);
        loop {
            match result {
                Ok(()) => return Ok(()),
                Err(err) => match classify_ws_error(err) {
                    WsErrorClass::WouldBlock => {
                        // The frame stays queued in tungstenite; keep
                        // flushing until the slow client drains it or the
                        // write budget runs out.
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "timed out writing websocket api message",
                            ));
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        std::thread::sleep(remaining.min(Duration::from_millis(10)));
                        result = self.flush_with_deadline(deadline);
                    }
                    WsErrorClass::Closed => {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "websocket connection closed",
                        ));
                    }
                    WsErrorClass::Failed(err) => return Err(err),
                },
            }
        }
    }

    fn pump_inbound(&mut self) -> io::Result<PeerState> {
        // Revoking a credential must end the streams it already holds, not
        // only refuse its next request: a subscribed browser that kept its
        // events would stay lit after the phone logged it out. It is told
        // why first — a stream that just stops is what transient trouble
        // looks like, and the client would retry forever instead of wiping.
        if self.dispatch.credentials.is_revoked() {
            let credentials = Arc::clone(&self.dispatch);
            self.send_revocation_verdict_once(&credentials.credentials)?;
            debug!(peer = %format_peer(self.peer), "websocket api connection dropped: credential revoked");
            return Ok(PeerState::Gone);
        }
        loop {
            // Read without blocking, but answer whatever arrived with the
            // established blocking-with-timeout config, so a response written
            // mid-stream behaves exactly like one written between requests.
            self.websocket.get_mut().set_nonblocking(true)?;
            let message = self.read_nonblocking();
            let restore = configure_established_ws_stream(self.websocket.get_mut());
            let message = restore_io_result(message, restore)?;

            match message {
                InboundFrame::Idle => return Ok(PeerState::Alive),
                InboundFrame::Closed => return Ok(PeerState::Gone),
                // A websocket is a message channel, so a client that
                // subscribed and then sent a request gets that request served
                // here and keeps its stream. Dropping the frame instead left
                // the client waiting on a response that would never come.
                InboundFrame::Text(text) => {
                    let dispatch = Arc::clone(&self.dispatch);
                    serve_interleaved_request(
                        self,
                        text.as_str(),
                        &dispatch.api_tx,
                        dispatch.capabilities.clone(),
                        None,
                        &dispatch.server_name,
                        &dispatch.server_reach,
                        &dispatch.credentials,
                    )?;
                }
                InboundFrame::Binary => {
                    self.write_message(BINARY_FRAME_REFUSAL)?;
                }
            }
        }
    }
}

enum InboundFrame {
    Idle,
    Closed,
    Text(Utf8Bytes),
    Binary,
}

impl WsTransport {
    /// One non-blocking read, with control frames handled in place.
    fn read_nonblocking(&mut self) -> io::Result<InboundFrame> {
        loop {
            match self.websocket.read() {
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    self.mark_inbound();
                    self.flush_ignore_would_block()?;
                }
                Ok(Message::Close(_)) => return Ok(InboundFrame::Closed),
                Ok(Message::Text(text)) => {
                    self.mark_inbound();
                    return Ok(InboundFrame::Text(text));
                }
                Ok(Message::Binary(_)) => {
                    self.mark_inbound();
                    return Ok(InboundFrame::Binary);
                }
                Ok(Message::Frame(_)) => {}
                Err(err) => {
                    return match classify_ws_error(err) {
                        WsErrorClass::WouldBlock => {
                            self.poll_liveness()?;
                            Ok(InboundFrame::Idle)
                        }
                        WsErrorClass::Closed => Ok(InboundFrame::Closed),
                        WsErrorClass::Failed(err) => Err(err),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiRequestMessage;
    use std::net::TcpStream;
    use tokio::sync::mpsc;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::stream::MaybeTlsStream;

    const TEST_TOKEN: &str = "test-token-1234";

    const TEST_SERVER_NAME: &str = "ws-test-server";

    /// Dispatch context for transport-level tests that never reach the app.
    /// The returned receiver is the app end; hold it for the test's lifetime.
    fn test_dispatch() -> (Arc<WsDispatch>, mpsc::UnboundedReceiver<ApiRequestMessage>) {
        let (api_tx, api_rx) = mpsc::unbounded_channel();
        let config = spec_config(Some("127.0.0.1:0"), Some(TEST_TOKEN));
        let dispatch = Arc::new(WsDispatch {
            api_tx,
            capabilities: None,
            server_name: crate::api::SharedServerName::new(TEST_SERVER_NAME.to_string()),
            server_reach: crate::api::SharedServerReach::from_config(&config),
            credentials: crate::api::credentials::CredentialContext::local_socket(test_registry()),
        });
        (dispatch, api_rx)
    }

    fn spec_config(bind: Option<&str>, token: Option<&str>) -> WebSocketApiConfig {
        WebSocketApiConfig {
            bind: bind.map(str::to_string),
            token: token.map(str::to_string),
            name: None,
            advertised_endpoint: None,
            reach: None,
        }
    }

    /// The accept loop must wake on the socket, not on the clock. A sleeping
    /// loop passes every functional test — it accepts everything, just late —
    /// so the only honest assertion is the timing one: with a client already
    /// waiting, the wait returns far sooner than its own timeout. Sleeping
    /// for the timeout (what this replaced) fails here by two orders of
    /// magnitude, which is the outage a throttled daemon actually suffers.
    #[test]
    fn wait_for_connection_returns_as_soon_as_a_client_is_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();

        let started = Instant::now();
        wait_for_connection(&listener, Duration::from_secs(5));
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(1),
            "a pending connection should wake the wait at once, waited {waited:?}"
        );
    }

    /// The other half of the contract: with nothing to accept, the wait still
    /// gives up on schedule so the loop can observe the shutdown flag and
    /// release the port for a live handoff.
    #[test]
    fn wait_for_connection_gives_up_at_the_timeout_when_no_client_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();

        let started = Instant::now();
        wait_for_connection(&listener, Duration::from_millis(150));
        let waited = started.elapsed();

        assert!(
            waited >= Duration::from_millis(100),
            "the wait should hold for its timeout, returned after {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(2),
            "the wait should not outlast its timeout, waited {waited:?}"
        );
    }

    #[test]
    fn spec_is_none_when_bind_is_unset_or_empty() {
        assert_eq!(websocket_api_spec(&spec_config(None, None)).unwrap(), None);
        assert_eq!(
            websocket_api_spec(&spec_config(None, Some(TEST_TOKEN))).unwrap(),
            None
        );
        assert_eq!(
            websocket_api_spec(&spec_config(Some(""), Some(TEST_TOKEN))).unwrap(),
            None
        );
    }

    #[test]
    fn spec_requires_a_token_when_bind_is_set() {
        let missing = websocket_api_spec(&spec_config(Some("127.0.0.1:0"), None)).unwrap_err();
        assert!(missing.to_string().contains("token is missing"));

        let empty = websocket_api_spec(&spec_config(Some("127.0.0.1:0"), Some(""))).unwrap_err();
        assert!(empty.to_string().contains("token is missing"));
    }

    #[test]
    fn spec_rejects_invalid_bind_address() {
        let err =
            websocket_api_spec(&spec_config(Some("not-an-address"), Some(TEST_TOKEN))).unwrap_err();
        assert!(err.to_string().contains("invalid websocket_api.bind"));
    }

    #[test]
    fn spec_rejects_tokens_with_unsafe_characters() {
        // Anything outside URL-unreserved characters would need escaping in
        // a query parameter, so it must be rejected at config time.
        for token in [
            "with space",
            "line\nbreak",
            "tab\ttab",
            "non-ascii-é",
            "has+plus",
            "has/slash",
            "has=equals",
        ] {
            let err =
                websocket_api_spec(&spec_config(Some("127.0.0.1:0"), Some(token))).unwrap_err();
            assert!(err.to_string().contains("websocket_api.token"), "{token:?}");
        }
    }

    #[test]
    fn spec_accepts_valid_bind_and_token() {
        let spec = websocket_api_spec(&spec_config(Some("127.0.0.1:4433"), Some(TEST_TOKEN)))
            .unwrap()
            .unwrap();
        assert_eq!(spec.addr.port(), 4433);
        assert_eq!(spec.token, TEST_TOKEN);

        // The full URL-unreserved set (base64url-shaped tokens included).
        let spec = websocket_api_spec(&spec_config(
            Some("127.0.0.1:4433"),
            Some("AZaz09-._~Base64Url_-Shaped"),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(spec.token, "AZaz09-._~Base64Url_-Shaped");
    }

    #[test]
    fn bind_retry_waits_for_the_previous_owner_to_release_the_port() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });

        // Starts while the port is still held (a live handoff race) and must
        // succeed once the previous owner lets go.
        let rebound = bind_with_addr_in_use_retry(addr);
        release.join().unwrap();
        assert!(rebound.is_ok(), "{rebound:?}");
    }

    #[test]
    fn accepted_streams_enable_tcp_nodelay() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || TcpStream::connect(addr).unwrap());

        let (stream, _) = listener.accept().unwrap();
        configure_accepted_ws_stream(&stream).unwrap();

        assert!(stream.nodelay().unwrap());
        drop(stream);
        drop(client.join().unwrap());
    }

    #[test]
    fn established_streams_use_read_timeout_instead_of_nonblocking_polling() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || TcpStream::connect(addr).unwrap());

        let (stream, _) = listener.accept().unwrap();
        let mut stream = WsTcpStream::new(stream);
        configure_established_ws_stream(&mut stream).unwrap();

        assert!(stream.nodelay().unwrap());
        assert_eq!(
            stream.read_timeout().unwrap(),
            Some(CONNECTION_POLL_INTERVAL)
        );
        assert_eq!(stream.write_timeout().unwrap(), Some(FRAME_WRITE_TIMEOUT));
        drop(stream);
        drop(client.join().unwrap());
    }

    #[test]
    fn read_budget_is_total_across_partial_socket_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (stream, _) = listener.accept().unwrap();
        client.write_all(&[1]).unwrap();

        let trickle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            let _ = client.write_all(&[2]);
        });

        let mut stream = WsTcpStream::new(stream);
        stream.begin_read_budget(Duration::from_millis(30));

        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 1);
        assert_eq!(byte[0], 1);

        let err = stream.read(&mut byte).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "{err:?}"
        );
        trickle.join().unwrap();
    }

    #[test]
    fn write_deadline_expires_before_next_socket_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut stream = WsTcpStream::new(stream);

        stream.begin_write_deadline(Instant::now());
        let err = stream.write(&[1]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(stream);
        drop(client);
    }

    /// The pump reads without blocking but must hand the socket back in its
    /// established blocking-with-timeout mode, so the response it writes and
    /// the stream's next tick behave normally.
    #[test]
    fn pump_inbound_answers_a_non_control_frame_and_restores_blocking_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (answered_tx, answered_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let (mut websocket, _) = tungstenite::connect(format!("ws://{addr}")).unwrap();
            websocket.send(Message::text("unexpected")).unwrap();
            let answer = websocket.read().unwrap();
            answered_tx
                .send(answer.into_text().unwrap().to_string())
                .unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let (stream, peer) = listener.accept().unwrap();
        configure_accepted_ws_stream(&stream).unwrap();
        let mut websocket = tungstenite::accept(WsTcpStream::new(stream)).unwrap();
        configure_established_ws_stream(websocket.get_mut()).unwrap();
        let (dispatch, _api_rx) = test_dispatch();
        let mut transport = WsTransport::new(websocket, Some(peer), dispatch);

        let mut answer = None;
        for _ in 0..50 {
            assert_eq!(transport.pump_inbound().unwrap(), PeerState::Alive);
            if let Ok(text) = answered_rx.try_recv() {
                answer = Some(text);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let answer: serde_json::Value =
            serde_json::from_str(&answer.expect("the frame must be answered, not dropped"))
                .unwrap();
        assert_eq!(answer["error"]["code"], "invalid_request");

        let started = Instant::now();
        let mut byte = [0];
        let err = transport
            .websocket
            .get_mut()
            .stream
            .peek(&mut byte)
            .unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "{err:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        client.join().unwrap();
    }

    /// A registry whose managing credential is the test token — the shape
    /// every handshake authenticates against.
    fn test_registry() -> crate::api::SharedCredentialRegistry {
        let registry = crate::api::credentials::SharedCredentialRegistry::open(
            crate::api::credentials::test_registry_path("ws"),
        );
        registry.attach_managing_token(SharedWebSocketToken::new(TEST_TOKEN.to_string()));
        registry
    }

    #[test]
    fn authorize_rejects_missing_token() {
        let registry = test_registry();
        for query in [None, Some("other=1"), Some("token=")] {
            assert_eq!(
                authorize_ws_request(None, query, &registry).unwrap_err(),
                WsAuthError::MissingToken,
                "{query:?}"
            );
        }
    }

    #[test]
    fn authorize_rejects_malformed_authorization_headers() {
        let registry = test_registry();
        for header in ["Basic dXNlcjpwdw==", "Bearer", "Bearer   ", "token abc", ""] {
            assert_eq!(
                authorize_ws_request(Some(header), None, &registry).unwrap_err(),
                WsAuthError::MalformedAuthorization,
                "{header:?}"
            );
        }
    }

    #[test]
    fn authorize_rejects_tokens_that_are_in_no_registry() {
        let registry = test_registry();
        let wrong = [
            (Some("Bearer wrong-token".to_string()), None),
            (None, Some("token=wrong-token".to_string())),
            // Prefixes and extensions of a live credential must not pass.
            (None, Some(format!("token={TEST_TOKEN}x"))),
            (
                Some(format!("Bearer {}", &TEST_TOKEN[..TEST_TOKEN.len() - 1])),
                None,
            ),
        ];
        for (header, query) in wrong {
            assert_eq!(
                authorize_ws_request(header.as_deref(), query.as_deref(), &registry).unwrap_err(),
                WsAuthError::WrongToken,
                "{header:?} {query:?}"
            );
        }
    }

    #[test]
    fn authorize_accepts_the_managing_credential_by_header_or_query() {
        let registry = test_registry();
        for (header, query) in [
            (Some(format!("Bearer {TEST_TOKEN}")), None),
            // Scheme is case-insensitive per RFC 7235.
            (Some(format!("bearer {TEST_TOKEN}")), None),
            (None, Some(format!("token={TEST_TOKEN}"))),
            (None, Some(format!("a=1&token={TEST_TOKEN}&b=2"))),
        ] {
            let outcome =
                authorize_ws_request(header.as_deref(), query.as_deref(), &registry).unwrap();
            match outcome {
                crate::api::credentials::HandshakeOutcome::Live(credential) => assert_eq!(
                    credential.tier,
                    crate::api::schema::CredentialTier::Managing,
                    "{header:?} {query:?}"
                ),
                other => panic!("{header:?} {query:?}: expected a live credential, got {other:?}"),
            }
        }
    }

    /// ADR-0026: a minted limited credential opens its own connection. The
    /// handshake accepts it exactly like the pairing token; only what it may
    /// then manage differs.
    #[test]
    fn authorize_accepts_a_minted_limited_credential() {
        let registry = test_registry();
        let (info, token) = registry.mint(Some("desk browser".into())).unwrap();

        let outcome =
            authorize_ws_request(Some(&format!("Bearer {token}")), None, &registry).unwrap();

        match outcome {
            crate::api::credentials::HandshakeOutcome::Live(credential) => {
                assert_eq!(credential.credential_id, info.credential_id);
                assert_eq!(credential.tier, crate::api::schema::CredentialTier::Limited);
            }
            other => panic!("expected a live credential, got {other:?}"),
        }

        // Once revoked it stops being honored — but it is still recognized,
        // so the handshake admits it to say so rather than closing blind.
        registry.revoke_limited(&info.credential_id).unwrap();
        match authorize_ws_request(Some(&format!("Bearer {token}")), None, &registry).unwrap() {
            crate::api::credentials::HandshakeOutcome::Revoked { credential_id } => {
                assert_eq!(credential_id, info.credential_id)
            }
            other => panic!("expected a revoked verdict, got {other:?}"),
        }
        // A token this server never issued stays an opaque rejection.
        assert_eq!(
            authorize_ws_request(Some("Bearer never-minted"), None, &registry).unwrap_err(),
            WsAuthError::WrongToken
        );
    }

    #[test]
    fn authorize_prefers_header_over_query_and_fails_closed() {
        let registry = test_registry();
        // A malformed header must not fall back to a valid query token.
        assert_eq!(
            authorize_ws_request(
                Some("Basic abc"),
                Some(&format!("token={TEST_TOKEN}")),
                &registry
            )
            .unwrap_err(),
            WsAuthError::MalformedAuthorization
        );
        // A wrong header must not fall back either.
        assert_eq!(
            authorize_ws_request(
                Some("Bearer wrong"),
                Some(&format!("token={TEST_TOKEN}")),
                &registry
            )
            .unwrap_err(),
            WsAuthError::WrongToken
        );
    }

    #[test]
    fn shared_token_applies_a_valid_reloaded_token() {
        let shared = SharedWebSocketToken::new("old-token".to_string());

        let changed = shared
            .apply_reloaded_config(&spec_config(Some("127.0.0.1:4433"), Some("new-token")))
            .unwrap();

        assert!(changed);
        assert_eq!(shared.current(), "new-token");
    }

    #[test]
    fn shared_token_reports_unchanged_for_the_same_token() {
        let shared = SharedWebSocketToken::new("same-token".to_string());

        let changed = shared
            .apply_reloaded_config(&spec_config(Some("127.0.0.1:4433"), Some("same-token")))
            .unwrap();

        assert!(!changed);
        assert_eq!(shared.current(), "same-token");
    }

    #[test]
    fn shared_token_keeps_current_token_when_reload_removes_the_section() {
        let shared = SharedWebSocketToken::new("kept-token".to_string());

        for config in [
            spec_config(None, Some("new-token")),
            spec_config(Some(""), Some("new-token")),
        ] {
            let err = shared.apply_reloaded_config(&config).unwrap_err();
            assert!(err.contains("until the server restarts"), "{err}");
            assert_eq!(shared.current(), "kept-token");
        }
    }

    #[test]
    fn shared_token_keeps_current_token_when_reload_omits_or_breaks_the_token() {
        let shared = SharedWebSocketToken::new("kept-token".to_string());

        for token in [None, Some(""), Some("has space"), Some("has+plus")] {
            let err = shared
                .apply_reloaded_config(&spec_config(Some("127.0.0.1:4433"), token))
                .unwrap_err();
            assert!(err.contains("keeps its current token"), "{token:?}: {err}");
            assert_eq!(shared.current(), "kept-token");
        }
    }

    #[test]
    fn start_returns_none_when_not_configured() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel();
        let handle = start_websocket_server_with_capabilities(
            &WebSocketApiConfig::default(),
            api_tx,
            EventHub::default(),
            test_registry(),
            None,
            crate::api::SharedServerName::new(TEST_SERVER_NAME.to_string()),
            crate::api::SharedServerReach::from_config(&WebSocketApiConfig::default()),
        )
        .unwrap();
        assert!(handle.is_none());
    }

    struct TestServer {
        handle: WebSocketServerHandle,
        server_name: crate::api::SharedServerName,
        credentials: crate::api::SharedCredentialRegistry,
        _api_rx: mpsc::UnboundedReceiver<ApiRequestMessage>,
        event_hub: EventHub,
    }

    fn start_test_server() -> TestServer {
        start_test_server_with_capabilities(None)
    }

    fn start_test_server_with_capabilities(capabilities: Option<ServerCapabilities>) -> TestServer {
        let (api_tx, api_rx) = mpsc::unbounded_channel();
        let event_hub = EventHub::default();
        let server_name = crate::api::SharedServerName::new(TEST_SERVER_NAME.to_string());
        let config = spec_config(Some("127.0.0.1:0"), Some(TEST_TOKEN));
        let credentials = test_registry();
        let handle = start_websocket_server_with_capabilities(
            &config,
            api_tx,
            event_hub.clone(),
            credentials.clone(),
            capabilities,
            server_name.clone(),
            crate::api::SharedServerReach::from_config(&config),
        )
        .unwrap()
        .expect("listener should start when configured");
        TestServer {
            handle,
            server_name,
            credentials,
            _api_rx: api_rx,
            event_hub,
        }
    }

    fn connect_authorized(server: &TestServer) -> WebSocket<MaybeTlsStream<TcpStream>> {
        let url = format!("ws://{}", server.handle.local_addr());
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        let (websocket, _response) = tungstenite::connect(request).unwrap();
        set_client_read_timeout(&websocket);
        websocket
    }

    fn set_client_read_timeout(websocket: &WebSocket<MaybeTlsStream<TcpStream>>) {
        if let MaybeTlsStream::Plain(stream) = websocket.get_ref() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
    }

    fn read_json(websocket: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> serde_json::Value {
        loop {
            match websocket.read().unwrap() {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected websocket message: {other:?}"),
            }
        }
    }

    /// The listener's half of ADR-0026: a minted credential connects like
    /// any other, and a revoke reaches the connection it already holds —
    /// refused by code on its very next request. What that credential meets
    /// when it comes back is
    /// [`a_revoked_credential_connects_and_is_told_so_in_json`].
    #[test]
    fn a_revoked_credential_is_refused_on_its_next_request() {
        let server = start_test_server();
        let (info, token) = server.credentials.mint(Some("browser".into())).unwrap();

        let url = format!("ws://{}", server.handle.local_addr());
        let mut request = url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let (mut websocket, _response) = tungstenite::connect(request).unwrap();
        set_client_read_timeout(&websocket);

        websocket
            .send(Message::text(
                r#"{"id":"req_live","method":"ping","params":{}}"#,
            ))
            .unwrap();
        assert_eq!(read_json(&mut websocket)["result"]["type"], "pong");

        server
            .credentials
            .revoke_limited(&info.credential_id)
            .unwrap();

        websocket
            .send(Message::text(
                r#"{"id":"req_after_revoke","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let refused = read_json(&mut websocket);
        assert_eq!(refused["id"], "req_after_revoke");
        assert_eq!(refused["error"]["code"], "credential_revoked", "{refused}");
        // The managing credential is untouched by the browser's revoke.
        let mut managing = connect_authorized(&server);
        managing
            .send(Message::text(
                r#"{"id":"req_managing_after_revoke","method":"ping","params":{}}"#,
            ))
            .unwrap();
        assert_eq!(read_json(&mut managing)["result"]["type"], "pong");
        let _ = url;
    }

    /// A browser cannot see why a handshake failed, so a credential this
    /// server revoked must be let onto the socket and told in JSON — and
    /// told nothing else. An arbitrary bad token keeps its opaque 401.
    #[test]
    fn a_revoked_credential_connects_and_is_told_so_in_json() {
        let server = start_test_server();
        let (info, token) = server.credentials.mint(Some("browser".into())).unwrap();
        server
            .credentials
            .revoke_limited(&info.credential_id)
            .unwrap();

        let url = format!("ws://{}", server.handle.local_addr());
        let mut request = url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let (mut websocket, _response) =
            tungstenite::connect(request).expect("a revoked credential is let on to be told why");
        set_client_read_timeout(&websocket);

        // Any request it makes gets the verdict, not an answer.
        websocket
            .send(Message::text(
                r#"{"id":"req_revoked_reconnect","method":"pane.list","params":{}}"#,
            ))
            .unwrap();
        let refused = read_json(&mut websocket);
        assert_eq!(refused["id"], "req_revoked_reconnect");
        assert_eq!(refused["error"]["code"], "credential_revoked", "{refused}");
        assert!(refused.get("result").is_none(), "{refused}");

        // The verdict is terminal: the connection ends, having served
        // nothing else.
        let mut closed = false;
        for _ in 0..50 {
            match websocket.read() {
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                    closed = true;
                    break;
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                Ok(other) => panic!("a revoked connection served something: {other:?}"),
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => panic!("unexpected websocket error: {err}"),
            }
        }
        assert!(
            closed,
            "the revoked connection must be closed after its verdict"
        );

        // A token this server never minted is still an opaque rejection.
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            "Bearer never-minted-token".parse().unwrap(),
        );
        match tungstenite::connect(request).unwrap_err() {
            tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED)
            }
            other => panic!("expected http 401 rejection, got: {other:?}"),
        }
    }

    /// The capability is how a client tells a registry-bearing server from
    /// one that would refuse the verbs as unknown methods.
    #[test]
    fn pong_advertises_the_credential_registry_capability() {
        let server = start_test_server_with_capabilities(Some(ServerCapabilities {
            live_handoff: false,
            detached_server_daemon: false,
            send_affirm: true,
            stream_multiplex: true,
            credential_registry: true,
        }));
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"req_capability","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let pong = read_json(&mut websocket);

        assert_eq!(pong["result"]["capabilities"]["credential_registry"], true);
        assert_eq!(
            pong["result"]["protocol"],
            crate::protocol::PROTOCOL_VERSION,
            "the registry is additive; the protocol version does not move"
        );
    }

    #[test]
    fn handshake_without_token_is_rejected_with_401_before_dispatch() {
        let mut server = start_test_server();

        let url = format!("ws://{}", server.handle.local_addr());
        let err = tungstenite::connect(url).unwrap_err();
        match err {
            tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected http 401 rejection, got: {other:?}"),
        }

        // Auth runs inside the upgrade callback; nothing may reach dispatch.
        assert!(server._api_rx.try_recv().is_err());
    }

    #[test]
    fn handshake_with_wrong_token_is_rejected_with_401_before_dispatch() {
        let mut server = start_test_server();

        let url = format!("ws://{}", server.handle.local_addr());
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            "Bearer wrong-token".parse().unwrap(),
        );
        let err = tungstenite::connect(request).unwrap_err();
        match err {
            tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected http 401 rejection, got: {other:?}"),
        }

        assert!(server._api_rx.try_recv().is_err());
    }

    #[test]
    fn authorized_connection_answers_sequential_requests() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        for id in ["req_1", "req_2"] {
            websocket
                .send(Message::text(format!(
                    r#"{{"id":"{id}","method":"ping","params":{{}}}}"#
                )))
                .unwrap();
            let response = read_json(&mut websocket);
            assert_eq!(response["id"], id);
            assert_eq!(response["result"]["type"], "pong");
            assert_eq!(
                response["result"]["protocol"],
                crate::protocol::PROTOCOL_VERSION
            );
            assert_eq!(response["result"]["name"], TEST_SERVER_NAME);
        }
    }

    #[test]
    fn query_with_unknown_parameters_still_authenticates() {
        // The pairing URL gained a `name` parameter after the first fork
        // release. The listener reads only the token from the query, so a
        // client that replays the whole query — or a payload that grows more
        // parameters later — must keep connecting. Regression pin.
        let server = start_test_server();

        let url = format!(
            "ws://{}/?token={TEST_TOKEN}&name=some%20server&future=1",
            server.handle.local_addr()
        );
        let (mut websocket, _response) = tungstenite::connect(url).unwrap();
        set_client_read_timeout(&websocket);

        websocket
            .send(Message::text(
                r#"{"id":"req_extra","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let response = read_json(&mut websocket);
        assert_eq!(response["id"], "req_extra");
        assert_eq!(response["result"]["type"], "pong");
    }

    #[test]
    fn renaming_the_server_shows_in_the_next_pong_without_rebinding() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        let pong = |websocket: &mut WebSocket<MaybeTlsStream<TcpStream>>, id: &str| {
            websocket
                .send(Message::text(format!(
                    r#"{{"id":"{id}","method":"ping","params":{{}}}}"#
                )))
                .unwrap();
            read_json(websocket)
        };

        assert_eq!(
            pong(&mut websocket, "req_named")["result"]["name"],
            TEST_SERVER_NAME
        );

        let changed = server
            .server_name
            .apply_reloaded_config(&WebSocketApiConfig {
                name: Some("renamed-server".to_string()),
                ..spec_config(Some("127.0.0.1:0"), Some(TEST_TOKEN))
            });
        assert!(changed);

        // Even the connection opened before the rename sees the new name.
        assert_eq!(
            pong(&mut websocket, "req_renamed")["result"]["name"],
            "renamed-server"
        );
    }

    #[test]
    fn query_parameter_token_authenticates_browser_style_clients() {
        let server = start_test_server();

        let url = format!("ws://{}/?token={TEST_TOKEN}", server.handle.local_addr());
        let (mut websocket, _response) = tungstenite::connect(url).unwrap();
        set_client_read_timeout(&websocket);

        websocket
            .send(Message::text(
                r#"{"id":"req_q","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let response = read_json(&mut websocket);
        assert_eq!(response["id"], "req_q");
        assert_eq!(response["result"]["type"], "pong");
    }

    #[test]
    fn invalid_json_gets_error_frame_and_connection_stays_usable() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket.send(Message::text("this is not json")).unwrap();
        let error = read_json(&mut websocket);
        assert_eq!(error["error"]["code"], "invalid_request");

        websocket
            .send(Message::text(
                r#"{"id":"req_after","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let response = read_json(&mut websocket);
        assert_eq!(response["id"], "req_after");
        assert_eq!(response["result"]["type"], "pong");
    }

    #[test]
    fn malformed_pane_send_mouse_gets_normal_invalid_request_refusals() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        for request in [
            r#"{"id":"bad_action","method":"pane.send_mouse","params":{"pane_id":"pane_1","action":"drag","row":0,"col":0}}"#,
            r#"{"id":"missing_row","method":"pane.send_mouse","params":{"pane_id":"pane_1","action":"click","col":0}}"#,
        ] {
            websocket.send(Message::text(request)).unwrap();
            let error = read_json(&mut websocket);
            assert_eq!(error["id"], "");
            assert_eq!(error["error"]["code"], "invalid_request");
        }

        websocket
            .send(Message::text(
                r#"{"id":"req_after_mouse_error","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let response = read_json(&mut websocket);
        assert_eq!(response["id"], "req_after_mouse_error");
        assert_eq!(response["result"]["type"], "pong");
    }

    #[test]
    fn events_subscribe_streams_events_pushed_to_the_hub() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"sub_ws","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#,
            ))
            .unwrap();
        let ack = read_json(&mut websocket);
        assert_eq!(ack["id"], "sub_ws");
        assert_eq!(ack["result"]["type"], "subscription_started");

        server.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w1".to_string(),
            },
        });

        let event = read_json(&mut websocket);
        assert_eq!(event["event"], "workspace_focused");
        assert_eq!(event["data"]["workspace_id"], "w1");
    }

    /// The phone keeps one websocket open: it subscribes once and then issues
    /// ordinary requests on that same connection. Before this was fixed, the
    /// streaming loop's close probe consumed any inbound request frame and
    /// read it as "peer went away" — so the request vanished with no response
    /// and no error, and the subscription died silently while the socket
    /// stayed open. The client's next event never came.
    #[test]
    fn subscription_connection_answers_interleaved_requests_and_keeps_streaming() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"sub_live","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"live_only":true}}"#,
            ))
            .unwrap();
        let ack = read_json(&mut websocket);
        assert_eq!(ack["result"]["type"], "subscription_started");

        // A request sent while the subscription streams must be answered.
        websocket
            .send(Message::text(
                r#"{"id":"ping_during_stream","method":"ping","params":{}}"#,
            ))
            .unwrap();
        let response = read_json(&mut websocket);
        assert_eq!(response["id"], "ping_during_stream");
        assert_eq!(response["result"]["type"], "pong");

        // And the subscription must still be alive afterwards.
        server.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w_after_interleaved_request".to_string(),
            },
        });

        let event = read_json(&mut websocket);
        assert_eq!(event["event"], "workspace_focused");
        assert_eq!(event["data"]["workspace_id"], "w_after_interleaved_request");
    }

    /// One connection still serves one stream. A second streaming request
    /// arriving mid-stream is refused out loud instead of being swallowed,
    /// and refusing it must not take down the stream already running.
    #[test]
    fn interleaved_stream_request_is_refused_without_killing_the_live_subscription() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"sub_first","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"live_only":true}}"#,
            ))
            .unwrap();
        let ack = read_json(&mut websocket);
        assert_eq!(ack["result"]["type"], "subscription_started");

        websocket
            .send(Message::text(
                r#"{"id":"sub_second","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#,
            ))
            .unwrap();
        let refusal = read_json(&mut websocket);
        assert_eq!(refusal["id"], "sub_second");
        assert_eq!(refusal["error"]["code"], "stream_busy");

        server.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w_after_refusal".to_string(),
            },
        });

        let event = read_json(&mut websocket);
        assert_eq!(event["event"], "workspace_focused");
        assert_eq!(event["data"]["workspace_id"], "w_after_refusal");
    }

    /// The issue's run 2: a malformed frame sent on a subscription connection
    /// was swallowed with no error at all, while the same malformed frame sent
    /// before subscribing did get `invalid_request`. Malformed input must be
    /// refused identically either side of a stream starting.
    #[test]
    fn malformed_frame_during_a_stream_gets_the_same_invalid_request_refusal() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"sub_malformed","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"live_only":true}}"#,
            ))
            .unwrap();
        let ack = read_json(&mut websocket);
        assert_eq!(ack["result"]["type"], "subscription_started");

        websocket.send(Message::text("this is not json")).unwrap();
        let refusal = read_json(&mut websocket);
        assert_eq!(refusal["error"]["code"], "invalid_request");

        // The issue's run 2 called this ping "malformed" for its unexpected
        // fields, but `ping` ignores unknown params — so the honest contract
        // is that it is answered normally, which is what it never was.
        websocket
            .send(Message::text(
                r#"{"id":"ping_extra_fields","method":"ping","params":{"unexpected":true}}"#,
            ))
            .unwrap();
        let response = read_json(&mut websocket);
        assert_eq!(response["id"], "ping_extra_fields");
        assert_eq!(response["result"]["type"], "pong");

        server.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w_after_malformed".to_string(),
            },
        });

        let event = read_json(&mut websocket);
        assert_eq!(event["data"]["workspace_id"], "w_after_malformed");
    }

    /// Binary frames are rejected the same way mid-stream as they are between
    /// requests, and the rejection is not a reason to drop the subscription.
    #[test]
    fn binary_frame_during_a_stream_is_refused_without_killing_the_subscription() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"sub_binary","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"live_only":true}}"#,
            ))
            .unwrap();
        let ack = read_json(&mut websocket);
        assert_eq!(ack["result"]["type"], "subscription_started");

        websocket
            .send(Message::Binary(vec![1, 2, 3].into()))
            .unwrap();
        let refusal = read_json(&mut websocket);
        assert_eq!(refusal["error"]["code"], "invalid_request");

        server.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w_after_binary".to_string(),
            },
        });

        let event = read_json(&mut websocket);
        assert_eq!(event["data"]["workspace_id"], "w_after_binary");
    }

    /// A client that closes mid-stream must still end the stream promptly.
    #[test]
    fn close_frame_during_a_stream_still_ends_the_connection() {
        let server = start_test_server();
        let mut websocket = connect_authorized(&server);

        websocket
            .send(Message::text(
                r#"{"id":"sub_close","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"live_only":true}}"#,
            ))
            .unwrap();
        let ack = read_json(&mut websocket);
        assert_eq!(ack["result"]["type"], "subscription_started");

        websocket.close(None).unwrap();
        let started = Instant::now();
        loop {
            match websocket.read() {
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
                Ok(_) => continue,
                Err(err) => panic!("unexpected websocket error: {err:?}"),
            }
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn dropping_the_handle_releases_the_port() {
        let server = start_test_server();
        let addr = server.handle.local_addr();
        drop(server);

        let rebound = TcpListener::bind(addr);
        assert!(rebound.is_ok(), "port should be released: {rebound:?}");
    }

    #[test]
    fn rotating_the_token_rejects_the_old_token_without_rebinding() {
        let server = start_test_server();

        // A connection authorized before the rotation stays usable: auth
        // happens at the handshake, like a Unix socket permission check.
        let mut pre_rotation = connect_authorized(&server);

        let rotated = server
            .handle
            .shared_token()
            .apply_reloaded_config(&spec_config(Some("127.0.0.1:0"), Some("rotated-token")))
            .unwrap();
        assert!(rotated);

        // The previous token is rejected at the very next handshake.
        let url = format!("ws://{}", server.handle.local_addr());
        let mut request = url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        match tungstenite::connect(request).unwrap_err() {
            tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected http 401 rejection, got: {other:?}"),
        }

        // The rotated token authenticates.
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            "Bearer rotated-token".parse().unwrap(),
        );
        let (mut websocket, _response) = tungstenite::connect(request).unwrap();
        set_client_read_timeout(&websocket);
        websocket
            .send(Message::text(
                r#"{"id":"req_rotated","method":"ping","params":{}}"#,
            ))
            .unwrap();
        assert_eq!(read_json(&mut websocket)["result"]["type"], "pong");

        pre_rotation
            .send(Message::text(
                r#"{"id":"req_pre","method":"ping","params":{}}"#,
            ))
            .unwrap();
        assert_eq!(read_json(&mut pre_rotation)["result"]["type"], "pong");
    }
}
