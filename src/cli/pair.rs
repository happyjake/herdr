//! `herdr pair` — mint the WebSocket API bearer token and print the pairing
//! payload (endpoint URL + token + server name) as a terminal QR code and as
//! plaintext.
//!
//! The token is stored in `[websocket_api].token` in config.toml, so the
//! server honors it across restarts. Re-running the command mints a fresh
//! token and replaces the stored one; a running server is asked to reload its
//! config so the previous token stops authenticating immediately. When the
//! listener is not configured the command explains what to enable instead of
//! printing a payload that cannot work.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use base64::Engine;

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{EmptyParams, Method, Request};

/// 256 bits of OS randomness per token. Encoded as base64url without
/// padding (43 characters), which stays inside the URL-unreserved charset
/// the listener requires, so the token never needs escaping.
const TOKEN_BYTES: usize = 32;

const LISTENER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

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

    let previous_token_existed = config
        .websocket_api
        .token
        .as_deref()
        .is_some_and(|token| !token.is_empty());
    let activation = apply_token_to_running_server(local_listener);

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

    let (report, exit_code) =
        activation_report(&activation, previous_token_existed, local_listener);
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
    /// Base URL without a trailing slash, e.g. `ws://100.64.0.5:4433`.
    endpoint: String,
    token: String,
    name: String,
}

impl PairingPayload {
    fn url(&self) -> String {
        // The token charset is URL-unreserved by construction, so the query
        // form needs no percent-encoding; the free-form name does.
        format!(
            "{}/?token={}&name={}",
            self.endpoint,
            self.token,
            percent_encode_query_value(&self.name)
        )
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
                 Declare the url something else serves this listener at, e.g. \"wss://a-host.example.net\" — scheme, host, and optional port only.\n\
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
fn advertised_endpoint(advertised: Option<&str>) -> Result<Option<String>, PairingUnavailable> {
    let advertised = match advertised.map(str::trim) {
        None | Some("") => return Ok(None),
        Some(advertised) => advertised,
    };

    normalize_advertised_endpoint(advertised)
        .map(Some)
        .map_err(|reason| PairingUnavailable::InvalidAdvertisedEndpoint {
            advertised: advertised.to_string(),
            reason,
        })
}

/// Validate a declared endpoint and return its canonical form: a lowercase
/// `ws`/`wss` scheme, the host as configured, and the optional port.
///
/// This validator is deliberately a STRICT SUBSET of what a client can dial.
/// The invariant is one-directional: **every value it accepts must parse, in
/// a client, to exactly the host and port configured.** Refusing something a
/// client could have dialled is acceptable and must state the canonical form
/// to use instead. Accepting something a client rejects — or dials as
/// something else — is the only defect.
///
/// Judge every future question about this function by that invariant. It is
/// what lets the rules below stay small: they do not reimplement WHATWG URL
/// parsing (which no hand-written Rust can track, and no URL crate is worth
/// adding here), they only carve out a region where this function and a
/// client provably agree. Minting a payload costs a token rotation, so a
/// value we misjudge is a QR that fails after the credential has moved.
fn normalize_advertised_endpoint(advertised: &str) -> Result<String, String> {
    let (scheme, authority) = advertised
        .split_once("://")
        .ok_or_else(|| "expected a ws:// or wss:// url".to_string())?;

    let scheme = scheme.to_ascii_lowercase();
    if scheme != "ws" && scheme != "wss" {
        return Err(format!(
            "a websocket client cannot dial {scheme}://; use ws:// or wss://"
        ));
    }

    // One trailing slash is the same url; the query form appends its own.
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty() {
        return Err(no_host_remedy());
    }
    if authority.chars().any(char::is_whitespace) {
        return Err(format!(
            "a host cannot contain whitespace; declare it as one unbroken url, \
             e.g. {ENDPOINT_EXAMPLE}"
        ));
    }
    if authority.contains('@') {
        return Err(format!(
            "credentials do not belong in a pairing endpoint; \
             declare the host alone and let the token carry authorization, \
             e.g. {ENDPOINT_EXAMPLE}"
        ));
    }
    // A backslash separates path segments for ws/wss just as `/` does, so it
    // smuggles in exactly what the next check refuses.
    if authority.contains(['/', '?', '#', '\\']) {
        return Err(format!(
            "a path, query, or fragment would be lost when the token is appended; \
             declare scheme, host, and optional port only, e.g. {ENDPOINT_EXAMPLE}"
        ));
    }

    let (host, port) = split_host_and_port(authority)?;
    validate_host(host)?;
    if let Some(port) = port {
        validate_port(port)?;
    }

    Ok(format!("{scheme}://{authority}"))
}

/// The shape every structural refusal points back at. A refusal owes the
/// operator a form they can paste, so no message may end without one.
const ENDPOINT_EXAMPLE: &str = "wss://a-host.example.net:8443";

/// The remedy for "there is no host here", shared by the empty authority and
/// the empty bare host, which are the same mistake seen at two depths.
fn no_host_remedy() -> String {
    format!("no host to dial; declare scheme, host, and optional port, e.g. {ENDPOINT_EXAMPLE}")
}

/// One half of an authority's host: what shape a client will read it as.
enum HostForm<'a> {
    /// Bracketed, so a client parses the contents as an IPv6 address.
    Ipv6Literal(&'a str),
    /// Everything else: an IPv4 literal or a DNS name.
    Bare(&'a str),
}

/// Split an authority into host and optional port. IPv6 literals must be
/// bracketed, as URLs require: without brackets `fd7a::1` is indistinguishable
/// from a host with a port.
fn split_host_and_port(authority: &str) -> Result<(HostForm<'_>, Option<&str>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            "unterminated ipv6 literal; brackets must be closed, \
             e.g. wss://[fd7a::1]:8443"
                .to_string()
        })?;
        let port = match tail {
            "" => None,
            tail => Some(tail.strip_prefix(':').ok_or_else(|| {
                format!(
                    "expected a port after the ipv6 literal, found {tail:?}; \
                     write it as wss://[fd7a::1]:8443"
                )
            })?),
        };
        return Ok((HostForm::Ipv6Literal(host), port));
    }

