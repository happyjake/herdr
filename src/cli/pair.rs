//! `herdr pair` — mint the WebSocket API bearer token and print the pairing
//! payload (endpoint URL + token + server name) as a terminal QR code and as
//! plaintext.
//!
//! The token is stored in `[websocket_api].token` in config.toml, so the
//! server honors it across restarts. Re-running the command mints a fresh
//! token and replaces the stored one; the server that reads that config file
//! is asked to reload it so the previous token stops authenticating
//! immediately, and the new token is then presented to the listener, so what
//! the command reports is what a client would find. When the listener is not
//! configured the command explains what to enable instead of printing a
//! payload that cannot work.
//!
//! # Known limits of the proof
//!
//! These are accepted, not open work. Each one is a thing this command cannot
//! observe, and the rule it follows is to say so rather than to guess:
//!
//! - **A `wss://` payload is never dialed.** This build carries no TLS client
//!   and will not grow one for a CLI message; the proof falls back to the
//!   listener behind the proxy and every outcome names the printed url as not
//!   dialed.
//! - **A refused handshake identifies nobody.** The listener's answer to an
//!   unknown token is deliberately opaque, so it is indistinguishable from any
//!   other service's refusal. Attributing a refusal would need protocol
//!   evidence in the rejection, which is a server change, not a pairing one.
//! - **No server can be tied to a config file.** A config path and a socket
//!   path are set independently and nothing records the pairing, so the reload
//!   is an ordered attempt and never evidence about which server serves the
//!   payload.
//! - **A reload reply that fails describes the reply.** A server can apply a
//!   config perfectly and still answer unreadably, so nothing about server
//!   state is inferred from a failed request.
//! - **The bounds are wall-clock, and their workers outlive them.** A stalled
//!   resolver or control socket is abandoned on its thread, which ends with
//!   the process; the command reports a named outcome rather than waiting.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget};
use crate::api::schema::{
    EmptyParams, Method, PingParams, Request, ResponseResult, SuccessResponse,
};

/// 256 bits of OS randomness per token. Encoded as base64url without
/// padding (43 characters), which stays inside the URL-unreserved charset
/// the listener requires, so the token never needs escaping.
const TOKEN_BYTES: usize = 32;

/// Bounds on the token proof. Every step of it can stall — a resolver, a
/// half-open hop in front of a proxy, a peer that upgrades and then says
/// nothing — and a command that hangs leaves an operator with no outcome at
/// all, which is worse than a named unproven one. Each step is capped, so the
/// whole proof ends in a sentence.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long one control socket gets to answer a reload before pairing moves
/// on to the next candidate. A stale process that accepts and never answers
/// is a real shape, and waiting on it forever would keep the proof — the part
/// the report rests on — from ever running.
const CONTROL_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(3);

/// How many frames to skip while waiting for the answer to the identifying
/// ping. Control frames and any unsolicited traffic are stepped over; a peer
/// that only chatters is one that never answered.
const PING_FRAME_BUDGET: usize = 8;

pub(super) fn run_pair_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        None => {}
        Some("help" | "--help" | "-h") => {
            print_pair_help();
            return Ok(0);
        }
        Some(other) => {
            eprintln!("unknown option: {other}");
            print_pair_help();
            return Ok(2);
        }
    }

    let config_path = crate::config::config_path();
    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    // The server falls back to defaults (listener off) when the config does
    // not parse, so a payload printed from an unparseable config would be
    // dead. Refuse and explain instead.
    let config = match toml::from_str::<crate::config::Config>(&content) {
        Ok(config) => config,
        Err(err) => {
            eprintln!(
                "config file at {} could not be parsed as herdr config: {err}",
                config_path.display()
            );
            eprintln!("Fix it before pairing; the server ignores an unparseable config and starts without the websocket listener.");
            return Ok(1);
        }
    };

    // The advertised endpoint is resolved first because it decides how much
    // the bind address still has to carry: once a proxy fronts the listener,
    // the bind is only the local socket to install the token on.
    let advertised = match advertised_endpoint(config.websocket_api.advertised_endpoint.as_deref())
    {
        Ok(advertised) => advertised,
        Err(unavailable) => {
            eprint!("{}", unavailable.explanation(&config_path));
            return Ok(1);
        }
    };

    let addr = match pairing_endpoint(config.websocket_api.bind.as_deref(), advertised.is_some()) {
        Ok(addr) => addr,
        Err(unavailable) => {
            eprint!("{}", unavailable.explanation(&config_path));
            return Ok(1);
        }
    };

    let advertised_was_declared = advertised.is_some();
    let endpoint = advertised.unwrap_or_else(|| format!("ws://{addr}"));
    // A wildcard listener has no address to dial as written; locally it
    // answers on loopback at the same port.
    let local_listener = local_listener_address(addr);

    let name = crate::api::resolve_server_name(&config.websocket_api);

    let token = mint_token()?;
    if !crate::api::valid_websocket_token_chars(&token) {
        return Err(std::io::Error::other(
            "minted token contains characters the listener would reject; this is a bug in herdr",
        ));
    }

    let updated = rotate_token_in_config(&content, &token);
    if let Err(err) = updated.parse::<toml::Value>() {
        eprintln!(
            "storing the token would make {} invalid TOML: {err}; leaving config unchanged",
            config_path.display()
        );
        return Ok(1);
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_path, updated)?;

    let previous_token = config
        .websocket_api
        .token
        .as_deref()
        .filter(|token| !token.is_empty());
    let previous_token_existed = previous_token.is_some();
    // The proof exercises the url the operator is handed, so it is planned
    // from the payload endpoint rather than from the bind address.
    let check = plan_token_check(&endpoint, advertised_was_declared, local_listener);
    let activation = apply_token_to_running_server(&config_path, &check, &token, previous_token);

    let payload = PairingPayload {
        endpoint,
        token,
        name,
    };
    match qr_code_text(&payload.url()) {
        Ok(qr) => {
            println!("Scan with the herdr mobile client:");
            println!();
            println!("{qr}");
        }
        Err(err) => eprintln!("warning: {err}; use the plaintext payload below"),
    }
    println!("  endpoint  {}", payload.endpoint);
    println!("  name      {}", payload.name);
    println!("  token     {}", payload.token);
    println!();
    println!("  {}", payload.url());
    println!();

    let (report, exit_code) = activation_report(
        &activation,
        previous_token_existed,
        &check.scope,
        &config_path,
    );
    println!("{report}");
    Ok(exit_code)
}

/// The pairing payload: the WebSocket endpoint, the bearer token, and the
/// server's display name. The QR encodes [`Self::url`], a directly
/// connectable form — the listener accepts the token as a query parameter
/// and ignores the rest — that also carries the fields for clients that
/// prefer `Authorization: Bearer` header auth. The name and token ride only
/// the scannable URL: the endpoint stays bare, so pasting it never leaks a
/// credential and renaming never changes a server's identity.
struct PairingPayload {
    /// Base URL without a trailing slash, e.g. `ws://100.64.0.5:4433` or
    /// `wss://a-host.example.net/herdr-ws`.
    endpoint: String,
    token: String,
    name: String,
}

impl PairingPayload {
    fn url(&self) -> String {
        // The token charset is URL-unreserved by construction, so the query
        // form needs no percent-encoding; the free-form name does.
        format!(
            "{}{}token={}&name={}",
            self.endpoint,
            query_prefix(&self.endpoint),
            self.token,
            percent_encode_query_value(&self.name)
        )
    }
}

/// What goes between an endpoint and the pairing query, so the url a client
/// dials carries exactly the path the endpoint declares.
///
/// A server is named by two surfaces — this url and the `advertised_endpoint`
/// in its pong — and a client keys it by what it parses out of them, path
/// included. They have to agree.
///
/// An endpoint that is only scheme and authority declares no path, and a
/// client fills the path in as `/` whichever way this is written; the `/` is
/// written out, which is the form that has shipped and keeps every paired
/// server keyed as it already is. An endpoint that declares a path is already
/// complete: another `/` would dial `/herdr-ws/` while the pong advertises
/// `/herdr-ws`, and to a client those are two different servers.
fn query_prefix(endpoint: &str) -> &'static str {
    let declares_a_path = endpoint
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'));
    if declares_a_path {
        "?"
    } else {
        "/?"
    }
}

/// Percent-encode a query parameter value: every byte outside the
/// URL-unreserved set (RFC 3986: ALPHA, DIGIT, `-._~`) is escaped, UTF-8
/// bytewise. Stricter than strictly necessary for a query, so the value
/// survives any spec-conforming decoder unchanged.
fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Why `herdr pair` refuses to print a payload.
#[derive(Debug, PartialEq, Eq)]
enum PairingUnavailable {
    /// `[websocket_api].bind` is unset: the listener is off by default and
    /// there is no endpoint to encode.
    NotConfigured,
    /// `bind` is set but is not a socket address.
    InvalidBind { bind: String, error: String },
    /// `bind` parses, but no client could reach the address as printed.
    UnusableBind { bind: String, reason: String },
    /// `advertised_endpoint` is set but is not a url a client could dial.
    InvalidAdvertisedEndpoint { advertised: String, reason: String },
}

impl PairingUnavailable {
    fn explanation(&self, config_path: &Path) -> String {
        match self {
            // Pairing comes before the server (re)start: with `bind` set and
            // no token stored yet, the server refuses to start, and `herdr
            // pair` is what stores the token.
            Self::NotConfigured => format!(
                "error: the websocket api listener is not configured, so there is no endpoint to pair against.\n\
                 \n\
                 The listener is off by default and never enabled implicitly. To enable it:\n\
                 \n\
                 1. add the listener address to {}:\n\
                 \n\
                 [websocket_api]\n\
                 bind = \"<address>:4433\"   # exact TCP address to serve, e.g. your tailnet IP\n\
                 \n\
                 2. run `herdr pair` again — it mints and stores the bearer token —\n\
                 3. then start (or restart) the herdr server to bind the listener.\n",
                config_path.display()
            ),
            Self::InvalidBind { bind, error } => format!(
                "error: websocket_api.bind {bind:?} in {} is not a usable listener address: {error}.\n\
                 Use an exact <ip>:<port> address, e.g. your tailnet IP, then run `herdr pair` again.\n",
                config_path.display()
            ),
            Self::UnusableBind { bind, reason } => format!(
                "error: websocket_api.bind {bind:?} in {} cannot appear in a pairing payload: {reason}.\n\
                 Update the bind address and run `herdr pair` again; it will say whether a server restart is still needed.\n",
                config_path.display()
            ),
            Self::InvalidAdvertisedEndpoint { advertised, reason } => format!(
                "error: websocket_api.advertised_endpoint {advertised:?} in {} is not a url a client could dial: {reason}.\n\
                 Declare the url something else serves this listener at, e.g. \"wss://a-host.example.net/herdr-ws\" — scheme, host, optional port, and optional path only.\n\
                 Remove it to pair against the bind address instead.\n",
                config_path.display()
            ),
        }
    }
}

/// The declared endpoint in canonical form, or `None` when none is declared.
///
/// The bind address describes the local socket, which is the reachable url
/// only when nothing fronts the listener. An operator who puts a proxy in
/// front declares `websocket_api.advertised_endpoint`, and that wins; unset,
/// empty, or whitespace keeps the bind-derived `ws://<bind>` form.
///
/// The declaration is read and judged by [`crate::api`], the same code the
/// `ping` pong publishes it through, so a payload and a pong never disagree
/// about what this server tells clients to dial.
fn advertised_endpoint(advertised: Option<&str>) -> Result<Option<String>, PairingUnavailable> {
    let Some(advertised) = crate::api::declared_advertised_endpoint(advertised) else {
        return Ok(None);
    };

    crate::api::normalize_advertised_endpoint(advertised)
        .map(Some)
        .map_err(|reason| PairingUnavailable::InvalidAdvertisedEndpoint {
            advertised: advertised.to_string(),
            reason,
        })
}

/// Resolve the configured bind address into the listener this command works
/// against. Without an advertised endpoint it is also what clients dial, so
/// addresses that cannot be dialed as printed are rejected — unless an
/// advertised endpoint carries the payload instead, in which case the bind
/// is only the local socket to install the token on and no longer has to be
/// dialable from elsewhere. A wildcard bind is the ordinary shape for a
/// listener behind a proxy, which is exactly what advertising exists for.
fn pairing_endpoint(
    bind: Option<&str>,
    endpoint_is_advertised: bool,
) -> Result<SocketAddr, PairingUnavailable> {
    let bind = match bind {
        None | Some("") => return Err(PairingUnavailable::NotConfigured),
        Some(bind) => bind,
    };

    let addr: SocketAddr =
        bind.parse().map_err(
            |err: std::net::AddrParseError| PairingUnavailable::InvalidBind {
                bind: bind.to_string(),
                error: err.to_string(),
            },
        )?;

    // Port 0 is checked first because it is fatal either way: the OS picks
    // the port at bind time, so neither this command nor whatever fronts the
    // listener knows where to reach it, and advertising cannot make up for it.
    if addr.port() == 0 {
        return Err(PairingUnavailable::UnusableBind {
            bind: bind.to_string(),
            reason: "port 0 makes the OS pick a random port, so nothing knows which port the listener ends up on".to_string(),
        });
    }
    if addr.ip().is_unspecified() && !endpoint_is_advertised {
        return Err(PairingUnavailable::UnusableBind {
            bind: bind.to_string(),
            reason: "clients cannot dial the wildcard address; bind a concrete address such as the machine's tailnet IP, or declare websocket_api.advertised_endpoint".to_string(),
        });
    }

    Ok(addr)
}

