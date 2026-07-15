//! The operator-declared route to a herdr server.
//!
//! Reach comes from `websocket_api.reach`: it is what another machine calls
//! this box when opening a shell, typically an ssh alias. The process cannot
//! infer that fact from its hostname or display name, so an absent or empty
//! value stays undeclared.

use std::sync::{Arc, PoisonError, RwLock};

use crate::config::WebSocketApiConfig;

fn resolve_server_reach(config: &WebSocketApiConfig) -> Option<String> {
    config
        .reach
        .as_deref()
        .map(str::trim)
        .filter(|reach| !reach.is_empty())
        .map(str::to_string)
}

/// The server's current declared reach, shared by both API transports.
#[derive(Debug, Clone)]
pub struct SharedServerReach {
    reach: Arc<RwLock<Option<String>>>,
}

impl SharedServerReach {
    pub fn from_config(config: &WebSocketApiConfig) -> Self {
        Self {
            reach: Arc::new(RwLock::new(resolve_server_reach(config))),
        }
    }

    pub(crate) fn current(&self) -> Option<String> {
        self.reach
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Apply a reloaded `[websocket_api]` section. An absent, empty, or
    /// whitespace-only value clears the declaration. Returns whether reach
    /// changed.
    pub(crate) fn apply_reloaded_config(&self, config: &WebSocketApiConfig) -> bool {
        let reach = resolve_server_reach(config);
        let mut current = self.reach.write().unwrap_or_else(PoisonError::into_inner);
        if *current == reach {
            return false;
        }
        *current = reach;
        true
    }
}
