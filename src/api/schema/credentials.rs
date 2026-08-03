use serde::{Deserialize, Serialize};

/// What a credential is allowed to do (ADR-0026).
///
/// A server honors one managing credential — the pairing's token — and any
/// number of limited credentials it has minted. Both drive the whole pane
/// API; the tier decides only who may manage the registry itself.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum CredentialTier {
    /// The pairing's credential: mints, lists, and revokes. Rotated by
    /// re-running `herdr pair`, never revoked through the API.
    Managing,
    /// A minted credential: full pane access, and may revoke only itself.
    #[default]
    Limited,
}

/// The refusals a credential verb can answer with, and what each one means
/// to the holder of the credential.
///
/// A client must be able to implement the revocation-versus-trouble branch
/// from these three codes alone. Anything else — `internal_error`,
/// `server_unavailable`, `invalid_params`, a dropped connection — is
/// trouble: retry, and change nothing locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CredentialRefusalCode {
    /// This credential no longer exists on this server: it was revoked, or
    /// it is not one this server issued. The holder should discard it and
    /// pair or link again; retrying will never succeed.
    ///
    /// Delivered as a JSON error on an established connection — including to
    /// a connection whose credential was revoked under it, and to a revoked
    /// credential that reconnects — because a browser cannot read a
    /// handshake's HTTP status. It is terminal: the server serves nothing
    /// else on that connection and then closes it.
    #[serde(rename = "credential_revoked")]
    Revoked,
    /// The credential is live but its tier does not permit this: a limited
    /// credential may not mint, list, or revoke another credential. The
    /// credential itself is unaffected and stays usable for everything else.
    #[serde(rename = "credential_forbidden")]
    Forbidden,
    /// No credential with that `credential_id` is in this server's registry
    /// — already revoked, or never here. The caller's own credential is
    /// unaffected.
    #[serde(rename = "credential_not_found")]
    NotFound,
}

impl CredentialRefusalCode {
    /// The exact string that appears in `ErrorBody.code`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Revoked => "credential_revoked",
            Self::Forbidden => "credential_forbidden",
            Self::NotFound => "credential_not_found",
        }
    }
}

/// One credential as the registry reports it. Never carries a token: a
/// minted token is returned once, at mint time, and stored only as a
/// fingerprint afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CredentialInfo {
    pub credential_id: String,
    pub tier: CredentialTier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at_unix: u64,
    /// Unix seconds of the last request this credential made, absent until
    /// it has been used. Server-observed, which is what makes the phone's
    /// linked-browsers panel a report rather than local bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_unix: Option<u64>,
}

/// The credential a local-socket caller acts as.
///
/// Authenticated transports carry the acting credential themselves — a
/// WebSocket connection acts as the credential it presented at the
/// handshake, and sending this field there is refused. The Unix socket has
/// no handshake: owning the socket file is already full authority over the
/// server, so a caller there may act as the managing credential (the
/// default) or name one of the registry's credentials explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CredentialActorParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting_token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CredentialMintParams {
    /// Display label for the holder, e.g. the linked browser's device name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting_token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CredentialRevokeParams {
    /// The credential to end. Absent means the caller's own credential —
    /// how a limited credential logs itself out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting_token: Option<String>,
}