/// Where this command dials the listener to install the freshly minted
/// token. A wildcard bind accepts on every interface but is not an address
/// to connect to, and locally it always answers on loopback at the same
/// port; skipping the reload instead would leave a running server on the old
/// token while the QR advertises the new one.
fn local_listener_address(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_unspecified() {
        return addr;
    }
    let loopback = match addr.ip() {
        std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(loopback, addr.port())
}

fn mint_token() -> std::io::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| {
        std::io::Error::other(format!(
            "could not gather randomness for the pairing token: {err}"
        ))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Replace (or add) the stored token, preserving the rest of the config file
/// including comments. Writing the new value is what invalidates the previous
/// token: config.toml is the single source the server reads.
fn rotate_token_in_config(content: &str, token: &str) -> String {
    crate::config::upsert_section_value(content, "websocket_api", "token", &format!("{token:?}"))
}

fn qr_code_text(url: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(url.as_bytes())
        .map_err(|err| format!("could not encode the pairing url as a qr code: {err}"))?;
    // Inverted colors (light modules for dark terminals), the crate's
    // documented terminal form; phone cameras decode inverted codes.
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

/// The control sockets this command will try to reload, in order.
///
/// Ordering only. Nothing identifies which server reads which config file: a
/// config path and a socket path are set independently, so neither this list
/// nor the reload it drives can establish that the server reached is the one
/// serving the payload. That is what the token handshake is for, and the
/// reload is reported as the attempt it is.
#[derive(Debug, PartialEq, Eq)]
struct ControlSockets {
    /// The socket of a server whose home directory is the config file's own.
    /// Tried first when the config was selected explicitly, because on a
    /// machine running one server per config it is the likelier owner — a
    /// better guess than the ambient socket, and still only a guess.
    beside_config: Option<PathBuf>,
    /// The socket a server started in this command's environment serves.
    ambient: PathBuf,
}

impl ControlSockets {
    fn in_order(&self) -> Vec<&PathBuf> {
        self.beside_config
            .iter()
            .chain(std::iter::once(&self.ambient))
            .collect()
    }
}

/// Order the control sockets to try for the config at `config_path`.
///
/// `ambient` is the socket a server started in this command's own environment
/// serves, and it is the likely owner whenever the config being paired is
/// that environment's own config — the ordinary one-config machine, and a
/// named session, which shares that config file. When the config file was
/// selected explicitly and is some other file, the ambient socket belongs to
/// a server that need never have read it: dialing it alone is how pairing
/// came to write a token into one config and reload a server holding another.
/// The socket beside that config file goes first there.
///
/// Neither one is an identification, and nothing downstream treats it as one.
fn control_sockets_to_try(
    config_path: &Path,
    environment_config_path: &Path,
    ambient: PathBuf,
) -> ControlSockets {
    let beside_config = if config_path == environment_config_path {
        None
    } else {
        config_path
            .parent()
            .map(crate::session::api_socket_path_in)
            .filter(|beside| *beside != ambient)
    };
    ControlSockets {
        beside_config,
        ambient,
    }
}

/// What came back from asking a server to reload the config. Context for the
/// report, never the report's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigReload {
    /// A server answered on this control socket and reloaded its config.
    /// Which config that server holds is not knowable from here.
    Reloaded { socket: PathBuf },
    /// No candidate produced a reload, and this is what each one did.
    NoAnswer { tried: Vec<ControlSocketAttempt> },
    /// The request to this socket did not come back with an answer that could
    /// be read. Deliberately named for the request rather than the server: a
    /// truncated or malformed reply, and an i/o error mid-exchange, all land
    /// here, and a server that applied the config perfectly can produce any
    /// of them. Nothing about server state may be inferred from it.
    RequestFailed { socket: PathBuf, reason: String },
}

/// What one control socket did when asked to reload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlSocketAttempt {
    socket: PathBuf,
    outcome: ControlSocketOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlSocketOutcome {
    /// Nothing is listening on the socket.
    NotRunning,
    /// The attempt ran out of budget. Whether the connection was ever
    /// completed is not knowable here — the bound spans connecting and
    /// answering — so this says only that the request did not finish.
    Unfinished,
}

impl ControlSocketAttempt {
    fn describe(&self) -> String {
        match self.outcome {
            ControlSocketOutcome::NotRunning => format!("{} (not running)", self.socket.display()),
            ControlSocketOutcome::Unfinished => format!(
                "{} (the reload request did not finish within {}s)",
                self.socket.display(),
                CONTROL_SOCKET_TIMEOUT.as_secs()
            ),
        }
    }
}

/// Ask the first server that answers to reload its config. A socket nothing
/// is listening on is not a failure — it only means that server is not
/// running — and neither is one that accepts and then says nothing, which is
/// what a stale process looks like. Both move on to the next candidate,
/// carrying what they did into the report.
fn reload_config_of_owning_server(sockets: &ControlSockets) -> ConfigReload {
    let mut tried = Vec::new();
    for socket in sockets.in_order() {
        match reload_config_at(socket, CONTROL_SOCKET_TIMEOUT) {
            ConfigReload::NoAnswer { tried: attempts } => tried.extend(attempts),
            answered => return answered,
        }
    }
    ConfigReload::NoAnswer { tried }
}

/// Ask one server to reload, bounded by an absolute deadline.
///
/// The request runs on a thread this can stop waiting on rather than on a
/// socket timeout, because a socket timeout measures silence between bytes: a
/// stale peer that accepts and then trickles would reset it forever, and one
/// that accepts and says nothing at all would hold pairing open before the
/// bounded proof ever ran. The abandoned thread ends with the process.
fn reload_config_at(socket: &Path, timeout: Duration) -> ConfigReload {
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(socket.to_path_buf()));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(client.request_value(&Request {
            id: "cli:pair:reload-config".into(),
            method: Method::ServerReloadConfig(EmptyParams::default()),
        }));
    });

    let unanswered = |outcome| ConfigReload::NoAnswer {
        tried: vec![ControlSocketAttempt {
            socket: socket.to_path_buf(),
            outcome,
        }],
    };

    match rx.recv_timeout(timeout) {
        Ok(Ok(_)) => ConfigReload::Reloaded {
            socket: socket.to_path_buf(),
        },
        Ok(Err(ApiClientError::ErrorResponse(response))) => ConfigReload::RequestFailed {
            socket: socket.to_path_buf(),
            reason: response.error.message,
        },
        Ok(Err(ApiClientError::Io(err)))
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            unanswered(ControlSocketOutcome::NotRunning)
        }
        Ok(Err(err)) => ConfigReload::RequestFailed {
            socket: socket.to_path_buf(),
            reason: err.to_string(),
        },
        Err(_) => unanswered(ControlSocketOutcome::Unfinished),
    }
}

/// What a url this build can dial resolves to: what to connect to, and what
/// to ask for once connected.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DialTarget {
    /// The host as a client reads it out of the url, without ipv6 brackets.
    host: String,
    port: u16,
    /// The url the handshake requests, before the pairing query is appended.
    url: String,
}

/// What the token proof was allowed to exercise, in the operator's terms.
///
/// The url printed in the payload is the artifact handed over, so it is what
/// a proof should exercise. One shape cannot be: a `wss://` endpoint needs a
/// TLS client, and this build has none — `tungstenite` is vendored without
/// it, and adding a TLS stack to make a CLI message stronger is a permanent
/// cost. So the proof falls back to the listener behind the proxy and this
/// records that the printed url itself went unexercised, which every message
/// then says out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProofScope {
    /// The url that was actually dialed.
    dialed: String,
    /// Whether that url is this machine's own listener socket, which decides
    /// whether restarting the herdr server is the remedy to name.
    dialed_is_local_listener: bool,
    /// The printed url, when it is not the one that was dialed.
    unexercised_printed_url: Option<String>,
}

/// Where to present the token, and what that proves about the printed url.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenCheck {
    dial: DialTarget,
    scope: ProofScope,
}

/// Decide what the token proof dials.
///
/// A payload naming a `ws://` url — bind-derived or advertised — is dialed as
/// written, host, port and path included, because that url is the artifact
/// the operator was handed. A `wss://` payload cannot be dialed by this
/// build, so the proof settles for the listener behind it and the scope says
/// so; the alternative, quietly proving the backend and calling the payload
/// live, is the very substitution this command exists to stop making.
fn plan_token_check(
    endpoint: &str,
    endpoint_is_advertised: bool,
    local_listener: SocketAddr,
) -> TokenCheck {
    let backend = DialTarget {
        host: local_listener.ip().to_string(),
        port: local_listener.port(),
        url: format!("ws://{local_listener}"),
    };
    let backend_check = |unexercised: Option<String>| TokenCheck {
        scope: ProofScope {
            dialed: backend.url.clone(),
            dialed_is_local_listener: true,
            unexercised_printed_url: unexercised,
        },
        dial: backend.clone(),
    };

    if !endpoint_is_advertised {
        return backend_check(None);
    }

    match dial_target_for(endpoint) {
        Some(dial) => TokenCheck {
            scope: ProofScope {
                dialed: dial.url.clone(),
                dialed_is_local_listener: false,
                unexercised_printed_url: None,
            },
            dial,
        },
        None => backend_check(Some(endpoint.to_string())),
    }
}

/// The dial target for a url this build can open, or `None` when it cannot —
/// today only `wss://`, which would need a TLS client.
///
/// The url is split by the client library that will dial it rather than by
/// hand, so what this connects to is what a client parses out of the same
/// text.
fn dial_target_for(endpoint: &str) -> Option<DialTarget> {
    let request = endpoint.into_client_request().ok()?;
    let uri = request.uri();
    if uri.scheme_str().is_none_or(|scheme| scheme != "ws") {
        return None;
    }
    // A url carries an ipv6 literal bracketed; a connect call wants it bare.
    let host = uri
        .host()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    Some(DialTarget {
        host,
        // ws:// without a port is port 80, the same default a client uses.
        port: uri.port_u16().unwrap_or(80),
        url: endpoint.to_string(),
    })
}

/// What presenting a token to a peer established. Every variant is an
/// observation, and `Accepted` is the narrowest of them: it means a peer that
/// answered as herdr completed the handshake for this token.
#[derive(Debug, PartialEq, Eq)]
enum TokenProof {
    /// A herdr server completed the handshake and answered a request.
    Accepted,
    /// The handshake was refused with this status, by a peer that never
    /// identified itself. It says the connection did not get in; it does not
    /// say a herdr listener judged this token.
    RefusedWithoutIdentifying { status: u16 },
    /// Nothing accepted a connection.
    NoListener,
    /// The handshake completed, but the peer never answered as herdr, so
    /// nothing about the token was established and no credential of ours
    /// belongs there.
    NotHerdr(String),
    /// Neither accepted nor refused: the reason names what stopped it.
    Inconclusive(String),
}

/// Resolve a host to the addresses a client would try, bounded in time. An
/// ip literal answers here without asking anything; a name goes to the
/// resolver on a thread this can stop waiting on, because a stalled resolver
/// would otherwise hang the command with no outcome at all.
fn resolve_bounded(host: &str, port: u16) -> Result<Vec<SocketAddr>, TokenProof> {
    use std::net::ToSocketAddrs;

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let query = (host.to_string(), port);
    std::thread::spawn(move || {
        let _ = tx.send(
            query
                .to_socket_addrs()
                .map(|addrs| addrs.collect::<Vec<_>>()),
        );
    });
    match rx.recv_timeout(RESOLVE_TIMEOUT) {
        Ok(Ok(addrs)) if addrs.is_empty() => Err(TokenProof::Inconclusive(format!(
            "{host} resolved to no addresses"
        ))),
        Ok(Ok(addrs)) => Ok(addrs),
        Ok(Err(err)) => Err(TokenProof::Inconclusive(format!(
            "{host} could not be resolved: {err}"
        ))),
        Err(_) => Err(TokenProof::Inconclusive(format!(
            "{host} did not resolve within {}s",
            RESOLVE_TIMEOUT.as_secs()
        ))),
    }
}

/// Open a connection to the target, or say what failing to proves.
///
/// A refused connection is the one shape that means "nothing is serving this
/// address". A connect that times out is not: a listener behind a stalled
/// hop answers neither way, and calling that "no listener" would send an
/// operator to restart a server that may be running perfectly.
fn connect_to_target(dial: &DialTarget) -> Result<std::net::TcpStream, TokenProof> {
    let addrs = resolve_bounded(&dial.host, dial.port)?;
    let mut refused = false;
    let mut last_error = None;
    for addr in addrs {
        match std::net::TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => refused = true,
            Err(err) => last_error = Some((addr, err)),
        }
    }
    match last_error {
        Some((addr, err)) if err.kind() == std::io::ErrorKind::TimedOut => {
            Err(TokenProof::Inconclusive(format!(
                "the connection to {addr} did not complete within {}s",
                CONNECT_TIMEOUT.as_secs()
            )))
        }
        Some((addr, err)) => Err(TokenProof::Inconclusive(format!(
            "could not connect to {addr}: {err}"
        ))),
        None if refused => Err(TokenProof::NoListener),
        None => Err(TokenProof::Inconclusive(format!(
            "{} resolved to no address this machine could dial",
            dial.host
        ))),
    }
}

/// Present `token` at `dial` the way the printed payload does — appended as
/// the `token` query parameter to the url itself — and report what the
/// exchange proved.
///
/// Two things have to hold before this returns `Accepted`. The handshake has
/// to complete, and the peer has to answer as herdr: any compliant websocket
/// service answers 101, so an upgrade alone identifies a service, not this
/// program. A `ping` is the cheapest herdr-shaped question, and a response
/// that deserializes as this protocol's pong under the id we sent is an
/// answer only a herdr server gives.
fn probe_token(dial: &DialTarget, token: &str) -> TokenProof {
    probe_token_within(dial, token, EXCHANGE_TIMEOUT)
}

