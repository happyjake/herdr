mod advertised_endpoint;
mod attachment;
pub mod client;
pub(crate) mod credentials;
mod event_hub;
mod place_lookup;
pub mod schema;
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
pub(crate) use server::start_server_with_stop_control;
pub use server::{start_server_with_capabilities, ServerHandle};
pub(crate) use server_executable::initialize as initialize_server_executable;
pub(crate) use server_name::resolve_server_name;
pub use server_name::SharedServerName;
pub use server_reach::SharedServerReach;
pub use status::{read_runtime_status_at, RuntimeStatus};
pub(crate) use websocket::valid_token_chars as valid_websocket_token_chars;
pub use websocket::{
    start_websocket_server, start_websocket_server_with_capabilities, SharedWebSocketToken,
    WebSocketServerHandle,
};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

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
        send_affirm: true,
        stream_multiplex: true,
        credential_registry: true,
        file_attachments: Some(attachment::file_attachments_capability()),
    }
}

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    matches!(
        &request.method,
        Method::ServerReloadConfig(_)
            | Method::ServerReloadAgentManifests(_)
            | Method::NotificationShow(_)
            | Method::WorkspaceCreate(_)
            | Method::WorkspaceFocus(_)
            | Method::WorkspaceRename(_)
            | Method::WorkspaceMove(_)
            | Method::WorkspaceMoveBlock(_)
            | Method::WorkspaceReportMetadata(_)
            | Method::WorkspaceClose(_)
            | Method::WorktreeCreate(_)
            | Method::WorktreeOpen(_)
            | Method::WorktreeRemove(_)
            | Method::TabCreate(_)
            | Method::TabFocus(_)
            | Method::TabRename(_)
            | Method::TabMove(_)
            | Method::TabClose(_)
            | Method::LayoutApply(_)
            | Method::LayoutSetSplitRatio(_)
            | Method::AgentRename(_)
            | Method::AgentViewSet(_)
            | Method::AgentViewClear(_)
            | Method::AgentFocus(_)
            | Method::AgentStart(_)
            | Method::AgentPrompt(_)
            | Method::AgentSendKeys(_)
            | Method::PaneSplit(_)
            | Method::PaneSwap(_)
            | Method::PaneMove(_)
            | Method::PaneZoom(_)
            | Method::PaneFocusDirection(_)
            | Method::PaneResize(_)
            | Method::PaneFocus(_)
            | Method::PaneInputSet(_)
            | Method::PaneRename(_)
            | Method::PaneGraphicsSet(_)
            | Method::PaneGraphicsClear(_)
            | Method::PaneGraphicsStream(_)
            | Method::PaneGraphicsStreamSet(_)
            | Method::PaneGraphicsStreamDirect(_)
            | Method::PaneGraphicsStreamOpen(_)
            | Method::PaneGraphicsStreamClose(_)
            | Method::PaneSendMouse(_)
            | Method::PaneReportAgent(_)
            | Method::PaneReportAgentSession(_)
            | Method::PaneReportMetadata(_)
            | Method::PaneClearAgentAuthority(_)
            | Method::PaneReleaseAgent(_)
            | Method::PaneClose(_)
            | Method::PopupClose(_)
            | Method::PluginUnlink(_)
            | Method::PluginDisable(_)
            | Method::PluginActionInvoke(_)
            | Method::PluginPaneOpen(_)
            | Method::PluginPaneFocus(_)
            | Method::PluginPaneClose(_)
    )
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    pub stream_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
    }
}
