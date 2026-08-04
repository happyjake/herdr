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

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::http::StatusCode;

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget};
use crate::api::schema::{EmptyParams, Method, Request};

/// 256 bits of OS randomness per token. Encoded as base64url without
/// padding (43 characters), which stays inside the URL-unreserved charset
/// the listener requires, so the token never needs escaping.
const TOKEN_BYTES: usize = 32;

const LISTENER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the token handshake may take once the listener has accepted the
/// connection. The address dialed is this machine's own listener, so a slow
/// answer says something other than a herdr listener holds the port rather
/// than that the network is busy.
const TOKEN_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

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
    let activation =
        apply_token_to_running_server(&config_path, local_listener, &token, previous_token);

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
        local_listener,
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

/// The control sockets pairing will ask to reload, in the order it tries
/// them. At most two: a config file selected explicitly has a server whose
/// home directory is that config's own, and there is also whatever server
/// this command's own environment addresses.
#[derive(Debug, PartialEq, Eq)]
struct ControlSockets {
    /// The socket of a server living in the config file's own directory.
    owning: Option<PathBuf>,
    /// The socket a server started in this command's environment serves.
    ambient: PathBuf,
}

impl ControlSockets {
    fn in_order(&self) -> Vec<&PathBuf> {
        self.owning
            .iter()
            .chain(std::iter::once(&self.ambient))
            .collect()
    }
}

/// Resolve the control sockets to try for the server that reads
/// `config_path`.
///
/// This is a preference order, not a lookup: no config file records which
/// server reads it, and no server directory records which config it read.
///
/// `ambient` is the socket a server started in this command's own environment
/// serves, and it is the right one whenever the config being paired is that
/// environment's own config — the ordinary one-config machine, and a named
/// session, which shares that config file. When the config file was selected
/// explicitly and is some other file, the ambient socket belongs to a server
/// that need never have read it: a machine can run one server per config, and
/// dialing the ambient socket alone is how pairing came to write a token into
/// one config and reload a server holding another. The server whose home
/// directory is the config's own is tried first there.
///
/// Nothing here is allowed to stand in for proof. Whichever socket answers,
/// only presenting the token to the listener decides what gets reported.
fn control_sockets_for_config(
    config_path: &Path,
    environment_config_path: &Path,
    ambient: PathBuf,
) -> ControlSockets {
    let owning = if config_path == environment_config_path {
        None
    } else {
        config_path
            .parent()
            .map(crate::session::api_socket_path_in)
            .filter(|owning| *owning != ambient)
    };
    ControlSockets { owning, ambient }
}

/// Which server was asked to reload the config, and what came back.
#[derive(Debug, PartialEq, Eq)]
enum ConfigReload {
    /// A server answered on this control socket and reloaded its config.
    Reloaded { socket: PathBuf },
    /// No server answered any control socket that was tried.
    NoServer { socket: PathBuf },
    /// A server answered and the reload failed.
    Failed(String),
}

/// Ask the first server that answers to reload its config. A socket nothing
/// is listening on is not a failure — it only means that server is not
/// running — so the next candidate is tried.
fn reload_config_of_owning_server(sockets: &ControlSockets) -> ConfigReload {
    let mut unanswered: Option<PathBuf> = None;
    for socket in sockets.in_order() {
        match reload_config_at(socket) {
            ConfigReload::NoServer { socket } => unanswered = unanswered.or(Some(socket)),
            answered => return answered,
        }
    }
    ConfigReload::NoServer {
        socket: unanswered.unwrap_or_else(|| sockets.ambient.clone()),
    }
}

