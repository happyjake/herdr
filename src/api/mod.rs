mod attachment;
pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod server_executable;
mod server_name;
mod server_reach;
mod status;
mod subscriptions;
mod wait;
mod websocket;

pub use event_hub::EventHub;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub(crate) use server_executable::initialize as initialize_server_executable;
pub(crate) use server_name::resolve_server_name;
pub use server_name::SharedServerName;
pub use server_reach::SharedServerReach;
pub use status::{read_runtime_status_at, RuntimeStatus};
pub(crate) use websocket::valid_token_chars as valid_websocket_token_chars;
pub use websocket::{start_websocket_server, SharedWebSocketToken, WebSocketServerHandle};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::Request;

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

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
