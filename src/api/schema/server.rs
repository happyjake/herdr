use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PingParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerLiveHandoffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachmentCreateParams {
    /// Image bytes encoded as standard base64 (RFC 4648, padding accepted).
    pub bytes_b64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
    /// Whether `pane.send_input` dedupes by `send_id`, so a client may
    /// safely re-issue an unacked send after a reconnect. Additive: absent
    /// (false) in pongs of servers that predate it.
    #[serde(default)]
    pub send_affirm: bool,
    /// Whether this server serves requests that arrive on a connection while
    /// one of that connection's streams is running, so a client may subscribe
    /// once and keep issuing requests on the same connection.
    ///
    /// It describes the server, not one connection: transports that frame
    /// messages (WebSocket) honor it, while the Unix socket cannot multiplex
    /// and still ends the connection on payload during a stream. Reported
    /// identically on every transport, because the pong payload is contracted
    /// to be byte-identical across them. Additive: absent (false) in pongs of
    /// servers that predate it.
    #[serde(default)]
    pub stream_multiplex: bool,
}