fn reload_config_at(socket: &Path) -> ConfigReload {
    let response = ApiClient::for_target(ConnectionTarget::SocketPath(socket.to_path_buf()))
        .request_value(&Request {
            id: "cli:pair:reload-config".into(),
            method: Method::ServerReloadConfig(EmptyParams::default()),
        });

    match response {
        Ok(_) => ConfigReload::Reloaded {
            socket: socket.to_path_buf(),
        },
        Err(ApiClientError::ErrorResponse(response)) => {
            ConfigReload::Failed(response.error.message)
        }
        Err(ApiClientError::Io(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            ConfigReload::NoServer {
                socket: socket.to_path_buf(),
            }
        }
        Err(err) => ConfigReload::Failed(err.to_string()),
    }
}

/// What presenting a token to the listener established. Every variant is an
/// observation: the handshake either completed, was refused, found nothing to
/// talk to, or ended without answering the question.
#[derive(Debug, PartialEq, Eq)]
enum TokenProof {
    /// The listener completed the handshake for this token.
    Accepted,
    /// The listener answered the handshake by refusing the token.
    Refused,
    /// Nothing accepted a connection on the address.
    NoListener,
    /// Something answered, but neither accepted nor refused the token.
    Inconclusive(String),
}

/// Open a connection to the local listener, or say what its absence proves.
/// A refused connection and a connect timeout are the two shapes of "nothing
/// is serving this address"; any other error is a fact about this machine
/// rather than about the listener, so it proves nothing either way.
fn connect_to_listener(addr: SocketAddr) -> Result<std::net::TcpStream, TokenProof> {
    match std::net::TcpStream::connect_timeout(&addr, LISTENER_PROBE_TIMEOUT) {
        Ok(stream) => Ok(stream),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ) =>
        {
            Err(TokenProof::NoListener)
        }
        Err(err) => Err(TokenProof::Inconclusive(format!(
            "could not connect to {addr}: {err}"
        ))),
    }
}

/// Present `token` to the listener the way the printed payload does — as the
/// `token` query parameter — and report what the handshake proved. Using the
/// credential is the only check that distinguishes a listener holding this
/// token from a listener holding some other one, which is the whole
/// difference between a payload that works and a payload that does not.
fn probe_token(addr: SocketAddr, token: &str) -> TokenProof {
    let stream = match connect_to_listener(addr) {
        Ok(stream) => stream,
        Err(proof) => return proof,
    };
    if let Err(err) = stream
        .set_read_timeout(Some(TOKEN_HANDSHAKE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TOKEN_HANDSHAKE_TIMEOUT)))
    {
        return TokenProof::Inconclusive(format!("could not bound the handshake: {err}"));
    }

    // The token charset is URL-unreserved by construction, so the query form
    // needs no escaping here either.
    let request = match format!("ws://{addr}/?token={token}").into_client_request() {
        Ok(request) => request,
        Err(err) => {
            return TokenProof::Inconclusive(format!(
                "could not build a handshake for {addr}: {err}"
            ));
        }
    };

    match tungstenite::client::client(request, stream) {
        Ok((mut websocket, _response)) => {
            // Nothing is sent on the connection: the handshake is the whole
            // proof, and closing it looks to the server like any other client
            // that connected and left.
            let _ = websocket.close(None);
            let _ = websocket.flush();
            TokenProof::Accepted
        }
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                TokenProof::Refused
            } else {
                TokenProof::Inconclusive(format!(
                    "the listener answered the handshake with http {}",
                    response.status()
                ))
            }
        }
        Err(err) => TokenProof::Inconclusive(format!("the handshake did not complete: {err}")),
    }
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

/// Whether and how the freshly stored token reached a live listener. Every
/// variant names something this command observed — a control socket that
/// answered or did not, and a handshake that was accepted, refused, or never
/// answered the question.
#[derive(Debug, PartialEq, Eq)]
enum TokenActivation {
    /// A handshake presenting the new token was accepted. `previous_token`
    /// carries what presenting the previous one proved.
    LiveNow { previous_token: PreviousTokenProof },
    /// No server answered a control socket and nothing is listening on the
    /// bind address; the stored token applies at next start.
    NoServer { socket: PathBuf },
    /// A server reloaded its config, but nothing is listening on the
    /// configured address — the bind was added after the server started.
    NeedsListenerRestart,
    /// A listener answers on the bind address and refused the new token, so
    /// whatever serves that address is not running the config just written.
    NotAccepted { reload: ConfigReload },
    /// A listener answered but neither accepted nor refused the token.
    Unverified { reason: String },
    /// A server answered and the reload failed; the previous token may still
    /// authenticate until a reload or restart succeeds.
    Failed(String),
}