/// The same, with the exchange budget spelled out so a test can watch the
/// deadline fire instead of taking the production one on faith.
fn probe_token_within(dial: &DialTarget, token: &str, budget: Duration) -> TokenProof {
    let stream = match connect_to_target(dial) {
        Ok(stream) => stream,
        Err(proof) => return proof,
    };
    // One deadline for the handshake and the identifying exchange together.
    // A per-read timeout is a silence timer: a peer trickling a byte just
    // under it keeps every read successful and the command open forever, and
    // a frame budget that only counts whole frames never trips either.
    let deadline = std::time::Instant::now() + budget;
    if let Err(err) = stream.set_write_timeout(Some(budget)) {
        return TokenProof::Inconclusive(format!("could not bound the handshake: {err}"));
    }
    let stream = DeadlineStream {
        inner: stream,
        deadline,
    };

    // The token charset is URL-unreserved by construction, so the query form
    // needs no escaping here either; the prefix is the payload's own, so a
    // url declaring a path is dialed with that path intact.
    let url = format!("{}{}token={token}", dial.url, query_prefix(&dial.url));
    let request = match url.into_client_request() {
        Ok(request) => request,
        Err(err) => {
            return TokenProof::Inconclusive(format!(
                "could not build a handshake for {}: {err}",
                dial.url
            ));
        }
    };

    let expired = |budget| {
        TokenProof::Inconclusive(format!(
            "the exchange did not finish within {}",
            format_budget(budget)
        ))
    };
    match tungstenite::client::client(request, stream) {
        Ok((websocket, _response)) => identify_herdr_peer(websocket, deadline, budget),
        // A refusal is a status and nothing else. Herdr answers an unknown
        // token with a deliberately opaque 401, which is exactly what an
        // unrelated backend or a proxy in front of one answers with, so a
        // status alone cannot say the herdr listener rejected this token —
        // only that whatever holds the url did not let this connection in.
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            TokenProof::RefusedWithoutIdentifying {
                status: response.status().as_u16(),
            }
        }
        Err(_) if std::time::Instant::now() >= deadline => expired(budget),
        Err(err) => TokenProof::Inconclusive(format!("the handshake did not complete: {err}")),
    }
}

/// A stream whose reads are bounded by one absolute deadline.
///
/// A socket read timeout is a silence timer applied to a single syscall, and
/// both the handshake and one `read()` of a frame issue several: a peer
/// emitting a byte just under the interval keeps every syscall successful
/// and the command blocked far past its budget. Recomputing the timeout from
/// what is left of the deadline before each read is what makes the bound
/// absolute — the budget is spent by the clock, not reset by traffic.
struct DeadlineStream {
    inner: std::net::TcpStream,
    deadline: std::time::Instant,
}

impl std::io::Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the exchange ran out of time",
            ));
        }
        // A sub-millisecond timeout rounds to zero on some platforms, which
        // means "block forever" — the one value this must never set.
        self.inner
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
        self.inner.read(buf)
    }
}

impl std::io::Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn format_budget(budget: Duration) -> String {
    format!("{}ms", budget.as_millis())
}

/// Ask the connected peer to identify itself as herdr, then close. Bounded by
/// the same deadline the handshake ran under: what is being bounded is the
/// whole exchange, and every read inside it.
fn identify_herdr_peer(
    mut websocket: tungstenite::WebSocket<DeadlineStream>,
    deadline: std::time::Instant,
    budget: Duration,
) -> TokenProof {
    const PROBE_ID: &str = "cli:pair:verify-token";

    let request = match serde_json::to_string(&Request {
        id: PROBE_ID.into(),
        method: Method::Ping(PingParams::default()),
    }) {
        Ok(request) => request,
        Err(err) => return TokenProof::Inconclusive(format!("could not encode a ping: {err}")),
    };
    if let Err(err) = websocket.send(tungstenite::Message::Text(request.into())) {
        return TokenProof::Inconclusive(format!("the connection closed before a ping: {err}"));
    }

    // Control frames and any traffic that is not this answer are skipped
    // rather than read as one; the frame budget bounds a peer that answers
    // quickly and endlessly, the deadline one that answers slowly — including
    // one that never finishes a single frame, which the stream's own reads
    // cut off.
    let mut proof = TokenProof::NotHerdr("it answered nothing".to_string());
    for _ in 0..PING_FRAME_BUDGET {
        if std::time::Instant::now() >= deadline {
            proof = TokenProof::Inconclusive(format!(
                "it did not answer within {}",
                format_budget(budget)
            ));
            break;
        }
        match websocket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                proof = match serde_json::from_str::<SuccessResponse>(&text) {
                    Ok(response)
                        if response.id == PROBE_ID
                            && matches!(response.result, ResponseResult::Pong { .. }) =>
                    {
                        TokenProof::Accepted
                    }
                    _ => TokenProof::NotHerdr(
                        "its answer was not a herdr pong for the ping that was sent".to_string(),
                    ),
                };
                break;
            }
            Ok(_) => continue,
            Err(err) => {
                // A read that ended because the budget ran out is the
                // deadline speaking, not the peer.
                proof = if std::time::Instant::now() >= deadline {
                    TokenProof::Inconclusive(format!(
                        "it did not answer within {}",
                        format_budget(budget)
                    ))
                } else {
                    TokenProof::NotHerdr(format!("it answered no ping: {err}"))
                };
                break;
            }
        }
    }

    let _ = websocket.close(None);
    let _ = websocket.flush();
    proof
}

/// Present the new token, and the previous one only to a peer that has
/// identified itself as herdr.
///
/// The order is a safety rule, not an optimization: the previous token is a
/// live credential until the rotation is known to have taken, and handing it
/// to a peer that has not been identified would give it away to whatever
/// happens to hold the port.
fn prove_token(
    dial: &DialTarget,
    token: &str,
    previous_token: Option<&str>,
) -> (TokenProof, Option<TokenProof>) {
    let new_token = probe_token(dial, token);
    let previous_token = match (&new_token, previous_token) {
        (TokenProof::Accepted, Some(previous)) => Some(probe_token(dial, previous)),
        _ => None,
    };
    (new_token, previous_token)
}

/// What presenting the previous token proved after the rotation.
#[derive(Debug, PartialEq, Eq)]
enum PreviousTokenProof {
    /// There was no previous token, or presenting it proved nothing.
    Unproven,
    /// Presenting the previous token was refused: the rotation took effect.
    Refused,
    /// The previous token was still accepted after the rotation.
    StillAccepted,
}

/// What the command established about the freshly stored token.
///
/// The handshake decides which variant this is; the reload rides along inside
/// it, because the two are separate observations and a report that keeps only
/// one of them says something false about the other. Every variant except the
/// live one therefore carries what the reload attempt did, and every message
/// states both.
#[derive(Debug, PartialEq, Eq)]
enum TokenActivation {
    /// A peer that answered as herdr accepted a handshake presenting the new
    /// token. `previous_token` carries what presenting the previous one to
    /// that same peer proved, and `reload` is only mentioned when it failed —
    /// the claim rests on the handshake, but a broken control socket is still
    /// a fact the operator owns.
    LiveNow {
        previous_token: PreviousTokenProof,
        reload: ConfigReload,
    },
    /// Nothing is listening where the payload points.
    NothingListening { reload: ConfigReload },
    /// The handshake was refused, by a peer that never identified itself.
    RefusedWithoutIdentifying { status: u16, reload: ConfigReload },
    /// A websocket service answered, but never as herdr.
    NotHerdr {
        reason: String,
        reload: ConfigReload,
    },
    /// Neither accepted nor refused.
    Unverified {
        reason: String,
        reload: ConfigReload,
    },
}

fn apply_token_to_running_server(
    config_path: &Path,
    check: &TokenCheck,
    token: &str,
    previous_token: Option<&str>,
) -> TokenActivation {
    let sockets = control_sockets_to_try(
        config_path,
        &crate::config::config_dir().join("config.toml"),
        crate::api::socket_path(),
    );
    let reload = reload_config_of_owning_server(&sockets);
    let (new_token, previous_token) = prove_token(&check.dial, token, previous_token);

    decide_activation(reload, new_token, previous_token)
}

/// Turn the reload attempt and the handshakes into the one state they
/// support.
///
/// The handshake decides, in both directions. A peer that accepted the token
/// makes the payload live whichever process this command managed to reach —
/// that is the point of proving it against the url — and a reload that
/// succeeded proves nothing on its own. Neither observation replaces the
/// other: the reload travels with the outcome so the report can state both.
fn decide_activation(
    reload: ConfigReload,
    new_token: TokenProof,
    previous_token: Option<TokenProof>,
) -> TokenActivation {
    match new_token {
        TokenProof::Accepted => TokenActivation::LiveNow {
            previous_token: match previous_token {
                Some(TokenProof::RefusedWithoutIdentifying { .. }) => PreviousTokenProof::Refused,
                Some(TokenProof::Accepted) => PreviousTokenProof::StillAccepted,
                _ => PreviousTokenProof::Unproven,
            },
            reload,
        },
        TokenProof::NoListener => TokenActivation::NothingListening { reload },
        TokenProof::RefusedWithoutIdentifying { status } => {
            TokenActivation::RefusedWithoutIdentifying { status, reload }
        }
        TokenProof::NotHerdr(reason) => TokenActivation::NotHerdr { reason, reload },
        TokenProof::Inconclusive(reason) => TokenActivation::Unverified { reason, reload },
    }
}

/// Human-readable outcome plus the process exit code.
///
/// Every sentence here is bounded by what was observed, and by where it was
/// observed: each one names the url that was dialed, and when that url is not
/// the one printed in the payload, it names what went unexercised. An
/// operator told plainly that the proxy path was not checked can go and check
/// it; one told "live" cannot.
///
/// Exit is non-zero when the payload was observed not to work, and when it
/// could not be shown to work. A state that is understood and has a next step
/// — no server yet, no listener yet — exits zero; an unknown one does not,
/// because a payload nobody has connected with is exactly what this command
/// used to report as live.
fn activation_report(
    activation: &TokenActivation,
    previous_token_existed: bool,
    scope: &ProofScope,
    config_path: &Path,
) -> (String, i32) {
    let rotation = if previous_token_existed {
        "Rotated: the previous token was replaced in config.toml"
    } else {
        "Stored the first token in config.toml"
    };
    let dialed = &scope.dialed;
    let accepted = format!(
        "A handshake on {dialed} presenting the new token was accepted by a peer that answered as herdr"
    );
    // What the proof could not reach. Stated in every outcome it applies to,
    // because "the printed url was never dialed" is a fact about the payload
    // in the operator's hand whatever else happened — and phrased as the
    // attempt it is, so it cannot contradict an outcome where the fallback
    // reached nothing either.
    let unexercised = match &scope.unexercised_printed_url {
        None => String::new(),
        Some(url) => format!(
            "\nThe printed url {url} was not dialed: this build has no TLS client, so the proof \
             was attempted against the listener behind it instead. Nothing here says whether \
             {url} carries a connection to that listener; connect to it with the printed token \
             to find out."
        ),
    };
    let start_remedy = if scope.dialed_is_local_listener {
        "Restart the herdr server so it binds the websocket listener, then scan.".to_string()
    } else {
        format!(
            "Start whatever serves {dialed} — the proxy in front, or the herdr listener behind it \
             — then scan."
        )
    };

    match activation {
        TokenActivation::LiveNow {
            previous_token,
            reload,
        } => {
            // The live claim rests on the handshake alone, so the reload is
            // mentioned only where it is news: a control socket that errored
            // is a separate fault the operator still owns.
            let reload_fault = match reload {
                ConfigReload::RequestFailed { socket, reason } => format!(
                    "\nnote: the reload request to {} did not come back readable: {reason}. That \
                     describes the request, not what that server did with its config.",
                    socket.display()
                ),
                _ => String::new(),
            };
            match previous_token {
                PreviousTokenProof::Refused => (
                    format!(
                        "{rotation}. {accepted}, and one presenting the previous token was refused.{reload_fault}{unexercised}"
                    ),
                    0,
                ),
                PreviousTokenProof::StillAccepted => (
                    format!(
                        "{rotation}. {accepted}, but one presenting the previous token was accepted too.\n\
                         warning: the previous token has not stopped authenticating. Restart the herdr \
                         server serving {dialed} before treating it as revoked.{reload_fault}{unexercised}"
                    ),
                    1,
                ),
                PreviousTokenProof::Unproven if previous_token_existed => (
                    format!(
                        "{rotation}. {accepted}.\n\
                         Whether the previous token still authenticates was not established.{reload_fault}{unexercised}"
                    ),
                    0,
                ),
                PreviousTokenProof::Unproven => (
                    format!("{rotation}. {accepted}.{reload_fault}{unexercised}"),
                    0,
                ),
            }
        }
        TokenActivation::NothingListening { reload } => {
            let remedy = match reload {
                ConfigReload::NoAnswer { .. } if scope.dialed_is_local_listener => {
                    "The token takes effect when the herdr server starts.".to_string()
                }
                _ => start_remedy,
            };
            (
                format!(
                    "{rotation}. {}, and nothing is listening on {dialed}.\n\
                     {remedy}{unexercised}",
                    reload_clause(reload)
                ),
                0,
            )
        }
        TokenActivation::RefusedWithoutIdentifying { status, reload } => (
            format!(
                "{rotation}. {}, and a handshake on {dialed} presenting the new token was refused \
                 with http {status}.\n\
                 warning: the printed token did not get in, and the refusal names no one: herdr \
                 answers an unknown token with the same opaque refusal any other service would, and \
                 whatever answered may be something in front of the listener rather than a herdr \
                 server at all. Which process refused, and which config it holds, are not \
                 established. Find out what serves {dialed} — `herdr session list --json` names \
                 each running server and its socket path — and if that turns out to be a herdr \
                 server, `HERDR_SOCKET_PATH=<its socket_path> herdr server reload-config` or a \
                 restart is what makes it read {}.{unexercised}",
                reload_clause(reload),
                config_path.display()
            ),
            1,
        ),
        TokenActivation::NotHerdr { reason, reload } => (
            format!(
                "{rotation}. {}, and something on {dialed} completed a websocket handshake but did \
                 not answer as herdr: {reason}.\n\
                 warning: nothing about the printed token was established, and the previous token was \
                 not presented to that peer. Check what is serving {dialed} before scanning.{unexercised}",
                reload_clause(reload)
            ),
            1,
        ),
        TokenActivation::Unverified { reason, reload } => (
            format!(
                "{rotation}. {}, and whether {dialed} accepts the new token could not be established: \
                 {reason}.\n\
                 warning: nothing has connected with the printed token. Restart the herdr server \
                 serving {dialed}, or connect with the token yourself, before relying on it.{unexercised}",
                reload_clause(reload)
            ),
            1,
        ),
    }
}

