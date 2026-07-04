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
//! - `events.subscribe` dedicates the connection to the event stream until
//!   the client disconnects, exactly like the Unix socket.

use std::io;
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
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use crate::api::schema::ServerCapabilities;
use crate::api::server::{
    handle_parsed_request, is_connection_closed_error, parse_api_request, ApiTransport,
    CONNECTION_POLL_INTERVAL, INITIAL_REQUEST_TIMEOUT, MAX_INITIAL_REQUEST_BYTES,
};
use crate::api::{ApiRequestSender, EventHub};
use crate::config::WebSocketApiConfig;

/// Overall budget for completing the HTTP upgrade, including auth.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-syscall socket timeout during the blocking handshake phase.
const HANDSHAKE_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for flushing one outgoing frame to a slow client.
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
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

/// Validate handshake credentials. The Authorization header is authoritative
/// when present; the `token` query parameter exists for clients that cannot
/// set headers on a WebSocket upgrade (browsers). A malformed header fails
/// closed instead of falling back to the query parameter.
pub(crate) fn authorize_ws_request(
    authorization: Option<&str>,
    query: Option<&str>,
    expected_token: &str,
) -> Result<(), WsAuthError> {
    if let Some(authorization) = authorization {
        let Some(presented) = bearer_token(authorization) else {
            return Err(WsAuthError::MalformedAuthorization);
        };
        return check_token(presented, expected_token);
    }

    if let Some(presented) = query_token(query) {
        return check_token(presented, expected_token);
    }

    Err(WsAuthError::MissingToken)
}

fn check_token(presented: &str, expected: &str) -> Result<(), WsAuthError> {
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(WsAuthError::WrongToken)
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

/// Compare tokens without an early exit, so the comparison time does not
/// depend on how many leading bytes match. Length mismatches return early;
/// leaking the token length is acceptable.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

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
) -> io::Result<Option<WebSocketServerHandle>> {
    start_websocket_server_with_capabilities(
        config,
        api_tx,
        event_hub,
        Some(ServerCapabilities {
            live_handoff: crate::platform::capabilities().live_handoff,
            detached_server_daemon: crate::platform::current_process_is_detached_server_daemon(),
        }),
        server_name,
    )
}

