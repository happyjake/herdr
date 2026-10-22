mod advertised_endpoint;
mod agent_settings;
mod attachment;
pub mod client;
pub(crate) mod credentials;
mod event_hub;
mod place_lookup;
mod prompt_trail;
pub mod schema;
mod search_sessions;
mod server;
mod server_executable;
mod server_name;
mod server_reach;
mod status;
mod subscriptions;
mod wait;
mod websocket;

pub use advertised_endpoint::SharedAdvertisedEndpoint;
pub(crate) use advertised_endpoint::{declared_advertised_endpoint, normalize_advertised_endpoint};
pub use credentials::SharedCredentialRegistry;
pub use event_hub::EventHub;
pub(crate) use search_sessions::handle_active_session_snapshot;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub(crate) use server_executable::initialize as initialize_server_executable;
pub(crate) use server_name::resolve_server_name;
pub use server_name::SharedServerName;
pub use server_reach::SharedServerReach;
pub use status::{read_runtime_status_at, RuntimeStatus};
#[cfg(test)]
pub(crate) use websocket::start_websocket_server_with_capabilities;
pub(crate) use websocket::valid_token_chars as valid_websocket_token_chars;
pub use websocket::{start_websocket_server, SharedWebSocketToken, WebSocketServerHandle};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::Request;

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

/// What this build's API declares it can do, for every listener and every
/// server mode.
///
/// One constructor on purpose: the pong payload is contracted to be
/// identical across transports, and a mode that assembled its own set would
/// under-declare a capability the same binary in fact has — which a client
/// reads as "this server is too old" and acts on.
pub fn server_capabilities() -> crate::api::schema::ServerCapabilities {
    crate::api::schema::ServerCapabilities {
        live_handoff: crate::platform::capabilities().live_handoff,
        detached_server_daemon: crate::platform::current_process_is_detached_server_daemon(),
        endpoint_protocol_generation: Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION),
        surface_interest: true,
        health_check: true,
        // Only a unix socket whose agent registry opened can offer it; that
        // listener raises it for itself.
        ssh_agent_registration: false,
        send_affirm: true,
        stream_multiplex: true,
        credential_registry: true,
        file_attachments: Some(attachment::file_attachments_capability()),
        agent_report_cancelled: true,
        agent_prompt_expectations: true,
        turn_index: true,
        agent_settings: true,
        preface: true,
    }
}

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    request.method.operation().changes_ui
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
}

/// The process that sent a request over the local socket, read by the
/// server while the connection was open. Never taken from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPeer {
    pub pid: u32,
    pub placement: crate::platform::ProcessPlacement,
}

/// Whether a request's sender runs inside a given pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerInPane {
    Inside,
    Outside,
    Unknown,
}

impl RequestPeer {
    /// A sender with a controlling terminal is in the pane whose terminal
    /// that is. A sender without one (a hook started detached) is in the
    /// pane whose shell it descends from.
    pub fn in_pane(&self, pane_shell_pid: u32, pane_tty: Option<u64>) -> PeerInPane {
        if let (Some(peer_tty), Some(pane_tty)) = (self.placement.tty, pane_tty) {
            return if peer_tty == pane_tty {
                PeerInPane::Inside
            } else {
                PeerInPane::Outside
            };
        }
        match &self.placement.ancestry {
            Some(chain) if chain.contains(&pane_shell_pid) => PeerInPane::Inside,
            Some(_) => PeerInPane::Outside,
            None => PeerInPane::Unknown,
        }
    }
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

#[cfg(test)]
mod tests {
    /// One capability set for every server mode: monolithic mode used to
    /// declare nothing, so a client read the same binary as a server too old
    /// for features it in fact had.
    #[test]
    fn declared_capabilities_cover_the_additive_features() {
        let capabilities = super::server_capabilities();

        assert!(capabilities.credential_registry);
        assert!(capabilities.send_affirm);
        assert!(capabilities.stream_multiplex);
        assert!(capabilities.agent_report_cancelled);
        assert!(capabilities.agent_prompt_expectations);
        assert!(capabilities.turn_index);
        assert!(capabilities.agent_settings);
        assert!(capabilities.preface);
    }

    const PANE_SHELL: u32 = 500;
    const PANE_TTY: u64 = 0x1000_0003;

    fn peer(tty: Option<u64>, ancestry: Option<&[u32]>) -> super::RequestPeer {
        super::RequestPeer {
            pid: 900,
            placement: crate::platform::ProcessPlacement {
                tty,
                ancestry: ancestry.map(<[u32]>::to_vec),
            },
        }
    }

    #[test]
    fn a_sender_on_the_pane_terminal_is_inside_the_pane() {
        let sender = peer(Some(PANE_TTY), Some(&[900, 1]));

        assert_eq!(
            sender.in_pane(PANE_SHELL, Some(PANE_TTY)),
            super::PeerInPane::Inside
        );
    }

    /// An agent started from the pane's agent in a detached multiplexer
    /// session runs on that session's terminal and descends from its daemon.
    #[test]
    fn a_sender_on_another_terminal_is_outside_the_pane() {
        let sender = peer(Some(0x1000_0007), Some(&[900, 800, 1]));

        assert_eq!(
            sender.in_pane(PANE_SHELL, Some(PANE_TTY)),
            super::PeerInPane::Outside
        );
    }

    /// A hook started detached has no terminal; its parents still lead back
    /// to the pane's shell through the agent that ran it.
    #[test]
    fn a_sender_without_a_terminal_is_placed_by_its_parents() {
        let hook = peer(None, Some(&[900, 700, 600, PANE_SHELL, 1]));
        let daemon_child = peer(None, Some(&[900, 800, 1]));

        assert_eq!(
            hook.in_pane(PANE_SHELL, Some(PANE_TTY)),
            super::PeerInPane::Inside
        );
        assert_eq!(
            daemon_child.in_pane(PANE_SHELL, Some(PANE_TTY)),
            super::PeerInPane::Outside
        );
    }

    #[test]
    fn a_pane_whose_terminal_is_unknown_places_the_sender_by_its_parents() {
        let inside = peer(Some(PANE_TTY), Some(&[900, PANE_SHELL, 1]));
        let outside = peer(Some(PANE_TTY), Some(&[900, 800, 1]));

        assert_eq!(inside.in_pane(PANE_SHELL, None), super::PeerInPane::Inside);
        assert_eq!(
            outside.in_pane(PANE_SHELL, None),
            super::PeerInPane::Outside
        );
    }

    #[test]
    fn a_sender_whose_parents_cannot_be_read_is_unknown() {
        let sender = peer(None, None);

        assert_eq!(
            sender.in_pane(PANE_SHELL, Some(PANE_TTY)),
            super::PeerInPane::Unknown
        );
    }
}