/// The reload attempt as one clause, so every outcome states it alongside
/// what the handshake found instead of one standing in for the other.
fn reload_clause(reload: &ConfigReload) -> String {
    match reload {
        ConfigReload::Reloaded { socket } => {
            format!("A herdr server at {} reloaded its config", socket.display())
        }
        ConfigReload::NoAnswer { tried } => format!(
            "No running herdr server answered a control socket ({})",
            tried
                .iter()
                .map(ControlSocketAttempt::describe)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ConfigReload::RequestFailed { socket, reason } => format!(
            "A reload request to {} did not come back readable: {reason}",
            socket.display()
        ),
    }
}

fn print_pair_help() {
    eprintln!("usage: herdr pair");
    eprintln!();
    eprintln!("Mint the websocket api bearer token and print the pairing payload");
    eprintln!("(endpoint + token) as a QR code and as plaintext. Requires");
    eprintln!("[websocket_api].bind to be configured. Re-running rotates the token");
    eprintln!("and invalidates the previous one.");
    eprintln!();
    eprintln!("When something else fronts the listener (a TLS terminating proxy,");
    eprintln!("for instance), set [websocket_api].advertised_endpoint to the url");
    eprintln!("clients should dial, e.g. \"wss://a-host.example.net\", or with a");
    eprintln!("path when one proxy fronts several servers, e.g.");
    eprintln!("\"wss://a-host.example.net/herdr-ws\"; the payload names that instead");
    eprintln!("of the bind address. Unset pairs against bind.");
    eprintln!();
    eprintln!("The scannable URL also carries the server's display name so clients");
    eprintln!("can label the server before first connect: [websocket_api].name,");
    eprintln!("or the machine's hostname when unset. Display only, never identity.");
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    #[test]
    fn minted_tokens_are_long_urlsafe_and_unique() {
        let first = mint_token().unwrap();
        let second = mint_token().unwrap();

        assert_eq!(first.len(), 43, "{first}");
        assert!(crate::api::valid_websocket_token_chars(&first), "{first}");
        assert_ne!(first, second, "two mints must not collide");
    }

    /// The canonical endpoint for a declared value, or the reason it is not
    /// one. Mirrors what `run_pair_command` does before minting anything.
    fn declared(advertised: &str) -> Result<String, String> {
        match advertised_endpoint(Some(advertised)) {
            Ok(Some(endpoint)) => Ok(endpoint),
            Ok(None) => panic!("{advertised:?} was read as undeclared"),
            Err(PairingUnavailable::InvalidAdvertisedEndpoint {
                advertised: value,
                reason,
            }) => {
                assert_eq!(value, advertised.trim(), "the refusal must quote the value");
                Err(reason)
            }
            Err(other) => {
                panic!("{advertised:?}: expected InvalidAdvertisedEndpoint, got {other:?}")
            }
        }
    }

    #[test]
    fn pairing_endpoint_requires_a_configured_bind() {
        assert_eq!(
            pairing_endpoint(None, false),
            Err(PairingUnavailable::NotConfigured)
        );
        assert_eq!(
            pairing_endpoint(Some(""), false),
            Err(PairingUnavailable::NotConfigured)
        );
        // An advertised endpoint does not stand in for a listener.
        assert_eq!(
            pairing_endpoint(None, true),
            Err(PairingUnavailable::NotConfigured)
        );
    }

    #[test]
    fn pairing_endpoint_rejects_unparseable_binds() {
        match pairing_endpoint(Some("not-an-address"), false) {
            Err(PairingUnavailable::InvalidBind { bind, .. }) => assert_eq!(bind, "not-an-address"),
            other => panic!("expected InvalidBind, got {other:?}"),
        }
    }

    #[test]
    fn pairing_endpoint_rejects_addresses_that_cannot_be_dialed_as_printed() {
        for bind in ["0.0.0.0:4433", "[::]:4433"] {
            match pairing_endpoint(Some(bind), false) {
                Err(PairingUnavailable::UnusableBind { reason, .. }) => {
                    assert!(reason.contains("wildcard"), "{bind}: {reason}");
                }
                other => panic!("{bind}: expected UnusableBind, got {other:?}"),
            }
        }

        match pairing_endpoint(Some("127.0.0.1:0"), false) {
            Err(PairingUnavailable::UnusableBind { reason, .. }) => {
                assert!(reason.contains("port 0"), "{reason}");
            }
            other => panic!("expected UnusableBind, got {other:?}"),
        }
    }

    #[test]
    fn a_wildcard_bind_is_usable_once_an_endpoint_is_advertised() {
        // The deployment this feature exists for: the listener accepts on
        // every interface and a proxy in front is what clients dial. Nothing
        // derives the payload from the bind any more, so it need not be
        // dialable from elsewhere.
        for bind in ["0.0.0.0:4433", "[::]:4433"] {
            assert_eq!(
                pairing_endpoint(Some(bind), true),
                Ok(addr(bind)),
                "{bind} must be accepted when an endpoint is advertised"
            );
        }

        // Port 0 stays fatal either way: the port is not known until the
        // server binds, so neither this command nor a proxy can find it.
        for advertised in [false, true] {
            match pairing_endpoint(Some("0.0.0.0:0"), advertised) {
                Err(PairingUnavailable::UnusableBind { reason, .. }) => {
                    assert!(reason.contains("port 0"), "{advertised}: {reason}");
                }
                other => panic!("{advertised}: expected UnusableBind, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_token_is_installed_on_loopback_when_the_listener_binds_the_wildcard() {
        // A wildcard bind is not an address to connect to, but the listener
        // does answer locally on loopback at the same port. Dialing that is
        // what keeps the running server's token in step with the printed QR.
        assert_eq!(
            local_listener_address(addr("0.0.0.0:4433")),
            addr("127.0.0.1:4433")
        );
        assert_eq!(
            local_listener_address(addr("[::]:4433")),
            addr("[::1]:4433")
        );

        // A concrete bind is already the address to dial, unchanged.
        for bind in ["100.64.0.5:4433", "127.0.0.1:4433", "[fd7a::1]:8443"] {
            assert_eq!(local_listener_address(addr(bind)), addr(bind), "{bind}");
        }
    }

    /// The claim the wildcard resolution rests on, against a real socket: a
    /// listener that accepted the wildcard address answers on loopback at the
    /// port it was given, so the resolved address is one this command can
    /// actually dial to install the token.
    #[test]
    fn a_wildcard_listener_answers_on_the_resolved_loopback_address() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("bind wildcard");
        let bound = listener.local_addr().expect("bound address");
        assert!(bound.ip().is_unspecified(), "{bound}");

        let dial_target = local_listener_address(bound);

        assert_eq!(dial_target.port(), bound.port());
        assert!(dial_target.ip().is_loopback(), "{dial_target}");
        let check = plan_token_check(&format!("ws://{dial_target}"), false, dial_target);
        assert!(
            connect_to_target(&check.dial).is_ok(),
            "a wildcard listener must answer on {dial_target}"
        );
    }

    #[test]
    fn pairing_endpoint_accepts_concrete_addresses() {
        assert_eq!(
            pairing_endpoint(Some("100.64.0.5:4433"), false),
            Ok(addr("100.64.0.5:4433"))
        );
        assert_eq!(
            pairing_endpoint(Some("[::1]:4433"), false),
            Ok(addr("[::1]:4433"))
        );
    }

    #[test]
    fn payload_url_carries_endpoint_token_and_name_in_connectable_form() {
        let payload = PairingPayload {
            endpoint: "ws://100.64.0.5:4433".to_string(),
            token: "abcDEF123-_".to_string(),
            name: "the-mini".to_string(),
        };
        assert_eq!(payload.endpoint, "ws://100.64.0.5:4433");
        assert_eq!(
            payload.url(),
            "ws://100.64.0.5:4433/?token=abcDEF123-_&name=the-mini"
        );

        let ipv6 = PairingPayload {
            endpoint: "ws://[::1]:4433".to_string(),
            token: "t".to_string(),
            name: "n".to_string(),
        };
        assert_eq!(ipv6.url(), "ws://[::1]:4433/?token=t&name=n");
    }

    #[test]
    fn payload_percent_encodes_the_name_and_keeps_the_endpoint_bare() {
        let payload = PairingPayload {
            endpoint: "ws://100.64.0.5:4433".to_string(),
            token: "tok".to_string(),
            name: "Can's Mini (büro)".to_string(),
        };

        // The name rides only the scannable URL, percent-encoded.
        assert_eq!(
            payload.url(),
            "ws://100.64.0.5:4433/?token=tok&name=Can%27s%20Mini%20%28b%C3%BCro%29"
        );
        // The plaintext endpoint carries neither name nor token.
        assert_eq!(payload.endpoint, "ws://100.64.0.5:4433");
    }

    #[test]
    fn without_an_advertised_endpoint_the_payload_url_is_bind_derived() {
        // Undeclared: nothing to resolve, so the bind carries the payload.
        assert_eq!(advertised_endpoint(None), Ok(None));

        // Empty and whitespace-only values mean undeclared, like `name`.
        for unset in ["", "   "] {
            assert_eq!(advertised_endpoint(Some(unset)), Ok(None), "{unset:?}");
        }

        let payload = PairingPayload {
            endpoint: format!("ws://{}", addr("100.64.0.5:4433")),
            token: "tok".to_string(),
            name: "the-mini".to_string(),
        };
        assert_eq!(payload.endpoint, "ws://100.64.0.5:4433");
        assert_eq!(
            payload.url(),
            "ws://100.64.0.5:4433/?token=tok&name=the-mini"
        );
    }

    #[test]
    fn an_advertised_endpoint_replaces_the_bind_derived_url() {
        // A proxy-fronted server names the url clients should dial instead.
        let endpoint = declared("wss://a-host.example.ts.net").unwrap();
        assert_eq!(endpoint, "wss://a-host.example.ts.net");

        let payload = PairingPayload {
            endpoint,
            token: "abcDEF123-_".to_string(),
            name: "the mini".to_string(),
        };
        assert_eq!(
            payload.url(),
            "wss://a-host.example.ts.net/?token=abcDEF123-_&name=the%20mini"
        );
    }

    /// What accepting a path rests on: the query goes on after the declared
    /// url, so the path is still there in front of the token, and a client
    /// that strips the token and name is left with exactly the host and path
    /// that were declared — the same ones the pong publishes.
    #[test]
    fn an_advertised_path_survives_the_appended_token() {
        let endpoint = declared("wss://a-host.example.ts.net/herdr-ws").unwrap();
        assert_eq!(endpoint, "wss://a-host.example.ts.net/herdr-ws");

        let payload = PairingPayload {
            endpoint,
            token: "abcDEF123-_".to_string(),
            name: "the mini".to_string(),
        };
        assert_eq!(
            payload.url(),
            "wss://a-host.example.ts.net/herdr-ws?token=abcDEF123-_&name=the%20mini"
        );

        // A declared trailing slash lands on the same url, because the
        // canonical form drops it and nothing puts it back.
        let with_slash = PairingPayload {
            endpoint: declared("wss://a-host.example.ts.net/herdr-ws/").unwrap(),
            token: "abcDEF123-_".to_string(),
            name: "the mini".to_string(),
        };
        assert_eq!(with_slash.url(), payload.url());
    }

    /// A path of exactly `/` is the same url as no path at all, so it has to
    /// reach a client as the same server: same canonical form, same pairing
    /// url, byte for byte.
    #[test]
    fn a_path_of_only_a_slash_is_the_same_url_as_no_path() {
        for (with_slash, without) in [
            (
                "wss://a-host.example.ts.net/",
                "wss://a-host.example.ts.net",
            ),
            (
                "wss://a-host.example.ts.net:8443/",
                "wss://a-host.example.ts.net:8443",
            ),
            ("wss://[fd7a::1]:8443/", "wss://[fd7a::1]:8443"),
        ] {
            assert_eq!(declared(with_slash), declared(without), "{with_slash:?}");

            let payload = |endpoint: String| PairingPayload {
                endpoint,
                token: "tok".to_string(),
                name: "the-mini".to_string(),
            };
            assert_eq!(
                payload(declared(with_slash).unwrap()).url(),
                payload(declared(without).unwrap()).url(),
                "{with_slash:?}"
            );
        }
    }

    /// The root case is what has already shipped and what every paired server
    /// is keyed by, so its pairing url is pinned byte for byte — an endpoint
    /// that declares no path still gets the `/` written out, whether it came
    /// from the bind address or from a declaration.
    #[test]
    fn an_endpoint_without_a_path_keeps_the_pairing_url_it_has_always_had() {
        let payload = |endpoint: &str| {
            PairingPayload {
                endpoint: endpoint.to_string(),
                token: "abcDEF123-_".to_string(),
                name: "the-mini".to_string(),
            }
            .url()
        };

        assert_eq!(
            payload("ws://100.64.0.5:4433"),
            "ws://100.64.0.5:4433/?token=abcDEF123-_&name=the-mini"
        );
        assert_eq!(
            payload("wss://a-host.example.ts.net:8443"),
            "wss://a-host.example.ts.net:8443/?token=abcDEF123-_&name=the-mini"
        );
        assert_eq!(
            payload("ws://[fd7a::1]:4433"),
            "ws://[fd7a::1]:4433/?token=abcDEF123-_&name=the-mini"
        );
    }

    /// Every value the endpoint validator accepts, with the canonical form it
    /// is accepted as. Shared by the test that pins those forms and the one
    /// that pins the pong and the pairing url against each other, so a value
    /// added here is judged by both rather than by whichever it was added to.
    fn accepted_endpoints() -> Vec<(&'static str, &'static str)> {
        vec![
            // A plain single-label host.
            ("wss://a-host", "wss://a-host"),
            ("wss://a-host.example.ts.net", "wss://a-host.example.ts.net"),
            ("ws://a-host.example.ts.net", "ws://a-host.example.ts.net"),
            // The shape this ships against: a magicdns fqdn with an explicit
            // port, hyphens and digits in the labels.
            (
                "wss://a-host-2.tailnet-name.ts.net:8443",
                "wss://a-host-2.tailnet-name.ts.net:8443",
            ),
            // Underscores appear in internal names; a client accepts them.
            (
                "wss://an_internal.host.local",
                "wss://an_internal.host.local",
            ),
            // A trailing dot is the dns root, and a client keeps it on a
            // name — so the payload keeps it too.
            ("wss://a-host.example.net.", "wss://a-host.example.net."),
            // A punycode name is already the ascii form a client resolves a
            // non-ascii name to, so it rides through untouched.
            (
                "wss://xn--nave-6pa.example.net",
                "wss://xn--nave-6pa.example.net",
            ),
            // The one ipv4 spelling this function and a client agree on,
            // with and without a port.
            ("wss://100.64.0.5", "wss://100.64.0.5"),
            ("wss://100.64.0.5:8443", "wss://100.64.0.5:8443"),
            ("wss://0.0.0.0", "wss://0.0.0.0"),
            ("wss://255.255.255.255:1", "wss://255.255.255.255:1"),
            // Bracketed ipv6 literals, with and without a port.
            ("wss://[fd7a::1]", "wss://[fd7a::1]"),
            ("wss://[fd7a::1]:8443", "wss://[fd7a::1]:8443"),
            // One trailing slash is the same url; the query form appends its own.
            (
                "wss://a-host.example.ts.net/",
                "wss://a-host.example.ts.net",
            ),
            // Surrounding whitespace is config noise, not part of the url.
            (
                "  wss://a-host.example.ts.net  ",
                "wss://a-host.example.ts.net",
            ),
            // Schemes are case-insensitive; the payload carries the canonical form.
            ("WSS://a-host.example.ts.net", "wss://a-host.example.ts.net"),
            // A path survives the appended token rather than being lost to
            // it: the query goes on after the declared url, so this is dialled
            // as wss://a-host.example.ts.net/herdr-ws?token=…, which a client
            // reads back as this host and this path — the pair of them the
            // pong publishes.
            (
                "wss://a-host.example.ts.net/herdr-ws",
                "wss://a-host.example.ts.net/herdr-ws",
            ),
            // The path form this ships against: one path per host, so every
            // node can answer on the same port behind one proxy.
            (
                "wss://a-host-2.tailnet-name.ts.net/herdr-ws",
                "wss://a-host-2.tailnet-name.ts.net/herdr-ws",
            ),
            // A trailing slash is the same url as none, and the canonical
            // form drops it, so both spellings pair as the same server.
            (
                "wss://a-host.example.ts.net/herdr-ws/",
                "wss://a-host.example.ts.net/herdr-ws",
            ),
            // A path rides alongside a port, an ipv4 literal, and a
            // bracketed ipv6 literal without changing any of them.
            (
                "wss://a-host.example.ts.net:8443/herdr-ws",
                "wss://a-host.example.ts.net:8443/herdr-ws",
            ),
            ("wss://100.64.0.5/herdr-ws", "wss://100.64.0.5/herdr-ws"),
            (
                "wss://[fd7a::1]:8443/herdr-ws",
                "wss://[fd7a::1]:8443/herdr-ws",
            ),
            // Several segments are one path, not several.
            (
                "wss://a-host.example.ts.net/api/v1/herdr-ws",
                "wss://a-host.example.ts.net/api/v1/herdr-ws",
            ),
            // Every character a client dials unchanged, including case: a
            // path is case-sensitive where a host is not.
            (
                "wss://a-host.example.ts.net/Herdr_ws-2.0~x",
                "wss://a-host.example.ts.net/Herdr_ws-2.0~x",
            ),
            // Dots inside a segment are ordinary text; only a segment that
            // is exactly "." or ".." is resolved away.
            (
                "wss://a-host.example.ts.net/...",
                "wss://a-host.example.ts.net/...",
            ),
            // Config noise around a url that carries a path is still noise.
            (
                "  WSS://a-host.example.ts.net/herdr-ws/  ",
                "wss://a-host.example.ts.net/herdr-ws",
            ),
        ]
    }

    /// Half of the invariant: an accepted value is carried into the payload
    /// as exactly the host, port, and path configured, because that is the
    /// only promise this function makes to whatever client parses it back out.
    #[test]
    fn an_advertised_endpoint_keeps_the_configured_scheme_host_port_and_path() {
        for (configured, expected) in accepted_endpoints() {
            assert_eq!(
                declared(configured),
                Ok(expected.to_string()),
                "{configured:?}"
            );
        }
    }

    /// What a client is left holding after it parses one of these urls: the
    /// scheme, the authority as written, and the path, with the pairing query
    /// dropped — which is the whole query, because the payload puts nothing
    /// else there. A split is a faithful stand-in for a url parser over the
    /// accepted set on purpose: that set is exactly the text this crate
    /// promises a client dials without rewriting any of it.
    fn client_server_key(url: &str) -> (String, String, String) {
        let (scheme, rest) = url.split_once("://").expect("a scheme");
        let rest = rest.split_once('?').map_or(rest, |(before, _)| before);
        match rest.split_once('/') {
            Some((authority, path)) => (
                scheme.to_string(),
                authority.to_string(),
                format!("/{path}"),
            ),
            // A url with no path is dialled at the root; a client fills that
            // in rather than leaving the path empty.
            None => (scheme.to_string(), rest.to_string(), "/".to_string()),
        }
    }

    /// The property the two surfaces owe a client together: the endpoint a
    /// server publishes in its pong and the endpoint its pairing url carries
    /// name the same server. A client keys a server by what it parses out of
    /// the url it dialled, so this compares parsed keys rather than strings —
    /// which is what catches a normalization that moves one surface and not
    /// the other, whatever the spelling of it.
    #[test]
    fn the_pong_and_the_pairing_url_name_the_same_server() {
        for (configured, canonical) in accepted_endpoints() {
            // The pong's value, taken from the slot that publishes it rather
            // than re-derived here, so this compares the two real surfaces.
            let published = crate::api::SharedAdvertisedEndpoint::from_config(
                &crate::config::WebSocketApiConfig {
                    advertised_endpoint: Some(configured.to_string()),
                    ..crate::config::WebSocketApiConfig::default()
                },
            )
            .current()
            .unwrap_or_else(|| panic!("{configured:?} must be published in the pong"));
            assert_eq!(published, canonical, "{configured:?}");

            let payload = PairingPayload {
                endpoint: declared(configured).expect("an accepted endpoint"),
                token: "abcDEF123-_".to_string(),
                name: "Can's Mini (büro)".to_string(),
            };
            let dialled = payload.url();

            assert_eq!(
                client_server_key(&dialled),
                client_server_key(&published),
                "{configured:?}: the pong publishes {published}, the qr dials {dialled}"
            );
            // The name and token are the only query there is, so removing
            // them leaves the endpoint whole — a percent-encoded name with
            // spaces and parentheses does not reach the path.
            assert!(
                dialled.starts_with(&published),
                "{configured:?}: {dialled} must begin with the published endpoint"
            );
        }
    }

    /// The other half of the invariant. A refusal may be stricter than a
    /// client, but it owes the operator a message they can act on — so the
    /// whole message is asserted by equality, not by substring. A substring
    /// check cannot close this: three rounds of review found fragments that
    /// were satisfied incidentally, first by the empty string and then by
    /// text already present elsewhere in the same message. Equality cannot
    /// be satisfied by accident, and it puts the operator-facing wording in
    /// the table where it can be read and reviewed.
    ///
    /// Every refusal branch in the endpoint path has a row here. Deleting a
    /// remedy clause from any of them fails this test.
    #[test]
    fn a_malformed_advertised_endpoint_refuses_to_print_a_payload() {
        fn row(configured: &str, refusal: &str) -> (String, String) {
            (configured.to_string(), refusal.to_string())
        }

        // Long enough to exceed the 253-character name limit while every
        // label stays legal, so the name-length branch is what refuses it.
        let long_label = "a".repeat(63);
        let over_long_name = [long_label.as_str(); 4].join(".");
        // One label over the 63-character limit, inside a legal-length name.
        let over_long_label = "a".repeat(64);

        let mut rows = vec![
            // No scheme at all: nothing says how to dial it.
            row("a-host.example.ts.net", "expected a ws:// or wss:// url"),
            // A scheme no websocket client can dial.
            row("https://a-host.example.ts.net", "a websocket client cannot dial https://; use ws:// or wss://"),
            // Scheme but no authority.
            row("wss://", "no host to dial; declare scheme, host, and optional port, e.g. wss://a-host.example.net:8443"),
            // Whitespace anywhere in the url, host or path.
            row("wss://a host.example.ts.net", "a url cannot contain whitespace; declare it as one unbroken url, e.g. wss://a-host.example.net:8443"),
            row("wss://a-host.example.ts.net/herdr ws", "a url cannot contain whitespace; declare it as one unbroken url, e.g. wss://a-host.example.net:8443"),
            // Userinfo, with and without a path behind it.
            row("wss://user@a-host.example.ts.net", "credentials do not belong in a pairing endpoint; declare the host alone and let the token carry authorization, e.g. wss://a-host.example.net:8443"),
            row("wss://user@a-host.example.ts.net/herdr-ws", "credentials do not belong in a pairing endpoint; declare the host alone and let the token carry authorization, e.g. wss://a-host.example.net:8443"),
            // A query collides with the token the payload appends; a fragment
            // never reaches the server at all.
            row("wss://a-host.example.ts.net/?token=x", "a query would collide with the token the pairing payload appends; declare scheme, host, optional port, and optional path only, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/herdr-ws?x=1", "a query would collide with the token the pairing payload appends; declare scheme, host, optional port, and optional path only, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/herdr-ws#frag", "a fragment is never sent to a server, so it cannot be part of the url a client dials; declare scheme, host, optional port, and optional path only, e.g. wss://a-host.example.net/herdr-ws"),
            // Both at once is refused by whichever the url reaches first, so
            // the message names the delimiter that is actually in front.
            row("wss://a-host.example.ts.net/herdr-ws?x=1#frag", "a query would collide with the token the pairing payload appends; declare scheme, host, optional port, and optional path only, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/herdr-ws#frag?x=1", "a fragment is never sent to a server, so it cannot be part of the url a client dials; declare scheme, host, optional port, and optional path only, e.g. wss://a-host.example.net/herdr-ws"),
            // A backslash is a path separator for ws/wss, so a client dials a
            // url spelled differently than the one declared.
            row("wss://a-host\\api", "a backslash is read as a path separator, so a client would dial a url spelled differently than the one declared; write path separators as forward slashes, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/herdr\\ws", "a backslash is read as a path separator, so a client would dial a url spelled differently than the one declared; write path separators as forward slashes, e.g. wss://a-host.example.net/herdr-ws"),
            // A path with no host in front of it.
            row("wss:///herdr-ws", "no host to dial; declare scheme, host, and optional port, e.g. wss://a-host.example.net:8443"),
            // Dot segments are resolved away before a client dials, so the
            // path dialled is not the path declared.
            row("wss://a-host.example.ts.net/api/../herdr-ws", "\"..\" is resolved away before a client dials, so it would dial a different path than the one declared; write the path out, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/./herdr-ws", "\".\" is resolved away before a client dials, so it would dial a different path than the one declared; write the path out, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/herdr-ws/..", "\"..\" is resolved away before a client dials, so it would dial a different path than the one declared; write the path out, e.g. wss://a-host.example.net/herdr-ws"),
            // An empty segment: a doubled slash inside the path, and the one
            // a second trailing slash leaves behind.
            row("wss://a-host.example.ts.net/api//herdr-ws", "a path segment cannot be empty; write single slashes between the segments, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net//", "a path segment cannot be empty; write single slashes between the segments, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/herdr-ws//", "a path segment cannot be empty; write single slashes between the segments, e.g. wss://a-host.example.net/herdr-ws"),
            // A percent escape is preserved by some parsers and decoded by
            // others, so the path this server answers at would depend on
            // which client dialled it.
            row("wss://a-host.example.ts.net/herdr%2Fws", "a percent escape is not preserved identically by every client and proxy, so the path dialled would not reliably be the one declared; write the path in plain characters, e.g. wss://a-host.example.net/herdr-ws"),
            // Everything else outside the set that provably round-trips.
            row("wss://a-host.example.ts.net/herdr@ws", "\"herdr@ws\" is not a path segment this endpoint can promise to round-trip; a path may hold only ascii letters, digits, hyphens, dots, underscores, and tildes, which every client dials unchanged, e.g. wss://a-host.example.net/herdr-ws"),
            row("wss://a-host.example.ts.net/naïve", "\"naïve\" is not a path segment this endpoint can promise to round-trip; a path may hold only ascii letters, digits, hyphens, dots, underscores, and tildes, which every client dials unchanged, e.g. wss://a-host.example.net/herdr-ws"),
            // Bracket opened and never closed.
            row("wss://[fd7a::1", "unterminated ipv6 literal; brackets must be closed, e.g. wss://[fd7a::1]:8443"),
            // Something other than a port after the closing bracket.
            row("wss://[fd7a::1]x", "expected a port after the ipv6 literal, found \"x\"; write it as wss://[fd7a::1]:8443"),
            // An unbracketed ipv6 literal is ambiguous with host:port.
            row("wss://fd7a::1", "an ipv6 literal must be wrapped in brackets, e.g. wss://[fd7a::1]:8443"),
            // Brackets promise an ipv6 address; a client throws when the
            // contents are not one, rather than reading them as a name.
            row("wss://[not-an-ip]", "\"not-an-ip\" is bracketed but is not an ipv6 address, and brackets are only for ipv6 literals; write an address as wss://[fd7a::1]:8443 or a name without brackets as wss://a-host.example.net:8443"),
            row("wss://[not-an-ip]:8443", "\"not-an-ip\" is bracketed but is not an ipv6 address, and brackets are only for ipv6 literals; write an address as wss://[fd7a::1]:8443 or a name without brackets as wss://a-host.example.net:8443"),
            row("wss://[]", "\"\" is bracketed but is not an ipv6 address, and brackets are only for ipv6 literals; write an address as wss://[fd7a::1]:8443 or a name without brackets as wss://a-host.example.net:8443"),
            // A port with no host in front of it.
            row("wss://:8443", "no host to dial; declare scheme, host, and optional port, e.g. wss://a-host.example.net:8443"),
            // Number-shaped hosts: a client either throws (proxy.0x10,
            // 10.0.0.999, 1.2.3.4.5) or dials an address sharing no text with
            // what was written (0x7f000001 and 0177.0.0.1 become 127.0.0.1,
            // 010.0.0.1 becomes 8.0.0.1, 127.1 becomes 127.0.0.1, and the
            // trailing dot of 192.0.2.1. is dropped).
            row("wss://0x7f000001", "\"0x7f000001\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://proxy.0x10", "\"proxy.0x10\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://127.1", "\"127.1\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://192.0.2.1.", "\"192.0.2.1.\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://010.0.0.1", "\"010.0.0.1\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://10.0.0.05", "\"10.0.0.05\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://0177.0.0.1", "\"0177.0.0.1\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://10.0.0.999", "\"10.0.0.999\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            row("wss://1.2.3.4.5", "\"1.2.3.4.5\" is read as an ipv4 address, not a name, because it ends in a number, and a client would refuse it or dial a different address than it spells; write it as four plain decimal octets 0-255 with no leading zeros and no trailing dot, e.g. 100.64.0.5"),
            // Empty labels are not names.
            row("wss://a-host..example.net", "\"a-host..example.net\" has an empty label, and a host name cannot contain \"..\" or start with a dot; write the labels out, e.g. wss://a-host.example.net:8443"),
            row("wss://.example.net", "\".example.net\" has an empty label, and a host name cannot contain \"..\" or start with a dot; write the labels out, e.g. wss://a-host.example.net:8443"),
            // Hyphen-edged labels are not resolvable names.
            row("wss://-a-host.example.net", "\"-a-host.example.net\" has a label starting or ending with a hyphen, which is not a resolvable name; hyphens may only sit inside a label, e.g. wss://a-host.example.net:8443"),
            row("wss://a-host-.example.net", "\"a-host-.example.net\" has a label starting or ending with a hyphen, which is not a resolvable name; hyphens may only sit inside a label, e.g. wss://a-host.example.net:8443"),
            // Non-ascii is refused rather than converted, and a stray ascii
            // symbol lands in the same branch.
            row("wss://naïve.example.net", "\"naïve.example.net\" is not an ipv4 literal, a bracketed ipv6 literal, or a host name; a name may hold only ascii letters, digits, hyphens, and underscores. A non-ascii name has to be declared in its punycode (xn--) form, which is what a client resolves it to anyway"),
            row("wss://a$host.example.net", "\"a$host.example.net\" is not an ipv4 literal, a bracketed ipv6 literal, or a host name; a name may hold only ascii letters, digits, hyphens, and underscores. A non-ascii name has to be declared in its punycode (xn--) form, which is what a client resolves it to anyway"),
            // A port must be plain digits: a signed port parses in rust but
            // throws in a client, and an empty one is not a port at all.
            row("wss://a-host.example.ts.net:https", "\"https\" is not a port a client could dial; write plain digits, e.g. 8443"),
            row("wss://a-host.example.ts.net:+443", "\"+443\" is not a port a client could dial; write plain digits, e.g. 8443"),
            row("wss://a-host.example.ts.net:", "\"\" is not a port a client could dial; write plain digits, e.g. 8443"),
            // A leading zero is stripped by a client, so the payload text would
            // stop naming the port dialled; the remedy names the plain form.
            row("wss://a-host.example.ts.net:0443", "\"0443\" has a leading zero, which a client strips before dialling; write the port plainly, e.g. 443"),
            // An all-zero port strips to nothing and a leading-zero port over the
            // range strips to a value refused anyway, so neither may be echoed
            // back as the remedy: both fall through to the range message.
            row("wss://a-host.example.ts.net:00", "\"00\" is not a port a client could dial; use a port in 1-65535, e.g. 8443"),
            row("wss://a-host.example.ts.net:099999", "\"099999\" is not a port a client could dial; use a port in 1-65535, e.g. 8443"),
            // Out of range, with and without a leading zero.
            row("wss://a-host.example.ts.net:0", "\"0\" is not a port a client could dial; use a port in 1-65535, e.g. 8443"),
            row("wss://a-host.example.ts.net:99999", "\"99999\" is not a port a client could dial; use a port in 1-65535, e.g. 8443"),
        ];
        rows.push(row(
            &format!("wss://{over_long_name}"),
            "a host name cannot exceed 253 characters; declare the name the proxy actually serves, e.g. wss://a-host.example.net:8443",
        ));
        rows.push(row(
            &format!("wss://{over_long_label}"),
            &format!(
                "{over_long_label:?} has a label longer than 63 characters, which no \
                 resolver accepts; keep each label at 63 or fewer, \
                 e.g. wss://a-host.example.net:8443"
            ),
        ));

        for (configured, expected) in rows {
            let refusal = declared(&configured)
                .expect_err(&format!("{configured:?} must be refused, not encoded"));
            assert_eq!(refusal, expected, "refusal text for {configured:?}");
        }
    }

    #[test]
    fn invalid_advertised_endpoint_explanation_names_the_config_and_the_fix() {
        let explanation = PairingUnavailable::InvalidAdvertisedEndpoint {
            advertised: "https://a-host.example.ts.net".to_string(),
            reason: "only ws:// or wss:// urls can be dialed".to_string(),
        }
        .explanation(Path::new("/home/u/config.toml"));

        assert!(explanation.contains("advertised_endpoint"));
        assert!(explanation.contains("https://a-host.example.ts.net"));
        assert!(explanation.contains("only ws:// or wss:// urls can be dialed"));
        assert!(explanation.contains("/home/u/config.toml"));
        // An explanation never doubles as a payload.
        assert!(!explanation.contains("?token="));
    }

    #[test]
    fn percent_encoding_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(
            percent_encode_query_value("plain-Name_0.~"),
            "plain-Name_0.~"
        );
        assert_eq!(percent_encode_query_value("a b"), "a%20b");
        assert_eq!(percent_encode_query_value("a&b=c?d#e"), "a%26b%3Dc%3Fd%23e");
        assert_eq!(percent_encode_query_value("naïve"), "na%C3%AFve");
        assert_eq!(percent_encode_query_value(""), "");
    }

    #[test]
    fn rotation_replaces_the_stored_token_and_preserves_the_rest() {
        let content = "# comment\n[websocket_api]\nbind = \"127.0.0.1:4433\"\ntoken = \"old-token\"\n\n[ui]\nmouse_capture = true\n";

        let updated = rotate_token_in_config(content, "new-token");

        let config: crate::config::Config = toml::from_str(&updated).unwrap();
        assert_eq!(config.websocket_api.token.as_deref(), Some("new-token"));
        assert!(!updated.contains("old-token"), "{updated}");
        assert!(updated.contains("# comment"));
        assert!(updated.contains("mouse_capture = true"));
        assert_eq!(config.websocket_api.bind.as_deref(), Some("127.0.0.1:4433"));
    }

    #[test]
    fn rotation_adds_a_token_when_the_section_has_none() {
        let content = "[websocket_api]\nbind = \"127.0.0.1:4433\"\n";

        let updated = rotate_token_in_config(content, "first-token");

        let config: crate::config::Config = toml::from_str(&updated).unwrap();
        assert_eq!(config.websocket_api.token.as_deref(), Some("first-token"));
        assert_eq!(config.websocket_api.bind.as_deref(), Some("127.0.0.1:4433"));
    }

    #[test]
    fn rotating_twice_keeps_only_the_latest_token() {
        let content = "[websocket_api]\nbind = \"127.0.0.1:4433\"\n";

        let first = rotate_token_in_config(content, "token-one");
        let second = rotate_token_in_config(&first, "token-two");

        let config: crate::config::Config = toml::from_str(&second).unwrap();
        assert_eq!(config.websocket_api.token.as_deref(), Some("token-two"));
        assert!(!second.contains("token-one"), "{second}");
    }

    #[test]
    fn not_configured_explanation_names_the_required_config() {
        let explanation =
            PairingUnavailable::NotConfigured.explanation(Path::new("/home/u/config.toml"));

        assert!(explanation.contains("[websocket_api]"));
        assert!(explanation.contains("bind = "));
        assert!(explanation.contains("/home/u/config.toml"));
        assert!(explanation.contains("herdr pair"));
        // No payload fragments in an explanation.
        assert!(!explanation.contains("?token="));

        // The steps must put pairing before the server (re)start: a server
        // with `bind` set and no stored token refuses to start.
        let pair_step = explanation.find("herdr pair").unwrap();
        let start_step = explanation.find("start (or restart)").unwrap();
        assert!(pair_step < start_step, "{explanation}");
    }

    #[test]
    fn qr_renders_the_pairing_url_as_terminal_blocks() {
        let qr = qr_code_text("ws://100.64.0.5:4433/?token=abc").unwrap();

        assert!(qr.lines().count() > 10, "{qr}");
        assert!(
            qr.contains('█') || qr.contains('▀') || qr.contains('▄'),
            "expected unicode block modules: {qr}"
        );
    }

    /// The control sockets are ordered by the config file being written,
    /// because on a machine running one server per config the socket this
    /// command's own environment names need never belong to a server that
    /// ever read that file. Ordering is all this is: the handshake, not the
    /// reload, is what the report rests on.
    #[test]
    fn the_control_socket_order_starts_with_the_config_being_written() {
        let ambient = PathBuf::from("/home/u/.config/herdr/herdr.sock");
        let environment_config = PathBuf::from("/home/u/.config/herdr/config.toml");

        // The ordinary one-config machine: the config being paired is this
        // environment's own, so the socket it names is the likely owner.
        assert_eq!(
            control_sockets_to_try(&environment_config, &environment_config, ambient.clone()),
            ControlSockets {
                beside_config: None,
                ambient: ambient.clone(),
            }
        );

        // A named session serves its own socket and shares the same config
        // file, so that socket stays the one to try.
        let session = PathBuf::from("/home/u/.config/herdr/sessions/mobile/herdr.sock");
        assert_eq!(
            control_sockets_to_try(&environment_config, &environment_config, session.clone()),
            ControlSockets {
                beside_config: None,
                ambient: session,
            }
        );

        // A config file selected explicitly: the server whose home directory
        // is that config's own comes first, and the ambient socket — which
        // may be a server holding an entirely different config — only after.
        let selected = PathBuf::from("/home/u/.config/herdr-second/config.toml");
        let beside = PathBuf::from("/home/u/.config/herdr-second/herdr.sock");
        let sockets = control_sockets_to_try(&selected, &environment_config, ambient.clone());
        assert_eq!(
            sockets,
            ControlSockets {
                beside_config: Some(beside.clone()),
                ambient: ambient.clone(),
            }
        );
        assert_eq!(sockets.in_order(), vec![&beside, &ambient]);

        // A different file in the same directory is the same server; nothing
        // is dialed twice.
        let sibling = PathBuf::from("/home/u/.config/herdr/other.toml");
        assert_eq!(
            control_sockets_to_try(&sibling, &environment_config, ambient.clone()),
            ControlSockets {
                beside_config: None,
                ambient,
            }
        );
    }

    /// The proof exercises the url the operator was handed, which is the
    /// payload endpoint and not the bind address behind it — except for the
    /// one url this build cannot open, where what went unexercised is
    /// recorded instead of quietly swapped.
    #[test]
    fn the_proof_dials_the_url_the_payload_names() {
        let listener = addr("127.0.0.1:4433");

        // No advertised endpoint: the payload names the bind address, and
        // that is this machine's own listener.
        assert_eq!(
            plan_token_check("ws://127.0.0.1:4433", false, listener),
            TokenCheck {
                dial: DialTarget {
                    host: "127.0.0.1".to_string(),
                    port: 4433,
                    url: "ws://127.0.0.1:4433".to_string(),
                },
                scope: ProofScope {
                    dialed: "ws://127.0.0.1:4433".to_string(),
                    dialed_is_local_listener: true,
                    unexercised_printed_url: None,
                },
            }
        );

        // An advertised ws:// url is dialed as written, port and path
        // included — the bind address behind it is not what was printed.
        assert_eq!(
            plan_token_check("ws://a-host.example.net:8443/herdr-ws", true, listener),
            TokenCheck {
                dial: DialTarget {
                    host: "a-host.example.net".to_string(),
                    port: 8443,
                    url: "ws://a-host.example.net:8443/herdr-ws".to_string(),
                },
                scope: ProofScope {
                    dialed: "ws://a-host.example.net:8443/herdr-ws".to_string(),
                    dialed_is_local_listener: false,
                    unexercised_printed_url: None,
                },
            }
        );

        // No port means the scheme's default, which is what a client dials.
        assert_eq!(
            plan_token_check("ws://a-host.example.net", true, listener).dial,
            DialTarget {
                host: "a-host.example.net".to_string(),
                port: 80,
                url: "ws://a-host.example.net".to_string(),
            }
        );

        // An ipv6 literal is bracketed in a url and bare in a connect call.
        assert_eq!(
            plan_token_check("ws://[fd7a::1]:8443", true, listener).dial,
            DialTarget {
                host: "fd7a::1".to_string(),
                port: 8443,
                url: "ws://[fd7a::1]:8443".to_string(),
            }
        );

        // The deployment this feature exists for: a wss:// url this build has
        // no TLS client for. The listener behind it is dialed instead, and
        // the printed url is recorded as unexercised so every message says so
        // rather than reporting the backend's answer as the payload's.
        assert_eq!(
            plan_token_check("wss://a-host.example.net/herdr-ws", true, listener),
            TokenCheck {
                dial: DialTarget {
                    host: "127.0.0.1".to_string(),
                    port: 4433,
                    url: "ws://127.0.0.1:4433".to_string(),
                },
                scope: ProofScope {
                    dialed: "ws://127.0.0.1:4433".to_string(),
                    dialed_is_local_listener: true,
                    unexercised_printed_url: Some("wss://a-host.example.net/herdr-ws".to_string()),
                },
            }
        );
    }

    /// The defect this command carried, twice over: a reload that succeeded
    /// and a port that answers say nothing about which token is held, and a
    /// websocket upgrade says nothing about who holds it. Only a herdr peer
    /// that accepted the token is live.
    #[test]
    fn only_a_herdr_peer_that_accepted_the_token_is_reported_as_live() {
        let socket = PathBuf::from("/home/u/.config/herdr/herdr.sock");
        let reloaded = || ConfigReload::Reloaded {
            socket: socket.clone(),
        };
        let no_answer = || ConfigReload::NoAnswer {
            tried: vec![ControlSocketAttempt {
                socket: socket.clone(),
                outcome: ControlSocketOutcome::NotRunning,
            }],
        };
        let failed = || ConfigReload::RequestFailed {
            socket: socket.clone(),
            reason: "boom".to_string(),
        };
        let refused = || TokenProof::RefusedWithoutIdentifying { status: 401 };

        // A reloaded server plus a refused handshake is the shape that used
        // to print "the listener accepts the new token now".
        assert_eq!(
            decide_activation(reloaded(), refused(), None),
            TokenActivation::RefusedWithoutIdentifying {
                status: 401,
                reload: reloaded()
            }
        );

        // A websocket service that never answered as herdr is not a listener
        // holding this token, whatever the upgrade said.
        assert_eq!(
            decide_activation(
                reloaded(),
                TokenProof::NotHerdr("it answered nothing".to_string()),
                None
            ),
            TokenActivation::NotHerdr {
                reason: "it answered nothing".to_string(),
                reload: reloaded()
            }
        );

        // Live means observed live, and the previous token's fate is a
        // separate observation rather than something the first one implies.
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Accepted, Some(refused())),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Refused,
                reload: reloaded()
            }
        );
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Accepted, Some(TokenProof::Accepted)),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::StillAccepted,
                reload: reloaded()
            }
        );
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Accepted, None),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Unproven,
                reload: reloaded()
            }
        );
        assert_eq!(
            decide_activation(
                reloaded(),
                TokenProof::Accepted,
                Some(TokenProof::Inconclusive("eof".to_string()))
            ),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Unproven,
                reload: reloaded()
            }
        );

        // The handshake decides in both directions, and the reload rides
        // along rather than being replaced by it: a peer that accepted the
        // token makes the payload live whichever process this command
        // reached, or failed to reach.
        assert_eq!(
            decide_activation(no_answer(), TokenProof::Accepted, Some(refused())),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Refused,
                reload: no_answer()
            }
        );
        assert_eq!(
            decide_activation(failed(), TokenProof::Accepted, None),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Unproven,
                reload: failed()
            }
        );

        assert_eq!(
            decide_activation(reloaded(), TokenProof::NoListener, None),
            TokenActivation::NothingListening { reload: reloaded() }
        );
        assert_eq!(
            decide_activation(no_answer(), TokenProof::NoListener, None),
            TokenActivation::NothingListening {
                reload: no_answer()
            }
        );
        assert_eq!(
            decide_activation(
                reloaded(),
                TokenProof::Inconclusive("http 502".to_string()),
                None
            ),
            TokenActivation::Unverified {
                reason: "http 502".to_string(),
                reload: reloaded()
            }
        );

        // Neither observation may swallow the other. A failed reload with a
        // peer that never identified itself is both of those things, and a
        // refusal after nothing answered a control socket is too.
        assert_eq!(
            decide_activation(
                failed(),
                TokenProof::NotHerdr("it went quiet".to_string()),
                None
            ),
            TokenActivation::NotHerdr {
                reason: "it went quiet".to_string(),
                reload: failed()
            }
        );
        assert_eq!(
            decide_activation(no_answer(), refused(), None),
            TokenActivation::RefusedWithoutIdentifying {
                status: 401,
                reload: no_answer()
            }
        );
        assert_eq!(
            decide_activation(failed(), TokenProof::NoListener, None),
            TokenActivation::NothingListening { reload: failed() }
        );
    }

    /// A real listener, because the proof this command rests on is a real
    /// exchange: the same address answers, and the only thing that tells the
    /// freshly minted token from the one it replaced is presenting them.
    #[test]
    fn a_handshake_tells_the_new_token_from_the_previous_one() {
        let listener = start_listener_holding("the-new-token");
        let endpoint = listener.handle.local_addr();

        assert_eq!(
            probe_token(&local_dial(endpoint), "the-new-token"),
            TokenProof::Accepted
        );
        // The listener's refusal is a bare 401 by design, so this says the
        // connection did not get in and stops there: it names no peer.
        assert_eq!(
            probe_token(&local_dial(endpoint), "the-previous-token"),
            TokenProof::RefusedWithoutIdentifying { status: 401 }
        );

        // A payload whose url carries a path dials that path, and the token
        // still rides the query the listener reads.
        let with_path = DialTarget {
            url: format!("ws://{endpoint}/herdr-ws"),
            ..local_dial(endpoint)
        };
        assert_eq!(
            probe_token(&with_path, "the-new-token"),
            TokenProof::Accepted
        );

        drop(listener);
        assert_eq!(
            probe_token(&local_dial(endpoint), "the-new-token"),
            TokenProof::NoListener,
            "a released port must not read as a listener"
        );
    }

    /// Any compliant websocket service answers 101, so an upgrade identifies
    /// a service and not this program. Until the peer answers as herdr the
    /// token is unproven — and the previous token, which is still a live
    /// credential, is never handed to it.
    #[test]
    fn a_service_that_is_not_herdr_never_receives_the_previous_token() {
        let peer = start_websocket_service_that_is_not_herdr();
        let dial = local_dial(peer.addr);

        let (new_token, previous_token) =
            prove_token(&dial, "the-new-token", Some("the-previous-token"));

        match new_token {
            TokenProof::NotHerdr(reason) => assert!(!reason.is_empty(), "{reason}"),
            other => panic!("a peer that answered no ping must not be accepted: {other:?}"),
        }
        assert_eq!(
            previous_token, None,
            "the previous token must not be presented to an unidentified peer"
        );

        let presented = peer.presented();
        assert_eq!(presented.len(), 1, "{presented:?}");
        assert!(presented[0].contains("the-new-token"), "{presented:?}");
        assert!(
            !presented
                .iter()
                .any(|query| query.contains("the-previous-token")),
            "the previous credential must never reach an unidentified peer: {presented:?}"
        );
    }

    /// Something else holding the port is not a herdr listener refusing the
    /// token, and must not be reported as either.
    #[test]
    fn a_socket_that_never_answers_the_handshake_proves_nothing() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = socket.local_addr().expect("bound address");
        let accepter = std::thread::spawn(move || {
            if let Ok((stream, _)) = socket.accept() {
                drop(stream);
            }
        });

        match probe_token(&local_dial(endpoint), "a-token") {
            TokenProof::Inconclusive(reason) => assert!(reason.contains("handshake"), "{reason}"),
            other => panic!("expected an inconclusive proof, got {other:?}"),
        }

        accepter.join().expect("accepter thread");
    }

    /// A host that does not resolve is neither an acceptance nor a refusal,
    /// and the reason has to name it so an operator knows what to fix. An ip
    /// literal skips the resolver, which is what keeps the local probe off
    /// the network entirely.
    #[test]
    fn a_host_that_does_not_resolve_is_inconclusive_rather_than_refused() {
        assert_eq!(
            resolve_bounded("127.0.0.1", 4433),
            Ok(vec![addr("127.0.0.1:4433")])
        );
        assert_eq!(resolve_bounded("::1", 4433), Ok(vec![addr("[::1]:4433")]));

        // A space is not a host any resolver accepts, so this is refused
        // locally rather than depending on what this machine's dns does with
        // an unknown name.
        match resolve_bounded("not a host", 8443) {
            Err(TokenProof::Inconclusive(reason)) => {
                assert!(reason.contains("not a host"), "{reason}")
            }
            other => panic!("expected an inconclusive resolution, got {other:?}"),
        }
    }

    /// A peer that never stops sending and never finishes anything must end
    /// in a named outcome, and the deadline is what has to end it.
    ///
    /// Both peers here drip *partial bytes* — an http header that never
    /// completes, and a websocket frame that never completes — because that
    /// is the shape a per-syscall read timeout cannot bound: the handshake
    /// and a single `read()` each issue several reads, so a byte arriving
    /// under the interval keeps every syscall successful while the exchange
    /// makes no progress. A peer sending whole frames would only exercise the
    /// outer loop, which was bounded already; this reaches the reads inside.
    ///
    /// Deleting the per-read recomputation fails this: both peers stop
    /// dripping well after the budget, so the outcome becomes whatever their
    /// close produces and the elapsed assertion trips.
    #[test]
    fn a_peer_that_drips_partial_bytes_hits_the_deadline() {
        let budget = Duration::from_millis(300);

        for (what, addr) in [
            ("an http header", start_service_that_drips_a_handshake()),
            ("a websocket frame", start_service_that_drips_a_frame()),
        ] {
            let started = std::time::Instant::now();
            let proof = probe_token_within(&local_dial(addr), "a-token", budget);
            let elapsed = started.elapsed();

            match proof {
                TokenProof::Inconclusive(reason) => assert!(
                    reason.contains("300ms"),
                    "{what}: the bound must be named: {reason}"
                ),
                other => panic!("{what}: expected the deadline to end this, got {other:?}"),
            }
            assert!(
                elapsed < budget * 6,
                "{what}: the exchange ran {elapsed:?}, which is not bounded by {budget:?}"
            );
        }
    }

    /// A stale process that accepts a control socket and never answers must
    /// not hold pairing open: the attempt is bounded, recorded for the
    /// report, and the next candidate is still tried.
    #[cfg(unix)]
    #[test]
    fn a_control_socket_that_never_answers_is_bounded_and_moves_on() {
        let socket_path = test_socket_path("stale");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind a unix socket");
        let accepted = std::thread::spawn(move || {
            // Accept and hold: never write a byte back.
            let held = listener.accept();
            std::thread::sleep(Duration::from_millis(1500));
            drop(held);
        });

        let timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let reload = reload_config_at(&socket_path, timeout);
        let elapsed = started.elapsed();

        assert_eq!(
            reload,
            ConfigReload::NoAnswer {
                tried: vec![ControlSocketAttempt {
                    socket: socket_path.clone(),
                    outcome: ControlSocketOutcome::Unfinished,
                }]
            }
        );
        assert!(
            elapsed < timeout * 6,
            "the control socket attempt ran {elapsed:?}, which is not bounded by {timeout:?}"
        );

        accepted.join().expect("accepter thread");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[cfg(unix)]
    fn test_socket_path(name: &str) -> PathBuf {
        // Under the crate's own target directory, because a unix socket path
        // has to stay inside sun_path and another test in this binary
        // redirects and deletes the process temp dir.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-sockets");
        std::fs::create_dir_all(&dir).expect("socket directory");
        dir.join(format!("{name}-{}.sock", std::process::id()))
    }

    fn local_dial(addr: SocketAddr) -> DialTarget {
        plan_token_check(&format!("ws://{addr}"), false, addr).dial
    }

    /// A listener holding `token`, to present tokens to.
    struct TestListener {
        handle: crate::api::WebSocketServerHandle,
        _api_rx: tokio::sync::mpsc::UnboundedReceiver<crate::api::ApiRequestMessage>,
    }

    fn start_listener_holding(token: &str) -> TestListener {
        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::WebSocketApiConfig {
            bind: Some("127.0.0.1:0".to_string()),
            token: Some(token.to_string()),
            ..Default::default()
        };
        let handle = crate::api::start_websocket_server_with_capabilities(
            &config,
            api_tx,
            crate::api::EventHub::default(),
            crate::api::SharedCredentialRegistry::open(
                crate::api::credentials::test_registry_path("pair"),
            ),
            None,
            crate::api::SharedServerName::from_config(&config),
            crate::api::SharedServerReach::from_config(&config),
            crate::api::SharedAdvertisedEndpoint::from_config(&config),
        )
        .expect("the listener starts")
        .expect("bind is configured, so it binds");
        TestListener {
            handle,
            _api_rx: api_rx,
        }
    }

    /// A websocket service that is not herdr: it completes every handshake,
    /// records the query each client presented, and answers no request.
    struct FakePeer {
        addr: SocketAddr,
        presented: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakePeer {
        fn presented(&self) -> Vec<String> {
            self.presented.lock().expect("recorded handshakes").clone()
        }
    }

    fn start_websocket_service_that_is_not_herdr() -> FakePeer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("bound address");
        let presented = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&presented);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let recorder = std::sync::Arc::clone(&recorder);
                // The handshake callback's error type is tungstenite's own
                // http response, which is large; this fake never returns one,
                // and boxing a type this test does not construct would only
                // obscure what it is doing.
                #[allow(clippy::result_large_err)]
                let accepted = tungstenite::accept_hdr(
                    stream,
                    |request: &tungstenite::handshake::server::Request,
                     response: tungstenite::handshake::server::Response|
                     -> Result<
                        tungstenite::handshake::server::Response,
                        tungstenite::handshake::server::ErrorResponse,
                    > {
                        if let Some(query) = request.uri().query() {
                            if let Ok(mut recorded) = recorder.lock() {
                                recorded.push(query.to_string());
                            }
                        }
                        Ok(response)
                    },
                );
                if let Ok(mut websocket) = accepted {
                    // Read whatever the client asks and answer none of it:
                    // an upgrade is all this service has to offer.
                    let _ = websocket.read();
                    let _ = websocket.close(None);
                }
            }
        });

        FakePeer { addr, presented }
    }

    /// A service that starts an http response and never finishes it, one
    /// byte at a time. The client is left assembling headers, which is
    /// several reads inside one handshake call.
    fn start_service_that_drips_a_handshake() -> SocketAddr {
        drip(|stream| {
            let _ = stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\n");
            drip_bytes(stream, b"Upgrade: websocket\r\nConnection: Upgrade\r\n");
        })
    }

    /// A service that completes the handshake and then starts a frame it
    /// never finishes: a text-frame header promising 125 bytes, followed by a
    /// trickle that never reaches them. The client is left assembling one
    /// frame, which is several reads inside one `read()`.
    fn start_service_that_drips_a_frame() -> SocketAddr {
        drip(|stream| {
            #[allow(clippy::result_large_err)]
            let accepted = tungstenite::accept(stream.try_clone().expect("clone"));
            let Ok(mut websocket) = accepted else { return };
            let raw = websocket.get_mut();
            let _ = raw.write_all(&[0x81, 125]);
            drip_bytes(raw, b"a herdr pong would go here, byte by byte, forever");
        })
    }

    /// Write one byte at a time, slower than a whole exchange would take but
    /// faster than any single read would wait — the interval a silence timer
    /// can never fire under. Bounded so a bug ends the test with a failure
    /// rather than a hang.
    fn drip_bytes(stream: &mut std::net::TcpStream, bytes: &[u8]) {
        let deadline = std::time::Instant::now() + Duration::from_millis(2500);
        for byte in bytes.iter().cycle() {
            if std::time::Instant::now() >= deadline || stream.write_all(&[*byte]).is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn drip(serve: fn(&mut std::net::TcpStream)) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("bound address");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                serve(&mut stream);
            }
        });
        addr
    }

    /// Every outcome, whole-message. The wording is the deliverable here: an
    /// operator who reads "applied and live" and finds it false stops
    /// believing anything else this command prints, so each message is
    /// asserted by equality rather than by substring.
    ///
    /// Each row states two observations that must not stand in for each
    /// other — what the reload attempt did, and what the handshake found —
    /// and names the url it was proved against. Every outcome is covered at
    /// the TLS scope too, because that is where a suffix can contradict the
    /// sentence it follows, and a combination no human reads is a combination
    /// nothing checks.
    #[test]
    fn activation_report_spells_out_each_outcome() {
        let config_path = Path::new("/home/u/.config/herdr/config.toml");
        let local = ProofScope {
            dialed: "ws://100.64.0.5:4433".to_string(),
            dialed_is_local_listener: true,
            unexercised_printed_url: None,
        };
        let advertised = ProofScope {
            dialed: "ws://a-host.example.net:8443/herdr-ws".to_string(),
            dialed_is_local_listener: false,
            unexercised_printed_url: None,
        };
        let behind_tls = ProofScope {
            dialed: "ws://127.0.0.1:4433".to_string(),
            dialed_is_local_listener: true,
            unexercised_printed_url: Some("wss://a-host.example.net/herdr-ws".to_string()),
        };
        let tls_note = "\nThe printed url wss://a-host.example.net/herdr-ws was not dialed: this build has no TLS client, so the proof was attempted against the listener behind it instead. Nothing here says whether wss://a-host.example.net/herdr-ws carries a connection to that listener; connect to it with the printed token to find out.";
        let rotated = "Rotated: the previous token was replaced in config.toml";

        let reloaded = ConfigReload::Reloaded {
            socket: PathBuf::from("/home/u/.config/herdr/herdr.sock"),
        };
        let reloaded_clause =
            "A herdr server at /home/u/.config/herdr/herdr.sock reloaded its config";
        let no_answer = ConfigReload::NoAnswer {
            tried: vec![
                ControlSocketAttempt {
                    socket: PathBuf::from("/home/u/.config/herdr-second/herdr.sock"),
                    outcome: ControlSocketOutcome::NotRunning,
                },
                ControlSocketAttempt {
                    socket: PathBuf::from("/home/u/.config/herdr/herdr.sock"),
                    outcome: ControlSocketOutcome::Unfinished,
                },
            ],
        };
        let no_answer_clause = "No running herdr server answered a control socket (/home/u/.config/herdr-second/herdr.sock (not running), /home/u/.config/herdr/herdr.sock (the reload request did not finish within 2s))";
        let failed = ConfigReload::RequestFailed {
            socket: PathBuf::from("/home/u/.config/herdr/herdr.sock"),
            reason: "boom".to_string(),
        };
        let failed_clause =
            "A reload request to /home/u/.config/herdr/herdr.sock did not come back readable: boom";
        let refusal_warning = |dialed: &str| {
            format!(
            "warning: the printed token did not get in, and the refusal names no one: herdr answers an unknown token with the same opaque refusal any other service would, and whatever answered may be something in front of the listener rather than a herdr server at all. Which process refused, and which config it holds, are not established. Find out what serves {dialed} — `herdr session list --json` names each running server and its socket path — and if that turns out to be a herdr server, `HERDR_SOCKET_PATH=<its socket_path> herdr server reload-config` or a restart is what makes it read /home/u/.config/herdr/config.toml."
        )
        };
        let rows = vec![
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Refused,
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. A handshake on ws://100.64.0.5:4433 presenting the new token was accepted by a peer that answered as herdr, and one presenting the previous token was refused."),
                0,
            ),
            // The proxy deployment: the token is proved against the listener,
            // and the url the operator will scan is named as unexercised.
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Refused,
                    reload: reloaded.clone(),
                },
                true,
                &behind_tls,
                format!("{rotated}. A handshake on ws://127.0.0.1:4433 presenting the new token was accepted by a peer that answered as herdr, and one presenting the previous token was refused.{tls_note}"),
                0,
            ),
            // A ws:// proxy url is dialed as printed, so the proof is about
            // the whole path a client takes.
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Refused,
                    reload: reloaded.clone(),
                },
                true,
                &advertised,
                format!("{rotated}. A handshake on ws://a-host.example.net:8443/herdr-ws presenting the new token was accepted by a peer that answered as herdr, and one presenting the previous token was refused."),
                0,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Unproven,
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. A handshake on ws://100.64.0.5:4433 presenting the new token was accepted by a peer that answered as herdr.\nWhether the previous token still authenticates was not established."),
                0,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::StillAccepted,
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. A handshake on ws://100.64.0.5:4433 presenting the new token was accepted by a peer that answered as herdr, but one presenting the previous token was accepted too.\nwarning: the previous token has not stopped authenticating. Restart the herdr server serving ws://100.64.0.5:4433 before treating it as revoked."),
                1,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Unproven,
                    reload: reloaded.clone(),
                },
                false,
                &local,
                format!("{rotated_first}. A handshake on ws://100.64.0.5:4433 presenting the new token was accepted by a peer that answered as herdr.", rotated_first = "Stored the first token in config.toml"),
                0,
            ),
            // The live claim rests on the handshake, and a control socket
            // that errored is still a fault of its own: two facts, two
            // sentences.
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Refused,
                    reload: failed.clone(),
                },
                true,
                &local,
                format!("{rotated}. A handshake on ws://100.64.0.5:4433 presenting the new token was accepted by a peer that answered as herdr, and one presenting the previous token was refused.\nnote: the reload request to /home/u/.config/herdr/herdr.sock did not come back readable: boom. That describes the request, not what that server did with its config."),
                0,
            ),
            (
                TokenActivation::NothingListening {
                    reload: no_answer.clone(),
                },
                true,
                &local,
                format!("{rotated}. {no_answer_clause}, and nothing is listening on ws://100.64.0.5:4433.\nThe token takes effect when the herdr server starts."),
                0,
            ),
            (
                TokenActivation::NothingListening {
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. {reloaded_clause}, and nothing is listening on ws://100.64.0.5:4433.\nRestart the herdr server so it binds the websocket listener, then scan."),
                0,
            ),
            // Nothing answering an advertised url is not a reason to restart
            // a herdr server that may be running perfectly.
            (
                TokenActivation::NothingListening {
                    reload: reloaded.clone(),
                },
                true,
                &advertised,
                format!("{rotated}. {reloaded_clause}, and nothing is listening on ws://a-host.example.net:8443/herdr-ws.\nStart whatever serves ws://a-host.example.net:8443/herdr-ws — the proxy in front, or the herdr listener behind it — then scan."),
                0,
            ),
            // Behind TLS the fallback reached nothing either, and the note
            // says the printed url was only attempted — never that it was
            // reached.
            (
                TokenActivation::NothingListening {
                    reload: no_answer.clone(),
                },
                true,
                &behind_tls,
                format!("{rotated}. {no_answer_clause}, and nothing is listening on ws://127.0.0.1:4433.\nThe token takes effect when the herdr server starts.{tls_note}"),
                0,
            ),
            // A reload that errored, with nothing listening: both stated.
            (
                TokenActivation::NothingListening {
                    reload: failed.clone(),
                },
                true,
                &local,
                format!("{rotated}. {failed_clause}, and nothing is listening on ws://100.64.0.5:4433.\nRestart the herdr server so it binds the websocket listener, then scan."),
                0,
            ),
            (
                TokenActivation::RefusedWithoutIdentifying {
                    status: 401,
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. {reloaded_clause}, and a handshake on ws://100.64.0.5:4433 presenting the new token was refused with http 401.\n{}", refusal_warning("ws://100.64.0.5:4433")),
                1,
            ),
            // Nothing answered a control socket and the peer refused: the
            // report may not say a server answered one.
            (
                TokenActivation::RefusedWithoutIdentifying {
                    status: 403,
                    reload: no_answer.clone(),
                },
                true,
                &local,
                format!("{rotated}. {no_answer_clause}, and a handshake on ws://100.64.0.5:4433 presenting the new token was refused with http 403.\n{}", refusal_warning("ws://100.64.0.5:4433")),
                1,
            ),
            // Behind TLS a reload can only promise the listener, never the
            // path in front of it.
            (
                TokenActivation::RefusedWithoutIdentifying {
                    status: 401,
                    reload: reloaded.clone(),
                },
                true,
                &behind_tls,
                format!("{rotated}. {reloaded_clause}, and a handshake on ws://127.0.0.1:4433 presenting the new token was refused with http 401.\n{}{tls_note}", refusal_warning("ws://127.0.0.1:4433")),
                1,
            ),
            (
                TokenActivation::NotHerdr {
                    reason: "it answered nothing".to_string(),
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. {reloaded_clause}, and something on ws://100.64.0.5:4433 completed a websocket handshake but did not answer as herdr: it answered nothing.\nwarning: nothing about the printed token was established, and the previous token was not presented to that peer. Check what is serving ws://100.64.0.5:4433 before scanning."),
                1,
            ),
            // A failed reload and an unidentified peer are two facts.
            (
                TokenActivation::NotHerdr {
                    reason: "it answered nothing".to_string(),
                    reload: failed.clone(),
                },
                true,
                &behind_tls,
                format!("{rotated}. {failed_clause}, and something on ws://127.0.0.1:4433 completed a websocket handshake but did not answer as herdr: it answered nothing.\nwarning: nothing about the printed token was established, and the previous token was not presented to that peer. Check what is serving ws://127.0.0.1:4433 before scanning.{tls_note}"),
                1,
            ),
            (
                TokenActivation::Unverified {
                    reason: "it did not answer within 3000ms".to_string(),
                    reload: reloaded.clone(),
                },
                true,
                &local,
                format!("{rotated}. {reloaded_clause}, and whether ws://100.64.0.5:4433 accepts the new token could not be established: it did not answer within 3000ms.\nwarning: nothing has connected with the printed token. Restart the herdr server serving ws://100.64.0.5:4433, or connect with the token yourself, before relying on it."),
                1,
            ),
            (
                TokenActivation::Unverified {
                    reason: "it did not answer within 3000ms".to_string(),
                    reload: no_answer.clone(),
                },
                true,
                &behind_tls,
                format!("{rotated}. {no_answer_clause}, and whether ws://127.0.0.1:4433 accepts the new token could not be established: it did not answer within 3000ms.\nwarning: nothing has connected with the printed token. Restart the herdr server serving ws://127.0.0.1:4433, or connect with the token yourself, before relying on it.{tls_note}"),
                1,
            ),
        ];

        for (activation, previous_token_existed, scope, expected, expected_code) in rows {
            let (report, code) =
                activation_report(&activation, previous_token_existed, scope, config_path);
            assert_eq!(report, expected, "report for {activation:?} at {scope:?}");
            assert_eq!(code, expected_code, "exit code for {activation:?}");

            // The sentence that used to be printed without evidence appears
            // in exactly the outcome that has it: a handshake a herdr peer
            // accepted. No other state may borrow it.
            assert_eq!(
                report.contains("was accepted by a peer that answered as herdr"),
                matches!(activation, TokenActivation::LiveNow { .. }),
                "only an observed handshake may claim the token was accepted: {report}"
            );
            // Whenever the printed url was not the url dialed, every outcome
            // says so — an unexercised proxy path is a fact about the payload
            // in the operator's hand, not a detail of the happy path — and no
            // message may promise that payload works.
            assert_eq!(
                report.contains("was not dialed"),
                scope.unexercised_printed_url.is_some(),
                "the unexercised printed url must be named in every outcome: {report}"
            );
            if scope.unexercised_printed_url.is_some() {
                assert!(
                    !report.contains("the printed payload works"),
                    "a url that was never dialed may not be promised: {report}"
                );
            }
            // The reload attempt is stated in every outcome that carries one,
            // rather than being replaced by what the handshake found.
            if let Some(reload) = activation_reload(&activation) {
                assert!(
                    report.contains(&reload_clause(reload))
                        || matches!(activation, TokenActivation::LiveNow { .. }),
                    "the reload attempt must be stated alongside the proof: {report}"
                );
            }
        }
    }

    /// The reload an outcome carries, for the invariant above.
    fn activation_reload(activation: &TokenActivation) -> Option<&ConfigReload> {
        match activation {
            TokenActivation::LiveNow { reload, .. }
            | TokenActivation::NothingListening { reload }
            | TokenActivation::RefusedWithoutIdentifying { reload, .. }
            | TokenActivation::NotHerdr { reload, .. }
            | TokenActivation::Unverified { reload, .. } => Some(reload),
        }
    }
}
