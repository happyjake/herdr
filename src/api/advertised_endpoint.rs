//! The url a herdr server declares clients should dial.
//!
//! The bind address describes the local socket, which is the reachable url
//! only when nothing fronts the listener. `websocket_api.advertised_endpoint`
//! is what an operator declares when something does — a TLS terminating proxy,
//! for instance.
//!
//! One declaration serves two surfaces: the pairing payload `herdr pair`
//! prints, and the `ping` pong, which is how an already-paired client learns
//! the endpoint without scanning anything again. Both read it through this
//! module, so a value either reaches both or neither: the pong never
//! publishes a url `herdr pair` would refuse to encode.

use std::sync::{Arc, PoisonError, RwLock};

use tracing::warn;

use crate::config::WebSocketApiConfig;

/// The declared value with config noise removed, or `None` when nothing is
/// declared. Unset, empty, and whitespace-only all mean undeclared, as they
/// do for `name` and `reach`.
pub(crate) fn declared_advertised_endpoint(configured: Option<&str>) -> Option<&str> {
    match configured.map(str::trim) {
        None | Some("") => None,
        Some(declared) => Some(declared),
    }
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
pub(crate) fn normalize_advertised_endpoint(advertised: &str) -> Result<String, String> {
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

/// What the pong publishes for a loaded config section: the canonical form of
/// a declared endpoint, or nothing.
///
/// A declaration a client could not dial publishes nothing rather than
/// something broken — the same verdict `herdr pair` reaches, which refuses to
/// encode it. Publishing it anyway would hand clients a url that fails at
/// dial time, long after the config that caused it was written.
fn resolve_advertised_endpoint(config: &WebSocketApiConfig) -> Option<String> {
    let declared = declared_advertised_endpoint(config.advertised_endpoint.as_deref())?;
    match normalize_advertised_endpoint(declared) {
        Ok(endpoint) => Some(endpoint),
        Err(reason) => {
            warn!(
                advertised = %declared,
                reason = %reason,
                "websocket_api.advertised_endpoint is not a url a client could dial; declaring no endpoint"
            );
            None
        }
    }
}

/// The server's current advertised endpoint, shared by both API transports.
/// Every `ping` reads the current value, so an endpoint declared or corrected
/// in the config reaches clients on the next pong after a config reload,
/// without restarting a listener.
#[derive(Debug, Clone)]
pub struct SharedAdvertisedEndpoint {
    endpoint: Arc<RwLock<Option<String>>>,
}

impl SharedAdvertisedEndpoint {
    pub fn from_config(config: &WebSocketApiConfig) -> Self {
        Self {
            endpoint: Arc::new(RwLock::new(resolve_advertised_endpoint(config))),
        }
    }

    pub(crate) fn current(&self) -> Option<String> {
        self.endpoint
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Apply a reloaded `[websocket_api]` section. An absent, empty, or
    /// unusable value clears the declaration. Returns whether the endpoint
    /// changed.
    pub(crate) fn apply_reloaded_config(&self, config: &WebSocketApiConfig) -> bool {
        let endpoint = resolve_advertised_endpoint(config);
        let mut current = self
            .endpoint
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if *current == endpoint {
            return false;
        }
        *current = endpoint;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(advertised: Option<&str>) -> WebSocketApiConfig {
        WebSocketApiConfig {
            advertised_endpoint: advertised.map(str::to_string),
            ..WebSocketApiConfig::default()
        }
    }

    #[test]
    fn an_undeclared_endpoint_publishes_nothing() {
        // Absent, empty, and whitespace-only are the same statement: this
        // server declares no endpoint, so the pong carries no field rather
        // than an empty or invented one.
        for undeclared in [None, Some(""), Some("   ")] {
            assert_eq!(
                SharedAdvertisedEndpoint::from_config(&config(undeclared)).current(),
                None,
                "{undeclared:?}"
            );
        }
    }

    #[test]
    fn a_declared_endpoint_publishes_the_canonical_form() {
        // Exactly what the pairing payload would carry, including the
        // trimming and lowercasing that make the two agree.
        for (configured, expected) in [
            ("wss://a-host.example.ts.net", "wss://a-host.example.ts.net"),
            (
                "  wss://a-host.example.ts.net/  ",
                "wss://a-host.example.ts.net",
            ),
            (
                "WSS://a-host.example.ts.net:8443",
                "wss://a-host.example.ts.net:8443",
            ),
        ] {
            assert_eq!(
                SharedAdvertisedEndpoint::from_config(&config(Some(configured))).current(),
                Some(expected.to_string()),
                "{configured:?}"
            );
        }
    }

    #[test]
    fn an_endpoint_no_client_could_dial_publishes_nothing() {
        // The pong makes the same judgment as `herdr pair`: a url that would
        // be refused rather than encoded is not published either, because a
        // client that dials it fails long after the config was written.
        for unusable in [
            "a-host.example.ts.net",
            "https://a-host.example.ts.net",
            "wss://",
            "wss://user@a-host.example.ts.net",
            "wss://a-host.example.ts.net/?token=x",
            "wss://a-host.example.ts.net:0443",
        ] {
            assert_eq!(
                SharedAdvertisedEndpoint::from_config(&config(Some(unusable))).current(),
                None,
                "{unusable:?}"
            );
        }
    }

    #[test]
    fn a_reloaded_config_moves_the_published_endpoint() {
        let endpoint = SharedAdvertisedEndpoint::from_config(&config(None));
        assert_eq!(endpoint.current(), None);

        assert!(endpoint.apply_reloaded_config(&config(Some("wss://a-host.example.net"))));
        assert_eq!(
            endpoint.current(),
            Some("wss://a-host.example.net".to_string())
        );

        // The same value again is not a change, so nothing downstream is
        // told the declaration moved.
        assert!(!endpoint.apply_reloaded_config(&config(Some("wss://a-host.example.net"))));

        // A removed declaration clears it, and so does one that stopped
        // being dialable.
        assert!(endpoint.apply_reloaded_config(&config(Some("wss://a-host.example.net:0443"))));
        assert_eq!(endpoint.current(), None);
        assert!(endpoint.apply_reloaded_config(&config(Some("wss://a-host.example.net"))));
        assert!(endpoint.apply_reloaded_config(&config(None)));
        assert_eq!(endpoint.current(), None);
    }
}
