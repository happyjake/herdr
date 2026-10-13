mod advertised_endpoint;
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
    }
}