/// Like [`start_websocket_server`], with explicit ping capabilities. Call
/// sites must pass the same capabilities and shared name slot as their Unix
/// socket listener so `ping` responses are identical over both transports.
pub fn start_websocket_server_with_capabilities(
    config: &WebSocketApiConfig,
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    capabilities: Option<ServerCapabilities>,
    server_name: crate::api::SharedServerName,
) -> io::Result<Option<WebSocketServerHandle>> {
    let Some(spec) = websocket_api_spec(config)? else {
        return Ok(None);
    };

    let listener = bind_with_addr_in_use_retry(spec.addr)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    let token = SharedWebSocketToken::new(spec.token);
    let accept_token = token.clone();

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
                    let connection_running = Arc::clone(&listener_running);
                    let token = accept_token.clone();
                    std::thread::spawn(move || {
                        if let Err(err) = handle_ws_connection(
                            stream,
                            &token,
                            &api_tx,
                            &event_hub,
                            &connection_running,
                            capabilities,
                            &server_name,
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
                    std::thread::sleep(CONNECTION_POLL_INTERVAL);
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
    expected_token: &SharedWebSocketToken,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_name: &crate::api::SharedServerName,
) -> io::Result<()> {
    let peer = stream.peer_addr().ok();
    stream.set_read_timeout(Some(HANDSHAKE_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_IO_TIMEOUT))?;

    let mut auth_error = None;
    let websocket = match accept_websocket(stream, expected_token, &mut auth_error) {
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

    // Handshake done; switch to non-blocking polling like the rest of the
    // API server so the connection observes server shutdown promptly.
    websocket.get_ref().set_nonblocking(true)?;
    let mut transport = WsTransport { websocket };

    let result = ws_request_loop(
        &mut transport,
        api_tx,
        event_hub,
        running,
        capabilities,
        server_name,
    );

    // Best effort: tell well-behaved clients the server is done.
    let _ = transport.websocket.close(None);
    let _ = transport.websocket.flush();

    match result {
        Err(err) if is_connection_closed_error(&err) => Ok(()),
        result => result,
    }
}

fn format_peer(peer: Option<SocketAddr>) -> String {
    peer.map(|addr| addr.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn accept_websocket(
    stream: TcpStream,
    expected_token: &SharedWebSocketToken,
    auth_error: &mut Option<WsAuthError>,
) -> Result<WebSocket<TcpStream>, tungstenite::Error> {
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

        // Read the expected token at handshake time, not listener start, so a
        // rotated token is enforced on the very next connection attempt.
        let expected_token = expected_token.current();
        match authorize_ws_request(authorization, request.uri().query(), &expected_token) {
            Ok(()) => Ok(response),
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
) -> io::Result<()> {
    // Parity with the Unix socket: a client that completes the handshake
    // gets a bounded window to send its first request. Once the connection
    // has proven itself it may idle between requests.
    let mut first_request_deadline = Some(Instant::now() + INITIAL_REQUEST_TIMEOUT);

    loop {
        if !running.load(Ordering::Relaxed) {
            return Ok(());
        }

        match transport.websocket.read() {
            Ok(Message::Text(text)) => {
                first_request_deadline = None;
                let Some(request) = parse_api_request(transport, text.as_str())? else {
                    continue;
                };
                handle_parsed_request(
                    request,
                    transport,
                    api_tx,
                    event_hub,
                    running,
                    capabilities.clone(),
                    None,
                    server_name,
                )?;
            }
            Ok(Message::Binary(_)) => {
                first_request_deadline = None;
                transport.write_message(
                    r#"{"id":"","error":{"code":"invalid_request","message":"invalid request: binary frames are not supported; send one JSON message per text frame"}}"#,
                )?;
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                // tungstenite queues the pong reply internally; flush it.
                flush_ignore_would_block(&mut transport.websocket)?;
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
                    std::thread::sleep(CONNECTION_POLL_INTERVAL);
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

fn flush_ignore_would_block(websocket: &mut WebSocket<TcpStream>) -> io::Result<()> {
    match websocket.flush() {
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

struct WsTransport {
    websocket: WebSocket<TcpStream>,
}

impl ApiTransport for WsTransport {
    fn write_message(&mut self, message: &str) -> io::Result<()> {
        let deadline = Instant::now() + FRAME_WRITE_TIMEOUT;
        let mut result = self.websocket.send(Message::text(message));
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
                        std::thread::sleep(Duration::from_millis(10));
                        result = self.websocket.flush();
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

    fn probe_closed(&mut self) -> io::Result<bool> {
        loop {
            match self.websocket.read() {
                // Control frames keep the connection alive; anything else
                // from a client that started a stream forfeits the
                // connection, matching the Unix socket probe.
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    flush_ignore_would_block(&mut self.websocket)?;
                }
                Ok(Message::Close(_)) => return Ok(true),
                Ok(_) => return Ok(true),
                Err(err) => {
                    return match classify_ws_error(err) {
                        WsErrorClass::WouldBlock => Ok(false),
                        WsErrorClass::Closed => Ok(true),
                        WsErrorClass::Failed(err) => Err(err),
                    }
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

    fn spec_config(bind: Option<&str>, token: Option<&str>) -> WebSocketApiConfig {
        WebSocketApiConfig {
            bind: bind.map(str::to_string),
            token: token.map(str::to_string),
            name: None,
        }
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
    fn authorize_rejects_missing_token() {
        assert_eq!(
            authorize_ws_request(None, None, TEST_TOKEN),
            Err(WsAuthError::MissingToken)
        );
        assert_eq!(
            authorize_ws_request(None, Some("other=1"), TEST_TOKEN),
            Err(WsAuthError::MissingToken)
        );
        assert_eq!(
            authorize_ws_request(None, Some("token="), TEST_TOKEN),
            Err(WsAuthError::MissingToken)
        );
    }

    #[test]
    fn authorize_rejects_malformed_authorization_headers() {
        for header in ["Basic dXNlcjpwdw==", "Bearer", "Bearer   ", "token abc", ""] {
            assert_eq!(
                authorize_ws_request(Some(header), None, TEST_TOKEN),
                Err(WsAuthError::MalformedAuthorization),
                "{header:?}"
            );
        }
    }

    #[test]
    fn authorize_rejects_wrong_tokens() {
        assert_eq!(
            authorize_ws_request(Some("Bearer wrong-token"), None, TEST_TOKEN),
            Err(WsAuthError::WrongToken)
        );
        assert_eq!(
            authorize_ws_request(None, Some("token=wrong-token"), TEST_TOKEN),
            Err(WsAuthError::WrongToken)
        );
        // Prefixes and extensions of the real token must not pass.
        assert_eq!(
            authorize_ws_request(None, Some(&format!("token={TEST_TOKEN}x")), TEST_TOKEN),
            Err(WsAuthError::WrongToken)
        );
        assert_eq!(
            authorize_ws_request(
                Some(&format!("Bearer {}", &TEST_TOKEN[..TEST_TOKEN.len() - 1])),
                None,
                TEST_TOKEN
            ),
            Err(WsAuthError::WrongToken)
        );
    }

    #[test]
    fn authorize_accepts_valid_bearer_header() {
        assert_eq!(
            authorize_ws_request(Some(&format!("Bearer {TEST_TOKEN}")), None, TEST_TOKEN),
            Ok(())
        );
        // Scheme is case-insensitive per RFC 7235.
        assert_eq!(
            authorize_ws_request(Some(&format!("bearer {TEST_TOKEN}")), None, TEST_TOKEN),
            Ok(())
        );
    }

    #[test]
    fn authorize_accepts_valid_query_token() {
        assert_eq!(
            authorize_ws_request(None, Some(&format!("token={TEST_TOKEN}")), TEST_TOKEN),
            Ok(())
        );
        assert_eq!(
            authorize_ws_request(
                None,
                Some(&format!("a=1&token={TEST_TOKEN}&b=2")),
                TEST_TOKEN
            ),
            Ok(())
        );
    }

    #[test]
    fn authorize_prefers_header_over_query_and_fails_closed() {
        // A malformed header must not fall back to a valid query token.
        assert_eq!(
            authorize_ws_request(
                Some("Basic abc"),
                Some(&format!("token={TEST_TOKEN}")),
                TEST_TOKEN
            ),
            Err(WsAuthError::MalformedAuthorization)
        );
        // A wrong header must not fall back either.
        assert_eq!(
            authorize_ws_request(
                Some("Bearer wrong"),
                Some(&format!("token={TEST_TOKEN}")),
                TEST_TOKEN
            ),
            Err(WsAuthError::WrongToken)
        );
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
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
            None,
            crate::api::SharedServerName::new(TEST_SERVER_NAME.to_string()),
        )
        .unwrap();
        assert!(handle.is_none());
    }

    struct TestServer {
        handle: WebSocketServerHandle,
        server_name: crate::api::SharedServerName,
        _api_rx: mpsc::UnboundedReceiver<ApiRequestMessage>,
        event_hub: EventHub,
    }

    fn start_test_server() -> TestServer {
        let (api_tx, api_rx) = mpsc::unbounded_channel();
        let event_hub = EventHub::default();
        let server_name = crate::api::SharedServerName::new(TEST_SERVER_NAME.to_string());
        let handle = start_websocket_server_with_capabilities(
            &spec_config(Some("127.0.0.1:0"), Some(TEST_TOKEN)),
            api_tx,
            event_hub.clone(),
            None,
            server_name.clone(),
        )
        .unwrap()
        .expect("listener should start when configured");
        TestServer {
            handle,
            server_name,
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