fn apply_token_to_running_server(
    config_path: &Path,
    addr: SocketAddr,
    token: &str,
    previous_token: Option<&str>,
) -> TokenActivation {
    let sockets = control_sockets_for_config(
        config_path,
        &crate::config::config_dir().join("config.toml"),
        crate::api::socket_path(),
    );
    let reload = reload_config_of_owning_server(&sockets);
    if let ConfigReload::Failed(reason) = reload {
        return TokenActivation::Failed(reason);
    }

    let new_token = probe_token(addr, token);
    // The claim that the previous token stopped working is a second fact, so
    // it takes a second handshake; it is only worth asking once the listener
    // has been shown to be the one holding the new token.
    let previous_token = match (&new_token, previous_token) {
        (TokenProof::Accepted, Some(previous)) => Some(probe_token(addr, previous)),
        _ => None,
    };

    decide_activation(reload, new_token, previous_token)
}

/// Turn the reload outcome and the handshakes into the one state they
/// support. Nothing is inferred here that was not observed: a reload that
/// succeeded never implies the token is live, and a listener that answers
/// never implies it accepts anything.
fn decide_activation(
    reload: ConfigReload,
    new_token: TokenProof,
    previous_token: Option<TokenProof>,
) -> TokenActivation {
    match (reload, new_token) {
        (ConfigReload::Failed(reason), _) => TokenActivation::Failed(reason),
        (_, TokenProof::Accepted) => TokenActivation::LiveNow {
            previous_token: match previous_token {
                Some(TokenProof::Refused) => PreviousTokenProof::Refused,
                Some(TokenProof::Accepted) => PreviousTokenProof::StillAccepted,
                _ => PreviousTokenProof::Unproven,
            },
        },
        (ConfigReload::Reloaded { .. }, TokenProof::NoListener) => {
            TokenActivation::NeedsListenerRestart
        }
        (ConfigReload::NoServer { socket }, TokenProof::NoListener) => {
            TokenActivation::NoServer { socket }
        }
        (reload, TokenProof::Refused) => TokenActivation::NotAccepted { reload },
        (_, TokenProof::Inconclusive(reason)) => TokenActivation::Unverified { reason },
    }
}