    match authority.rsplit_once(':') {
        // More than one colon means an unbracketed ipv6 literal.
        Some((host, _)) if host.contains(':') => Err(
            "an ipv6 literal must be wrapped in brackets, e.g. wss://[fd7a::1]:8443".to_string(),
        ),
        Some((host, port)) => Ok((HostForm::Bare(host), Some(port))),
        None => Ok((HostForm::Bare(authority), None)),
    }
}

/// Accept only hosts a client's URL parser resolves the same way we do.
fn validate_host(host: HostForm<'_>) -> Result<(), String> {
    let host = match host {
        // Brackets promise an IPv6 address, so the contents must be one:
        // a client throws on anything else rather than treating it as a name.
        HostForm::Ipv6Literal(literal) => {
            return match literal.parse::<std::net::Ipv6Addr>() {
                Ok(_) => Ok(()),
                Err(_) => Err(format!(
                    "{literal:?} is bracketed but is not an ipv6 address, and brackets \
                     are only for ipv6 literals; write an address as wss://[fd7a::1]:8443 \
                     or a name without brackets as {ENDPOINT_EXAMPLE}"
                )),
            }
        }
        HostForm::Bare(host) => host,
    };

    if host.is_empty() {
        return Err(no_host_remedy());
    }

    // A client reads a host whose last label looks like a number — in any
    // base — as an IPv4 address rather than a name, and then either throws
    // or dials an address that shares no text with what was configured
    // ("0x7f000001" becomes 127.0.0.1 silently, "010.0.0.1" becomes 8.0.0.1).
    // Both break the invariant, so the whole number-shaped region is narrowed
    // to the one form this function and a client provably agree on.
    if last_label_looks_numeric(host) {
        return if is_canonical_dotted_quad(host) {
            Ok(())
        } else {
            Err(format!(
                "{host:?} is read as an ipv4 address, not a name, because it ends in a number, \
                 and a client would refuse it or dial a different address than it spells; \
                 write it as four plain decimal octets 0-255 with no leading zeros \
                 and no trailing dot, e.g. 100.64.0.5"
            ))
        };
    }

    // A trailing dot is the DNS root and names an absolute host; the empty
    // label it leaves behind is not a label to validate.
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.len() > 253 {
        return Err(format!(
            "a host name cannot exceed 253 characters; declare the name the proxy \
             actually serves, e.g. {ENDPOINT_EXAMPLE}"
        ));
    }

    let labels: Vec<&str> = name.split('.').collect();
    for label in &labels {
        if label.is_empty() {
            return Err(format!(
                "{host:?} has an empty label, and a host name cannot contain \"..\" \
                 or start with a dot; write the labels out, e.g. {ENDPOINT_EXAMPLE}"
            ));
        }
        if label.len() > 63 {
            return Err(format!(
                "{host:?} has a label longer than 63 characters, which no resolver \
                 accepts; keep each label at 63 or fewer, e.g. {ENDPOINT_EXAMPLE}"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "{host:?} has a label starting or ending with a hyphen, which is not \
                 a resolvable name; hyphens may only sit inside a label, \
                 e.g. {ENDPOINT_EXAMPLE}"
            ));
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "{host:?} is not an ipv4 literal, a bracketed ipv6 literal, or a host name; \
                 a name may hold only ascii letters, digits, hyphens, and underscores. \
                 A non-ascii name has to be declared in its punycode (xn--) form, \
                 which is what a client resolves it to anyway"
            ));
        }
    }

    Ok(())
}

