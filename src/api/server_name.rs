//! The display name a herdr server declares about itself.
//!
//! The name comes from `websocket_api.name`, falling back to the machine's
//! hostname when the config leaves it unset or empty. It is returned in the
//! `ping` pong on every transport and embedded in the pairing QR payload so
//! a client can show a friendly label before first connect. Names are
//! display only, never identity: nothing treats them as unique, and clients
//! compare servers by endpoint.

use std::sync::{Arc, PoisonError, RwLock};

use crate::config::WebSocketApiConfig;

/// Last-resort display name when the machine reports no usable hostname.
const FALLBACK_SERVER_NAME: &str = "herdr";

/// Resolve the declared server name from the config section: a non-empty
/// `websocket_api.name` wins, otherwise the machine hostname. The result is
/// never empty — a nameless pong would push the fallback problem onto every
/// client.
pub(crate) fn resolve_server_name(config: &WebSocketApiConfig) -> String {
    match config.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => crate::platform::hostname().unwrap_or_else(|| FALLBACK_SERVER_NAME.to_string()),
    }
}

/// The server's current display name, shared between the request dispatch
/// path and the owner of the config reload path. Every `ping` reads the
/// current value, so replacing the name takes effect on the next pong over
/// both transports without restarting any listener — the same live-slot
/// shape as [`super::SharedWebSocketToken`].
#[derive(Debug, Clone)]
pub struct SharedServerName {
    name: Arc<RwLock<String>>,
}

impl SharedServerName {
    #[cfg(test)]
    pub(crate) fn new(name: String) -> Self {
        Self {
            name: Arc::new(RwLock::new(name)),
        }
    }

    /// The name resolved from a loaded config section.
    pub fn from_config(config: &WebSocketApiConfig) -> Self {
        Self {
            name: Arc::new(RwLock::new(resolve_server_name(config))),
        }
    }

    /// Snapshot of the currently declared name.
    pub(crate) fn current(&self) -> String {
        self.name
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Apply a reloaded `[websocket_api]` section. Unlike the token, this
    /// never fails: an absent or empty name falls back to the hostname.
    /// Returns whether the name changed.
    pub(crate) fn apply_reloaded_config(&self, config: &WebSocketApiConfig) -> bool {
        let name = resolve_server_name(config);
        let mut current = self.name.write().unwrap_or_else(PoisonError::into_inner);
        if *current == name {
            return false;
        }
        *current = name;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_name(name: Option<&str>) -> WebSocketApiConfig {
        WebSocketApiConfig {
            name: name.map(str::to_string),
            ..WebSocketApiConfig::default()
        }
    }

    #[test]
    fn configured_name_wins() {
        assert_eq!(
            resolve_server_name(&config_with_name(Some("the-mini"))),
            "the-mini"
        );
        // Surrounding whitespace is presentation noise, not part of the name.
        assert_eq!(
            resolve_server_name(&config_with_name(Some("  the-mini  "))),
            "the-mini"
        );
    }

    #[test]
    fn absent_or_empty_name_falls_back_to_the_hostname() {
        let hostname =
            crate::platform::hostname().unwrap_or_else(|| FALLBACK_SERVER_NAME.to_string());
        for config in [
            config_with_name(None),
            config_with_name(Some("")),
            config_with_name(Some("   ")),
        ] {
            let resolved = resolve_server_name(&config);
            assert_eq!(resolved, hostname, "{:?}", config.name);
            assert!(!resolved.is_empty());
        }
    }

    #[test]
    fn reload_replaces_the_declared_name() {
        let shared = SharedServerName::new("old-name".to_string());

        assert!(shared.apply_reloaded_config(&config_with_name(Some("new-name"))));
        assert_eq!(shared.current(), "new-name");

        assert!(
            !shared.apply_reloaded_config(&config_with_name(Some("new-name"))),
            "an unchanged name must report unchanged"
        );
    }

    #[test]
    fn reload_without_a_name_falls_back_to_the_hostname() {
        let shared = SharedServerName::new("configured-name".to_string());

        shared.apply_reloaded_config(&config_with_name(None));

        assert_eq!(
            shared.current(),
            resolve_server_name(&config_with_name(None))
        );
        assert!(!shared.current().is_empty());
    }
}
