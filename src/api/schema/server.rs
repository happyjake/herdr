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
pub struct ServerLookupPlaceParams {
    /// What to look up. Text starting with `/` or `~` is a literal path,
    /// checked for existence; anything else is a place name, matched
    /// case-insensitively against the directories this server remembers.
    pub query: String,
    /// How many places to answer with, capped at eight. Absent means the
    /// cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Which of the server's memories held a place. Never a disk search: a
/// directory nobody has opened here is not a place this server can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlaceSource {
    /// A directory one of this server's own workspaces stands in, live or
    /// in the persisted layout.
    Workspace,
    /// The z frecency database.
    Z,
    /// The Claude Code project list.
    Claude,
    /// The working directory of a live tmux pane.
    Tmux,
    /// The working directory a recent agent session started in.
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlaceInfo {
    /// Absolute path of a directory that exists on this server.
    pub path: String,
    /// The memory this place came out of.
    pub source: PlaceSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachmentCreateParams {
    /// Attachment bytes encoded as standard base64 (RFC 4648, padding
    /// accepted). At most `capabilities.file_attachments.chunk_bytes`
    /// decoded, so the request still fits the unchanged message cap.
    pub bytes_b64: String,
    /// The file's own name, as the client knows it. Absent keeps the photo
    /// contract exactly: the bytes must sniff as JPEG, PNG, or WebP, and the
    /// server names the file alone. Present accepts any bytes verbatim and
    /// carries the sanitized name into the stored file's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachmentBeginParams {
    /// The file's own name, sanitized by the server into the stored name.
    pub name: String,
    /// Total bytes the upload will carry. Refused up front when it is over
    /// `capabilities.file_attachments.max_bytes` or the scratch volume has
    /// no room for it.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachmentAppendParams {
    /// The `upload_id` `attachment.begin` minted. Opaque: echo it back.
    pub upload_id: String,
    /// Byte offset this chunk starts at, which must equal the bytes already
    /// on disk — the partial file is the whole upload state.
    pub offset: u64,
    /// Chunk bytes encoded as standard base64, at most
    /// `capabilities.file_attachments.chunk_bytes` decoded.
    pub bytes_b64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachmentCommitParams {
    /// The `upload_id` `attachment.begin` minted.
    pub upload_id: String,
    /// Total bytes the finished file must hold. A partial of any other size
    /// is refused and left alone.
    pub size: u64,
}

/// What a chunked upload may carry on this server. Absent from the pong
/// capabilities of a server that predates files, which takes photos only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileAttachmentsCapability {
    /// Largest total size `attachment.begin` accepts.
    pub max_bytes: u64,
    /// Largest decoded payload one `attachment.create` or
    /// `attachment.append` may carry, so a client sizes its chunks from the
    /// server instead of hard-coding the message cap.
    pub chunk_bytes: u64,
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
    /// Whether this server keeps a credential registry (ADR-0026): the
    /// pairing's managing credential plus any minted limited credentials,
    /// with `credential.mint`, `credential.list`, `credential.revoke`, and
    /// `credential.revoke_all`. Additive: absent (false) in pongs of servers
    /// that predate it, which refuse the verbs as unparseable methods.
    #[serde(default)]
    pub credential_registry: bool,
    /// Whether this server stores any file as an attachment — a `name` on
    /// `attachment.create`, plus `attachment.begin`/`append`/`commit` for a
    /// file beyond one message — and the sizes it accepts. Additive: absent
    /// in pongs of servers that predate it, which take photos only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_attachments: Option<FileAttachmentsCapability>,
}