/// Whether a client will read this host as an IPv4 number rather than a
/// name: its last label (a single trailing dot is the DNS root, not a label)
/// is decimal digits, hex with an `0x` prefix, or a leading-zero octal.
/// Deliberately a shape test, not a parse — the point is to spot the region
/// where a client switches interpretations, not to reimplement it.
fn last_label_looks_numeric(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host);
    let Some(last) = name.rsplit('.').next() else {
        return false;
    };
    if last.is_empty() {
        return false;
    }
    let hex_prefixed = last.starts_with("0x") || last.starts_with("0X");
    hex_prefixed || last.chars().all(|c| c.is_ascii_digit())
}

/// The one IPv4 spelling this function and a client agree on exactly: four
/// plain decimal octets, no leading zeros (which a client reads as octal),
/// no trailing dot (which a client drops from the host it dials).
fn is_canonical_dotted_quad(host: &str) -> bool {
    let mut octets = 0;
    for part in host.split('.') {
        octets += 1;
        if octets > 4 {
            return false;
        }
        let is_plain_decimal = !part.is_empty()
            && part.len() <= 3
            && part.chars().all(|c| c.is_ascii_digit())
            && (part.len() == 1 || !part.starts_with('0'));
        if !is_plain_decimal || part.parse::<u16>().is_ok_and(|octet| octet > 255) {
            return false;
        }
    }
    octets == 4
}

/// Accept only ports a client dials as the digits configured. `parse::<u16>`
/// is not that test on its own: it accepts a leading `+`, which a client
/// refuses outright, and a leading zero, which a client silently normalizes
/// away — so the payload text would stop matching the port dialled.
fn validate_port(port: &str) -> Result<(), String> {
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "{port:?} is not a port a client could dial; write plain digits, e.g. 8443"
        ));
    }
    if port.len() > 1 && port.starts_with('0') {
        // Only name the stripped value when it is itself dialable: "00"
        // strips to nothing and "099999" strips to a number this function
        // would refuse on the next line, and a remedy the operator cannot
        // use is the same as naming no remedy at all.
        if let Some(canonical) = port
            .trim_start_matches('0')
            .parse::<u16>()
            .ok()
            .filter(|canonical| *canonical != 0)
        {
            return Err(format!(
                "{port:?} has a leading zero, which a client strips before dialling; \
                 write the port plainly, e.g. {canonical}"
            ));
        }
        return Err(port_range_remedy(port));
    }
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(port_range_remedy(port)),
        Ok(_) => Ok(()),
    }
}