/// Human-readable outcome plus the process exit code.
///
/// Exit is non-zero when the printed payload was observed not to work, and
/// when it could not be shown to work while something answers on the bind
/// address. A state that is understood and has a next step — no server yet,
/// no listener yet — exits zero; an unknown one does not, because a payload
/// nobody has connected with is exactly what this command used to report as
/// live.
fn activation_report(
    activation: &TokenActivation,
    previous_token_existed: bool,
    addr: SocketAddr,
    config_path: &Path,
) -> (String, i32) {
    let rotation = if previous_token_existed {
        "Rotated: the previous token was replaced in config.toml"
    } else {
        "Stored the first token in config.toml"
    };
    let accepted = format!("a handshake on {addr} presenting the new token was accepted");

    match activation {
        TokenActivation::NoServer { socket } => (
            format!(
                "{rotation}. No running herdr server answered the control socket at {}, \
                 and nothing is listening on {addr}; the token takes effect when the server starts.",
                socket.display()
            ),
            0,
        ),
        TokenActivation::LiveNow { previous_token } => match previous_token {
            PreviousTokenProof::Refused => (
                format!(
                    "{rotation} and applied to the running server: {accepted}, \
                     and one presenting the previous token was refused."
                ),
                0,
            ),
            PreviousTokenProof::StillAccepted => (
                format!(
                    "{rotation} and applied to the running server: {accepted}, \
                     but one presenting the previous token was accepted too.\n\
                     warning: the previous token has not stopped authenticating. \
                     Restart the herdr server serving {addr} before treating it as revoked."
                ),
                1,
            ),
            PreviousTokenProof::Unproven if previous_token_existed => (
                format!(
                    "{rotation} and applied to the running server: {accepted}.\n\
                     Whether the previous token still authenticates was not established."
                ),
                0,
            ),
            PreviousTokenProof::Unproven => (
                format!("{rotation} and applied to the running server: {accepted}."),
                0,
            ),
        },
        TokenActivation::NeedsListenerRestart => (
            format!(
                "{rotation} and the running server reloaded its config, but nothing is listening on {addr} yet.\n\
                 Restart the herdr server so it binds the websocket listener, then scan."
            ),
            0,
        ),
        TokenActivation::NotAccepted { reload } => {
            let observed = match reload {
                ConfigReload::Reloaded { socket } => format!(
                    "{rotation}, and the server at {} reloaded its config, but a handshake on {addr} \
                     presenting the new token was refused.",
                    socket.display()
                ),
                _ => format!(
                    "{rotation}, but no herdr server answered a control socket, and a handshake on \
                     {addr} presenting the new token was refused."
                ),
            };
            (
                format!(
                    "{observed}\n\
                     warning: the printed token does not authenticate. Whatever serves {addr} has not \
                     applied {}: it is a different server, or it did not reload. Find it with \
                     `herdr session list --json`, then reload it with \
                     `HERDR_SOCKET_PATH=<its socket_path> herdr server reload-config` or restart it; \
                     the printed payload works once it does.",
                    config_path.display()
                ),
                1,
            )
        }
        TokenActivation::Unverified { reason } => (
            format!(
                "{rotation}, but whether the listener on {addr} accepts the new token could not be \
                 established: {reason}.\n\
                 warning: nothing has connected with the printed token. Restart the herdr server \
                 serving {addr}, or connect with the token yourself, before relying on it."
            ),
            1,
        ),
        TokenActivation::Failed(reason) => (
            format!(
                "{rotation}, but applying it to the running server failed: {reason}.\n\
                 warning: the previous token may still be accepted until `herdr server reload-config` succeeds or the server restarts."
            ),
            1,
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
        assert!(
            connect_to_listener(dial_target).is_ok(),
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

    /// The control socket is resolved from the config file being written,
    /// because on a machine running one server per config the socket this
    /// command's own environment names need never belong to a server that
    /// ever read that file.
    #[test]
    fn the_control_socket_follows_the_config_being_written() {
        let ambient = PathBuf::from("/home/u/.config/herdr/herdr.sock");
        let environment_config = PathBuf::from("/home/u/.config/herdr/config.toml");

        // The ordinary one-config machine: the config being paired is this
        // environment's own, so the socket it names is a server that reads it.
        assert_eq!(
            control_sockets_for_config(&environment_config, &environment_config, ambient.clone()),
            ControlSockets {
                owning: None,
                ambient: ambient.clone(),
            }
        );

        // A named session serves its own socket and shares the same config
        // file, so that socket stays the one to reload.
        let session = PathBuf::from("/home/u/.config/herdr/sessions/mobile/herdr.sock");
        assert_eq!(
            control_sockets_for_config(&environment_config, &environment_config, session.clone()),
            ControlSockets {
                owning: None,
                ambient: session,
            }
        );

        // A config file selected explicitly: the server whose home directory
        // is that config's own comes first, and the ambient socket — which
        // may be a server holding an entirely different config — only after.
        let selected = PathBuf::from("/home/u/.config/herdr-second/config.toml");
        let owning = PathBuf::from("/home/u/.config/herdr-second/herdr.sock");
        let sockets = control_sockets_for_config(&selected, &environment_config, ambient.clone());
        assert_eq!(
            sockets,
            ControlSockets {
                owning: Some(owning.clone()),
                ambient: ambient.clone(),
            }
        );
        assert_eq!(sockets.in_order(), vec![&owning, &ambient]);

        // A different file in the same directory is the same server; nothing
        // is dialed twice.
        let sibling = PathBuf::from("/home/u/.config/herdr/other.toml");
        assert_eq!(
            control_sockets_for_config(&sibling, &environment_config, ambient.clone()),
            ControlSockets {
                owning: None,
                ambient,
            }
        );
    }

    /// The defect this command carried: a reload that succeeded and an
    /// address that answers say nothing about which token the listener
    /// holds. Only the handshake decides.
    #[test]
    fn only_an_accepted_handshake_is_reported_as_live() {
        let socket = PathBuf::from("/home/u/.config/herdr/herdr.sock");
        let reloaded = || ConfigReload::Reloaded {
            socket: socket.clone(),
        };
        let no_server = || ConfigReload::NoServer {
            socket: socket.clone(),
        };

        // A reloaded server plus a refused handshake is the shape that used
        // to print "the listener accepts the new token now".
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Refused, None),
            TokenActivation::NotAccepted { reload: reloaded() }
        );
        assert_eq!(
            decide_activation(no_server(), TokenProof::Refused, None),
            TokenActivation::NotAccepted {
                reload: no_server()
            }
        );

        // Live means observed live, and the previous token's fate is a
        // separate observation rather than something the first one implies.
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Accepted, Some(TokenProof::Refused)),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Refused
            }
        );
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Accepted, Some(TokenProof::Accepted)),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::StillAccepted
            }
        );
        assert_eq!(
            decide_activation(reloaded(), TokenProof::Accepted, None),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Unproven
            }
        );
        assert_eq!(
            decide_activation(
                reloaded(),
                TokenProof::Accepted,
                Some(TokenProof::Inconclusive("eof".to_string()))
            ),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Unproven
            }
        );
        // A listener holding the new token is live whether or not this
        // command is the one that reached the server holding it.
        assert_eq!(
            decide_activation(no_server(), TokenProof::Accepted, Some(TokenProof::Refused)),
            TokenActivation::LiveNow {
                previous_token: PreviousTokenProof::Refused
            }
        );

        assert_eq!(
            decide_activation(reloaded(), TokenProof::NoListener, None),
            TokenActivation::NeedsListenerRestart
        );
        assert_eq!(
            decide_activation(no_server(), TokenProof::NoListener, None),
            TokenActivation::NoServer {
                socket: socket.clone()
            }
        );
        assert_eq!(
            decide_activation(
                reloaded(),
                TokenProof::Inconclusive("http 502".to_string()),
                None
            ),
            TokenActivation::Unverified {
                reason: "http 502".to_string()
            }
        );
        // A failed reload is reported as failed whatever the listener says.
        assert_eq!(
            decide_activation(
                ConfigReload::Failed("boom".to_string()),
                TokenProof::Accepted,
                None
            ),
            TokenActivation::Failed("boom".to_string())
        );
    }

    /// A real listener, because the proof this command now rests on is a
    /// real handshake: the same address answers, and the only thing that
    /// tells the freshly minted token from the one it replaced is presenting
    /// them.
    #[test]
    fn a_handshake_tells_the_new_token_from_the_previous_one() {
        let listener = start_listener_holding("the-new-token");
        let endpoint = listener.handle.local_addr();

        assert_eq!(probe_token(endpoint, "the-new-token"), TokenProof::Accepted);
        assert_eq!(
            probe_token(endpoint, "the-previous-token"),
            TokenProof::Refused
        );

        drop(listener);
        assert_eq!(
            probe_token(endpoint, "the-new-token"),
            TokenProof::NoListener,
            "a released port must not read as a listener"
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

        match probe_token(endpoint, "a-token") {
            TokenProof::Inconclusive(reason) => assert!(reason.contains("handshake"), "{reason}"),
            other => panic!("expected an inconclusive proof, got {other:?}"),
        }

        accepter.join().expect("accepter thread");
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
        )
        .expect("the listener starts")
        .expect("bind is configured, so it binds");
        TestListener {
            handle,
            _api_rx: api_rx,
        }
    }

    /// Every outcome, whole-message. The wording is the deliverable here:
    /// an operator who reads "applied and live" and finds it false stops
    /// believing anything else this command prints, so each message is
    /// asserted by equality rather than by substring, and each one names
    /// only what was observed. A row per outcome means none of them can be
    /// deleted or softened without failing.
    #[test]
    fn activation_report_spells_out_each_outcome() {
        let endpoint = addr("100.64.0.5:4433");
        let config_path = Path::new("/home/u/.config/herdr/config.toml");
        let socket = PathBuf::from("/home/u/.config/herdr/herdr.sock");
        let rows = vec![
            (
                TokenActivation::NoServer {
                    socket: socket.clone(),
                },
                true,
                "Rotated: the previous token was replaced in config.toml. No running herdr server answered the control socket at /home/u/.config/herdr/herdr.sock, and nothing is listening on 100.64.0.5:4433; the token takes effect when the server starts.",
                0,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Refused,
                },
                true,
                "Rotated: the previous token was replaced in config.toml and applied to the running server: a handshake on 100.64.0.5:4433 presenting the new token was accepted, and one presenting the previous token was refused.",
                0,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Unproven,
                },
                true,
                "Rotated: the previous token was replaced in config.toml and applied to the running server: a handshake on 100.64.0.5:4433 presenting the new token was accepted.\nWhether the previous token still authenticates was not established.",
                0,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::StillAccepted,
                },
                true,
                "Rotated: the previous token was replaced in config.toml and applied to the running server: a handshake on 100.64.0.5:4433 presenting the new token was accepted, but one presenting the previous token was accepted too.\nwarning: the previous token has not stopped authenticating. Restart the herdr server serving 100.64.0.5:4433 before treating it as revoked.",
                1,
            ),
            (
                TokenActivation::LiveNow {
                    previous_token: PreviousTokenProof::Unproven,
                },
                false,
                "Stored the first token in config.toml and applied to the running server: a handshake on 100.64.0.5:4433 presenting the new token was accepted.",
                0,
            ),
            (
                TokenActivation::NeedsListenerRestart,
                true,
                "Rotated: the previous token was replaced in config.toml and the running server reloaded its config, but nothing is listening on 100.64.0.5:4433 yet.\nRestart the herdr server so it binds the websocket listener, then scan.",
                0,
            ),
            (
                TokenActivation::NotAccepted {
                    reload: ConfigReload::Reloaded {
                        socket: socket.clone(),
                    },
                },
                true,
                "Rotated: the previous token was replaced in config.toml, and the server at /home/u/.config/herdr/herdr.sock reloaded its config, but a handshake on 100.64.0.5:4433 presenting the new token was refused.\nwarning: the printed token does not authenticate. Whatever serves 100.64.0.5:4433 has not applied /home/u/.config/herdr/config.toml: it is a different server, or it did not reload. Find it with `herdr session list --json`, then reload it with `HERDR_SOCKET_PATH=<its socket_path> herdr server reload-config` or restart it; the printed payload works once it does.",
                1,
            ),
            (
                TokenActivation::NotAccepted {
                    reload: ConfigReload::NoServer { socket },
                },
                true,
                "Rotated: the previous token was replaced in config.toml, but no herdr server answered a control socket, and a handshake on 100.64.0.5:4433 presenting the new token was refused.\nwarning: the printed token does not authenticate. Whatever serves 100.64.0.5:4433 has not applied /home/u/.config/herdr/config.toml: it is a different server, or it did not reload. Find it with `herdr session list --json`, then reload it with `HERDR_SOCKET_PATH=<its socket_path> herdr server reload-config` or restart it; the printed payload works once it does.",
                1,
            ),
            (
                TokenActivation::Unverified {
                    reason: "the handshake did not complete: eof".to_string(),
                },
                true,
                "Rotated: the previous token was replaced in config.toml, but whether the listener on 100.64.0.5:4433 accepts the new token could not be established: the handshake did not complete: eof.\nwarning: nothing has connected with the printed token. Restart the herdr server serving 100.64.0.5:4433, or connect with the token yourself, before relying on it.",
                1,
            ),
            (
                TokenActivation::Failed("boom".to_string()),
                true,
                "Rotated: the previous token was replaced in config.toml, but applying it to the running server failed: boom.\nwarning: the previous token may still be accepted until `herdr server reload-config` succeeds or the server restarts.",
                1,
            ),
        ];

        for (activation, previous_token_existed, expected, expected_code) in rows {
            let (report, code) =
                activation_report(&activation, previous_token_existed, endpoint, config_path);
            assert_eq!(report, expected, "report for {activation:?}");
            assert_eq!(code, expected_code, "exit code for {activation:?}");

            // The sentence that used to be printed without evidence appears
            // in exactly the outcome that has it: a handshake that was
            // accepted. No other state may borrow it.
            assert_eq!(
                report.contains("presenting the new token was accepted"),
                matches!(activation, TokenActivation::LiveNow { .. }),
                "only an observed handshake may claim the token was accepted: {report}"
            );
        }
    }
}