fn port_range_remedy(port: &str) -> String {
    format!("{port:?} is not a port a client could dial; use a port in 1-65535, e.g. 8443")
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

/// Whether and how the freshly stored token reached a live listener.
#[derive(Debug, PartialEq, Eq)]
enum TokenActivation {
    /// No herdr server is running; the stored token applies at next start.
    NoServer,
    /// The running server reloaded its config and the listener answers.
    LiveNow,
    /// The running server reloaded its config, but nothing is listening on
    /// the configured address — the bind was added after the server started.
    NeedsListenerRestart,
    /// A server is running but the reload failed; the previous token may
    /// still authenticate until a reload or restart succeeds.
    Failed(String),
}

fn apply_token_to_running_server(addr: SocketAddr) -> TokenActivation {
    let response = ApiClient::local().request_value(&Request {
        id: "cli:pair:reload-config".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    });

    match response {
        Ok(_) => {
            if listener_is_reachable(addr) {
                TokenActivation::LiveNow
            } else {
                TokenActivation::NeedsListenerRestart
            }
        }
        Err(ApiClientError::ErrorResponse(response)) => {
            TokenActivation::Failed(response.error.message)
        }
        Err(ApiClientError::Io(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            TokenActivation::NoServer
        }
        Err(err) => TokenActivation::Failed(err.to_string()),
    }
}

fn listener_is_reachable(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, LISTENER_PROBE_TIMEOUT).is_ok()
}

/// Human-readable outcome plus the process exit code. Exit is non-zero only
/// when a running server could not be updated — the one case where the
/// printed payload might not authenticate while the old token still does.
fn activation_report(
    activation: &TokenActivation,
    previous_token_existed: bool,
    addr: SocketAddr,
) -> (String, i32) {
    let rotation = if previous_token_existed {
        "Rotated: the previous token was replaced in config.toml"
    } else {
        "Stored the first token in config.toml"
    };

    match activation {
        TokenActivation::NoServer => (
            format!("{rotation}. No running herdr server was detected; the token takes effect when the server starts."),
            0,
        ),
        TokenActivation::LiveNow => {
            let invalidated = if previous_token_existed {
                " The previous token is no longer accepted."
            } else {
                ""
            };
            (
                format!("{rotation} and applied to the running server; the listener accepts the new token now.{invalidated}"),
                0,
            )
        }
        TokenActivation::NeedsListenerRestart => (
            format!(
                "{rotation} and the running server reloaded its config, but nothing is listening on {addr} yet.\n\
                 Restart the herdr server so it binds the websocket listener, then scan."
            ),
            0,
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
    eprintln!("clients should dial, e.g. \"wss://a-host.example.net\"; the payload");
    eprintln!("names that instead of the bind address. Unset pairs against bind.");
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
            listener_is_reachable(dial_target),
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

    /// Half of the invariant: an accepted value is carried into the payload
    /// as exactly the host and port configured, because that is the only
    /// promise this function makes to whatever client parses it back out.
    #[test]
    fn an_advertised_endpoint_keeps_the_configured_scheme_host_and_port() {
        for (configured, expected) in [
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
        ] {
            assert_eq!(
                declared(configured),
                Ok(expected.to_string()),
                "{configured:?}"
            );
        }
    }

    /// The other half: this validator is a strict subset of what a client can
    /// dial, so a refusal is allowed to be stricter than a client — but it
    /// owes the operator the canonical form to write instead. Every refusal
    /// below is checked for both the reason and that remedy.
    #[test]
    fn a_malformed_advertised_endpoint_refuses_to_print_a_payload() {
        for (configured, expected_reason, expected_remedy) in [
            // No scheme at all: nothing says how to dial it.
            ("a-host.example.ts.net", "ws:// or wss://", "wss://"),
            // A scheme no websocket client can dial.
            ("https://a-host.example.ts.net", "cannot dial", "wss://"),
            // Scheme but no host.
            ("wss://", "no host", "wss://a-host.example.net:8443"),
            // Anything past the authority would be dropped or mangled.
            (
                "wss://a-host.example.ts.net/api",
                "path",
                "host, and optional port only",
            ),
            (
                "wss://a-host.example.ts.net/?token=x",
                "path",
                "host, and optional port only",
            ),
            // A backslash is a path separator for ws/wss, so this is the
            // same hazard as "/api" wearing a different costume: a client
            // reads the host as "a-host" and the rest as a path.
            ("wss://a-host\\api", "path", "host, and optional port only"),
            (
                "wss://user@a-host.example.ts.net",
                "credentials",
                "wss://a-host.example.net:8443",
            ),
            (
                "wss://a host.example.ts.net",
                "whitespace",
                "wss://a-host.example.net:8443",
            ),
            // Ports must be dialled as the digits written.
            ("wss://a-host.example.ts.net:0", "1-65535", "e.g. 8443"),
            ("wss://a-host.example.ts.net:99999", "1-65535", "e.g. 8443"),
            ("wss://a-host.example.ts.net:https", "plain digits", "8443"),
            // A signed port parses in rust but throws in a client.
            ("wss://a-host.example.ts.net:+443", "plain digits", "8443"),
            // A leading-zero port is stripped by a client, so the payload
            // text would stop naming the port actually dialled.
            ("wss://a-host.example.ts.net:0443", "leading zero", "443"),
            // An all-zero port strips to nothing and a leading-zero port
            // over the range strips to a value this function would refuse
            // anyway, so neither may be echoed back as the remedy.
            ("wss://a-host.example.ts.net:00", "1-65535", "e.g. 8443"),
            ("wss://a-host.example.ts.net:099999", "1-65535", "e.g. 8443"),
            // An unbracketed ipv6 literal is ambiguous with host:port.
            ("wss://fd7a::1", "brackets", "[fd7a::1]:8443"),
            ("wss://[fd7a::1", "brackets", "wss://[fd7a::1]:8443"),
            // Brackets promise an ipv6 address; a client throws when the
            // contents are not one, rather than reading them as a name.
            (
                "wss://[not-an-ip]",
                "not an ipv6 address",
                "without brackets",
            ),
            (
                "wss://[not-an-ip]:8443",
                "not an ipv6 address",
                "wss://[fd7a::1]:8443",
            ),
            ("wss://[]", "not an ipv6 address", "wss://[fd7a::1]:8443"),
            // Number-shaped hosts: a client either throws (proxy.0x10,
            // 10.0.0.999, 1.2.3.4.5) or dials an address that shares no text
            // with what was written (0x7f000001 and 0177.0.0.1 become
            // 127.0.0.1, 010.0.0.1 becomes 8.0.0.1, 127.1 becomes 127.0.0.1,
            // and the trailing dot of 192.0.2.1. is dropped). All refused by
            // one rule, all pointed at the dotted quad.
            ("wss://0x7f000001", "ipv4", "100.64.0.5"),
            ("wss://proxy.0x10", "ipv4", "100.64.0.5"),
            ("wss://127.1", "ipv4", "100.64.0.5"),
            ("wss://192.0.2.1.", "ipv4", "100.64.0.5"),
            ("wss://010.0.0.1", "ipv4", "no leading zeros"),
            ("wss://10.0.0.05", "ipv4", "no leading zeros"),
            ("wss://0177.0.0.1", "ipv4", "no leading zeros"),
            ("wss://10.0.0.999", "ipv4", "0-255"),
            ("wss://1.2.3.4.5", "ipv4", "100.64.0.5"),
            // Empty labels are not names.
            (
                "wss://a-host..example.net",
                "empty label",
                "wss://a-host.example.net:8443",
            ),
            (
                "wss://.example.net",
                "empty label",
                "wss://a-host.example.net:8443",
            ),
            // Hyphen-edged labels are not resolvable names.
            (
                "wss://-a-host.example.net",
                "hyphen",
                "only sit inside a label",
            ),
            (
                "wss://a-host-.example.net",
                "hyphen",
                "wss://a-host.example.net:8443",
            ),
            // Non-ascii is refused rather than converted: applying idna by
            // hand is the class this validator refuses to enter, so the
            // refusal owes the operator the punycode form instead.
            ("wss://naïve.example.net", "ascii", "punycode (xn--)"),
        ] {
            // `contains("")` is true of every string, so an empty expectation
            // would assert nothing while reading like a check. Fail the row
            // outright rather than let it pass vacuously.
            assert!(
                !expected_reason.is_empty() && !expected_remedy.is_empty(),
                "{configured:?}: both expectations must be substantive; \
                 an empty one asserts nothing"
            );

            let reason = declared(configured)
                .expect_err(&format!("{configured:?} must be refused, not encoded"));
            assert!(
                reason.contains(expected_reason),
                "{configured:?}: expected {expected_reason:?} in {reason:?}"
            );
            assert!(
                reason.contains(expected_remedy),
                "{configured:?}: refusal must name the canonical form \
                 {expected_remedy:?}, got {reason:?}"
            );
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

    #[test]
    fn activation_report_spells_out_each_outcome() {
        let endpoint = addr("100.64.0.5:4433");

        let (no_server, code) = activation_report(&TokenActivation::NoServer, true, endpoint);
        assert_eq!(code, 0);
        assert!(no_server.contains("No running herdr server"));

        let (live, code) = activation_report(&TokenActivation::LiveNow, true, endpoint);
        assert_eq!(code, 0);
        assert!(live.contains("previous token is no longer accepted"));

        let (first_pairing, code) = activation_report(&TokenActivation::LiveNow, false, endpoint);
        assert_eq!(code, 0);
        assert!(first_pairing.contains("first token"));
        assert!(!first_pairing.contains("previous token"));

        let (unbound, code) =
            activation_report(&TokenActivation::NeedsListenerRestart, true, endpoint);
        assert_eq!(code, 0);
        assert!(unbound.contains("Restart the herdr server"));
        assert!(unbound.contains("100.64.0.5:4433"));

        let (failed, code) =
            activation_report(&TokenActivation::Failed("boom".to_string()), true, endpoint);
        assert_eq!(code, 1);
        assert!(failed.contains("boom"));
        assert!(failed.contains("previous token may still be accepted"));
    }
}
