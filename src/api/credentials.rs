//! The per-server credential registry (ADR-0026).
//!
//! A server honors more than one credential: the pairing's **managing**
//! credential — the token `herdr pair` mints into `[websocket_api].token`,
//! rotated by re-pairing — and any number of **limited** credentials the
//! managing credential mints for linked browsers. Every live credential
//! authenticates the WebSocket handshake and drives the whole pane API;
//! only the managing credential may mint, list, or revoke others. A limited
//! credential may revoke itself and nothing else.
//!
//! The registry is runtime state beside the session (`credentials.json` in
//! the session data dir), never config: minting must not edit config.toml,
//! and a restart keeps every credential. Only the managing credential's
//! *value* still lives in config — the registry records its identity and
//! clocks by fingerprint, which is how a re-pair is recognized as a
//! rotation of that one credential rather than a new registry.
//!
//! Nothing in the file is a secret. Credentials are stored as SHA-256
//! fingerprints and compared as fingerprints, so a minted token exists in
//! full exactly once: in the mint response that hands it to its holder.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::api::schema::{
    CredentialInfo, CredentialMintParams, CredentialRefusalCode, CredentialRevokeParams,
    CredentialTier, ErrorBody, ErrorResponse, ResponseResult, SuccessResponse,
};
use crate::api::SharedWebSocketToken;

/// Registry file name, beside `session.json` in the session data dir.
pub(crate) const CREDENTIALS_FILE: &str = "credentials.json";

/// On-disk format version. Bumped only when the shape changes; an unknown
/// (newer) version is ignored rather than downgraded, exactly like the
/// session snapshot.
const REGISTRY_VERSION: u32 = 1;

/// 256 bits of OS randomness per limited credential, base64url without
/// padding — the same shape and charset `herdr pair` mints, so a limited
/// credential rides the existing `token` query parameter unescaped.
const LIMITED_TOKEN_BYTES: usize = 32;

/// Labels are display strings for the phone's linked-browsers panel.
const MAX_LABEL_LEN: usize = 120;

/// How long a revoked credential stays discoverable.
///
/// A tombstone is what lets a returning holder be *told* it was revoked
/// instead of meeting an opaque 401 (a browser cannot read a handshake
/// status), so **age is the policy**: within this window a revocation is
/// never forgotten, whatever else has happened since. Past it, the holder
/// falls back to a silent rejection — by then it has had ninety days to
/// come back and be told, and it was already told on the connection it held
/// when the revoke landed.
const TOMBSTONE_TTL_SECS: u64 = 90 * 24 * 60 * 60;

/// Hard ceiling on remembered revocations, so the file cannot grow without
/// end — not a working limit.
///
/// It is deliberately far out of reach of real use: revoking every browser
/// of a large fleet, or unlinking and re-linking one daily, does not come
/// near it. Reaching it means averaging ~45 revocations a day for the whole
/// ninety-day window. The cost of holding it is small — a tombstone is an
/// id, a fingerprint, and a timestamp, about 150 bytes of JSON, so a full
/// store is well under a megabyte — which is exactly why the ceiling can be
/// set high enough that count pressure never evicts a recent revocation.
/// A server that does hit it keeps the newest and logs that it is dropping
/// revocations; forgetting one silently is what would leave a logged-out
/// browser retrying forever.
const MAX_TOMBSTONES: usize = 4096;

/// How stale a persisted `last_seen` may get. Requests touch last-seen in
/// memory on every call; the file is rewritten at most this often so a busy
/// connection does not turn every request into a disk write.
const LAST_SEEN_PERSIST_INTERVAL_SECS: u64 = 60;

/// Refusals a client can act on. The wire meaning of each is published in
/// [`CredentialRefusalCode`], which is the contract a client implements the
/// revocation-versus-trouble branch from; these are the same strings, taken
/// from there so the code and the published schema cannot drift.
const CODE_FORBIDDEN: &str = CredentialRefusalCode::Forbidden.as_str();
const CODE_NOT_FOUND: &str = CredentialRefusalCode::NotFound.as_str();
const CODE_REVOKED: &str = CredentialRefusalCode::Revoked.as_str();

/// One sentence for every revoked-credential refusal, so a client keys on
/// the code and can still show something true.
const REVOKED_MESSAGE: &str = "this credential has been revoked; pair or link again";

/// One credential as stored. The token itself is never written down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredCredential {
    id: String,
    /// Lowercase hex SHA-256 of the credential's token.
    fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    created_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen_unix: Option<u64>,
}

impl StoredCredential {
    fn info(&self, tier: CredentialTier) -> CredentialInfo {
        CredentialInfo {
            credential_id: self.id.clone(),
            tier,
            label: self.label.clone(),
            created_at_unix: self.created_at_unix,
            last_seen_unix: self.last_seen_unix,
        }
    }
}

/// A credential that was live and is not any more.
///
/// Only its fingerprint and identity are kept, exactly like a live entry, so
/// the file still holds no secrets. Oldest-first pruning is in
/// [`push_tombstone`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokedCredential {
    id: String,
    fingerprint: String,
    revoked_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managing: Option<StoredCredential>,
    #[serde(default)]
    limited: Vec<StoredCredential>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    revoked: Vec<RevokedCredential>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            managing: None,
            limited: Vec::new(),
            revoked: Vec::new(),
        }
    }
}

/// A credential that presented itself and was recognized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthenticatedCredential {
    pub(crate) credential_id: String,
    pub(crate) tier: CredentialTier,
}

/// What a presented token turned out to be.
///
/// The three cases are deliberately distinct at the handshake: a browser
/// cannot read an HTTP status or body, so telling a revoked credential apart
/// from a token this server never issued is what lets one be answered in
/// JSON on an established connection while the other keeps its opaque 401.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandshakeOutcome {
    Live(AuthenticatedCredential),
    /// A credential this server minted and has since revoked.
    Revoked {
        credential_id: String,
    },
    /// Not a credential of this server's, now or ever (as far as it
    /// remembers).
    Unknown,
}

/// Whether this registry may be written at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryFormat {
    Writable,
    /// The file on disk was written by a newer herdr. Its contents are
    /// intact and meaningful to that version, so this one refuses every
    /// write rather than downgrading a user's credentials away.
    Newer {
        file_version: u32,
    },
}

struct RegistryState {
    path: PathBuf,
    format: RegistryFormat,
    file: RegistryFile,
    /// What `last_seen_unix` was when the file was last written, per id, so
    /// the throttle knows when the file has drifted far enough to rewrite.
    persisted_last_seen: HashMap<String, u64>,
}

/// The live registry, shared by every listener in the process.
///
/// Cloning shares one registry: the Unix socket and the WebSocket listener
/// must agree about which credentials exist, and a mint over one transport
/// is live on the other before the response is written.
#[derive(Clone)]
pub struct SharedCredentialRegistry {
    state: Arc<Mutex<RegistryState>>,
    /// The managing credential's live value, owned by the WebSocket
    /// listener. Reading through the shared slot rather than copying the
    /// token means a `herdr pair` rotation applied by a config reload is
    /// recognized here with no extra plumbing.
    managing_token: Arc<RwLock<Option<SharedWebSocketToken>>>,
}

impl std::fmt::Debug for SharedCredentialRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedCredentialRegistry").finish()
    }
}

impl SharedCredentialRegistry {
    /// Open (or start) the registry stored at `path`.
    ///
    /// A missing file is an empty registry. A file that cannot be read or
    /// parsed is ignored with a warning rather than fatal: refusing to serve
    /// because a side file went bad would take the whole server down, and
    /// the managing credential still authenticates from config.
    pub(crate) fn open(path: PathBuf) -> Self {
        let (file, format) = load_registry_file(&path);
        let persisted_last_seen = last_seen_index(&file);
        Self {
            state: Arc::new(Mutex::new(RegistryState {
                path,
                format,
                file,
                persisted_last_seen,
            })),
            managing_token: Arc::new(RwLock::new(None)),
        }
    }

    /// Point the registry at the listener's live managing token. Called when
    /// the WebSocket listener starts; without it the registry knows only its
    /// limited credentials, and the managing credential is whoever owns the
    /// local socket.
    pub(crate) fn attach_managing_token(&self, token: SharedWebSocketToken) {
        *self
            .managing_token
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(token);
    }

    fn managing_token(&self) -> Option<String> {
        self.managing_token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(SharedWebSocketToken::current)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a presented token is live, and as whom.
    ///
    /// Test-only: every caller in the server takes the three-way outcome
    /// instead, so that no path can quietly collapse "revoked" into
    /// "unknown" — that distinction is the whole revocation-discovery
    /// contract.
    #[cfg(test)]
    fn authenticate(&self, presented: &str) -> Option<AuthenticatedCredential> {
        match self.authenticate_handshake(presented) {
            HandshakeOutcome::Live(credential) => Some(credential),
            HandshakeOutcome::Revoked { .. } | HandshakeOutcome::Unknown => None,
        }
    }

    /// Recognize a presented token, including one this server revoked.
    ///
    /// A live credential is marked seen, which is what makes
    /// `credential.list`'s `last_seen_unix` a server-verified fact. Only
    /// [`HandshakeOutcome::Live`] is honored; the revoked case is
    /// recognition, not access — it exists so the holder can be told what
    /// happened over a transport that can carry a reason.
    pub(crate) fn authenticate_handshake(&self, presented: &str) -> HandshakeOutcome {
        let fingerprint = fingerprint(presented);
        let managing_token = self.managing_token();
        let mut state = self.lock();
        sync_managing(&mut state, managing_token.as_deref());

        let now = unix_now();
        if let Some(managing) = state.file.managing.as_mut() {
            if constant_time_eq(managing.fingerprint.as_bytes(), fingerprint.as_bytes()) {
                let id = managing.id.clone();
                managing.last_seen_unix = Some(now);
                persist_last_seen(&mut state, &id, now);
                return HandshakeOutcome::Live(AuthenticatedCredential {
                    credential_id: id,
                    tier: CredentialTier::Managing,
                });
            }
        }

        let found = state
            .file
            .limited
            .iter_mut()
            .find(|credential| {
                constant_time_eq(credential.fingerprint.as_bytes(), fingerprint.as_bytes())
            })
            .map(|credential| {
                credential.last_seen_unix = Some(now);
                credential.id.clone()
            });
        if let Some(id) = found {
            persist_last_seen(&mut state, &id, now);
            return HandshakeOutcome::Live(AuthenticatedCredential {
                credential_id: id,
                tier: CredentialTier::Limited,
            });
        }

        match state.file.revoked.iter().find(|tombstone| {
            constant_time_eq(tombstone.fingerprint.as_bytes(), fingerprint.as_bytes())
        }) {
            Some(tombstone) => HandshakeOutcome::Revoked {
                credential_id: tombstone.id.clone(),
            },
            None => HandshakeOutcome::Unknown,
        }
    }

    /// Mark an already-authenticated credential seen, reporting whether it
    /// is still live. This is the per-request check that turns a revoke into
    /// enforcement on an open connection instead of a request to disconnect.
    pub(crate) fn mark_seen(&self, credential_id: &str) -> Option<CredentialTier> {
        let managing_token = self.managing_token();
        let mut state = self.lock();
        sync_managing(&mut state, managing_token.as_deref());

        let now = unix_now();
        let tier = match state.file.managing.as_mut() {
            Some(managing) if managing.id == credential_id => {
                managing.last_seen_unix = Some(now);
                Some(CredentialTier::Managing)
            }
            _ => state
                .file
                .limited
                .iter_mut()
                .find(|credential| credential.id == credential_id)
                .map(|credential| {
                    credential.last_seen_unix = Some(now);
                    CredentialTier::Limited
                }),
        }?;
        persist_last_seen(&mut state, credential_id, now);
        Some(tier)
    }

    /// Every live credential, managing first. Metadata only: the tokens are
    /// not stored, so they cannot be listed.
    pub(crate) fn list(&self) -> Vec<CredentialInfo> {
        let managing_token = self.managing_token();
        let mut state = self.lock();
        sync_managing(&mut state, managing_token.as_deref());

        let mut credentials = Vec::new();
        if let Some(managing) = state.file.managing.as_ref() {
            credentials.push(managing.info(CredentialTier::Managing));
        }
        credentials.extend(
            state
                .file
                .limited
                .iter()
                .map(|credential| credential.info(CredentialTier::Limited)),
        );
        credentials
    }

    /// Mint a limited credential. Returns its record and — once — its token.
    pub(crate) fn mint(&self, label: Option<String>) -> io::Result<(CredentialInfo, String)> {
        let token = mint_token()?;
        let credential = StoredCredential {
            id: new_credential_id(),
            fingerprint: fingerprint(&token),
            label,
            created_at_unix: unix_now(),
            last_seen_unix: None,
        };

        let mut state = self.lock();
        let mut candidate = state.file.clone();
        candidate.limited.push(credential.clone());
        commit(&mut state, candidate)?;
        Ok((credential.info(CredentialTier::Limited), token))
    }

    /// Revoke one limited credential by id.
    ///
    /// The new registry is written before it is believed: a revoke that
    /// cannot be persisted must leave the world exactly as it was, or the
    /// grant would come back on the next restart while the caller had been
    /// told it was gone.
    pub(crate) fn revoke_limited(&self, credential_id: &str) -> io::Result<bool> {
        let mut state = self.lock();
        let Some(position) = state
            .file
            .limited
            .iter()
            .position(|credential| credential.id == credential_id)
        else {
            return Ok(false);
        };

        let mut candidate = state.file.clone();
        let removed = candidate.limited.remove(position);
        push_tombstone(&mut candidate, removed, unix_now());
        commit(&mut state, candidate)?;
        state.persisted_last_seen.remove(credential_id);
        Ok(true)
    }

    /// End every limited credential at once — the stolen-phone case.
    /// The managing credential is untouched: it rotates with `herdr pair`.
    pub(crate) fn revoke_all_limited(&self) -> io::Result<Vec<String>> {
        let mut state = self.lock();
        let mut candidate = state.file.clone();
        let ended: Vec<StoredCredential> = candidate.limited.drain(..).collect();
        let revoked: Vec<String> = ended
            .iter()
            .map(|credential| credential.id.clone())
            .collect();
        // One timestamp and one prune for the whole batch: revoking every
        // credential at once must leave every one of them discoverable, not
        // let the last few evict the first few.
        let now = unix_now();
        candidate.revoked.extend(
            ended
                .into_iter()
                .map(|credential| tombstone(credential, now)),
        );
        prune_tombstones(&mut candidate.revoked, now);
        commit(&mut state, candidate)?;
        for id in &revoked {
            state.persisted_last_seen.remove(id);
        }
        Ok(revoked)
    }

    /// Whether an id names the managing credential. Revoking it is refused:
    /// the managing credential ends by rotation (`herdr pair`), and a phone
    /// that could revoke it would lock every client out of the server.
    fn is_managing_id(&self, credential_id: &str) -> bool {
        let managing_token = self.managing_token();
        let mut state = self.lock();
        sync_managing(&mut state, managing_token.as_deref());
        state
            .file
            .managing
            .as_ref()
            .is_some_and(|managing| managing.id == credential_id)
    }
}

/// A registry path for tests, under the crate's own `target` directory.
///
/// Deliberately not the process temp dir: another test in this binary
/// redirects `TMPDIR` and then deletes that directory
/// (`remote::unix::tests::local_forward_socket_path_falls_back_to_tmp_when_dir_is_long`),
/// which would delete a concurrent test's registry out from under it.
#[cfg(test)]
pub(crate) fn test_registry_path(name: &str) -> PathBuf {
    let unique = format!(
        "{name}-{}-{}-{:?}",
        std::process::id(),
        unix_nanos(),
        std::thread::current().id()
    );
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-credentials")
        .join(unique)
        .join(CREDENTIALS_FILE)
}

/// The process-wide registry, beside the active session.
pub(crate) fn process_registry() -> SharedCredentialRegistry {
    static REGISTRY: OnceLock<SharedCredentialRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            SharedCredentialRegistry::open(crate::session::data_dir().join(CREDENTIALS_FILE))
        })
        .clone()
}

/// Keep the stored managing credential in step with the live token.
///
/// A re-pair replaces `[websocket_api].token`; the registry notices the new
/// fingerprint and starts a fresh managing credential — new id, new
/// created-at, no last-seen. Limited credentials are not involved, which is
/// exactly ADR-0026's promise that a rotation does not log the browsers out.
fn sync_managing(state: &mut RegistryState, managing_token: Option<&str>) {
    let Some(token) = managing_token else {
        return;
    };
    let fingerprint = fingerprint(token);
    if state
        .file
        .managing
        .as_ref()
        .is_some_and(|managing| managing.fingerprint == fingerprint)
    {
        return;
    }

    if let Some(previous) = state.file.managing.take() {
        state.persisted_last_seen.remove(&previous.id);
    }
    state.file.managing = Some(StoredCredential {
        id: new_credential_id(),
        fingerprint,
        label: None,
        created_at_unix: unix_now(),
        last_seen_unix: None,
    });
    // Memory first here, unlike a mint or a revoke: the managing
    // credential's value is decided by config, not by this file, so a failed
    // write must not stop the freshly paired client from authenticating. The
    // file catches up on the next successful write or the next start.
    if let Err(err) = save_current(state) {
        warn!(err = %err, "failed to record the rotated managing credential");
    }
}

/// Remember a revoked credential.
fn push_tombstone(file: &mut RegistryFile, credential: StoredCredential, now: u64) {
    file.revoked.push(tombstone(credential, now));
    prune_tombstones(&mut file.revoked, now);
}

fn tombstone(credential: StoredCredential, now: u64) -> RevokedCredential {
    RevokedCredential {
        id: credential.id,
        fingerprint: credential.fingerprint,
        revoked_at_unix: now,
    }
}

/// Evict by age, and only then by count.
///
/// Nothing still inside [`TOMBSTONE_TTL_SECS`] is dropped to make room for
/// something newer: a revocation the holder has not yet come back to learn
/// about is precisely the one worth keeping, and a small count cap that
/// evicted it would turn a bulk revoke — log every browser out at once —
/// into a set of browsers that can never find out. Only a store that is
/// still over the ceiling with every entry recent loses anything, and it
/// says so.
fn prune_tombstones(revoked: &mut Vec<RevokedCredential>, now: u64) {
    revoked.retain(|tombstone| now.saturating_sub(tombstone.revoked_at_unix) < TOMBSTONE_TTL_SECS);
    if revoked.len() <= MAX_TOMBSTONES {
        return;
    }

    let dropped = revoked.len() - MAX_TOMBSTONES;
    warn!(
        dropped,
        ceiling = MAX_TOMBSTONES,
        "credential revocation history is full; forgetting the oldest still-recent revocations, \
         whose holders will now meet a silent rejection instead of being told they were revoked"
    );
    revoked.drain(..dropped);
}

/// Record a fresh last-seen, rewriting the file only when the persisted copy
/// has drifted past the throttle. A crash therefore loses at most one
/// interval of last-seen precision and never a credential.
fn persist_last_seen(state: &mut RegistryState, credential_id: &str, now: u64) {
    let persisted = state.persisted_last_seen.get(credential_id).copied();
    let due = match persisted {
        Some(previous) => now.saturating_sub(previous) >= LAST_SEEN_PERSIST_INTERVAL_SECS,
        None => true,
    };
    if !due {
        return;
    }
    state.persisted_last_seen.insert(credential_id.into(), now);
    if let Err(err) = save_current(state) {
        warn!(err = %err, "failed to persist credential last-seen");
    }
}

fn last_seen_index(file: &RegistryFile) -> HashMap<String, u64> {
    file.managing
        .iter()
        .chain(file.limited.iter())
        .filter_map(|credential| {
            credential
                .last_seen_unix
                .map(|seen| (credential.id.clone(), seen))
        })
        .collect()
}

/// Read the registry, reporting whether it may be written back.
///
/// A missing or unparseable file starts empty and writable — unparseable
/// bytes carry nothing to protect. A file from a *newer* herdr is different:
/// it is intact and meaningful there, so it is preserved and this version
/// refuses to write over it. An unreadable file is never fatal: the server
/// must still serve, and the managing credential authenticates from config.
fn load_registry_file(path: &Path) -> (RegistryFile, RegistryFormat) {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (RegistryFile::default(), RegistryFormat::Writable)
        }
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to read credential registry, starting empty");
            return (RegistryFile::default(), RegistryFormat::Writable);
        }
    };

    let file_version = serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    if file_version > u64::from(REGISTRY_VERSION) {
        warn!(
            path = %path.display(),
            file_version,
            supported = REGISTRY_VERSION,
            "credential registry was written by a newer herdr; keeping it as found and refusing to write to it"
        );
        return (
            RegistryFile::default(),
            RegistryFormat::Newer {
                file_version: file_version.try_into().unwrap_or(u32::MAX),
            },
        );
    }

    match serde_json::from_str::<RegistryFile>(&content) {
        Ok(file) => (file, RegistryFormat::Writable),
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to parse credential registry, starting empty");
            (RegistryFile::default(), RegistryFormat::Writable)
        }
    }
}

/// Adopt a candidate registry only once it is safely on disk.
///
/// Authority-changing edits (mint, revoke, revoke-all) go through here so a
/// storage failure leaves memory and disk agreeing on the old state: a
/// half-applied revoke would answer a retry with `credential_not_found` —
/// which reads as confirmation — and hand the grant back at the next start.
fn commit(state: &mut RegistryState, candidate: RegistryFile) -> io::Result<()> {
    write_registry_file(&state.path, &state.format, &candidate)?;
    state.file = candidate;
    Ok(())
}

/// Best-effort write of the live registry, for the facts memory already
/// owns: last-seen clocks and the managing entry config decides.
fn save_current(state: &mut RegistryState) -> io::Result<()> {
    write_registry_file(&state.path, &state.format, &state.file)
}

/// Write the registry through a temp file so a crash mid-write cannot leave
/// a half-written registry — losing the file would lock every linked browser
/// out at once.
fn write_registry_file(
    path: &Path,
    format: &RegistryFormat,
    file: &RegistryFile,
) -> io::Result<()> {
    if let RegistryFormat::Newer { file_version } = format {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the credential registry at {} was written by a newer herdr \
                 (format {file_version}, this build supports {REGISTRY_VERSION}); \
                 refusing to overwrite it — run the newer herdr, or move that \
                 file aside to start a fresh registry",
                path.display()
            ),
        ));
    }

    let mut file = file.clone();
    file.version = REGISTRY_VERSION;
    let json = serde_json::to_string_pretty(&file)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)?;
    restrict_registry_permissions(&tmp_path)?;
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_registry_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_registry_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn mint_token() -> io::Result<String> {
    let mut bytes = [0u8; LIMITED_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| {
        io::Error::other(format!(
            "could not gather randomness for a credential: {err}"
        ))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn new_credential_id() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Ids only need to be unique within one registry; the clock is a
        // sound fallback when the OS entropy source is momentarily unusable.
        bytes = unix_nanos().to_be_bytes();
    }
    let mut id = String::with_capacity(4 + bytes.len() * 2);
    id.push_str("cred_");
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

fn fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Compare without an early exit, so the time taken does not depend on how
/// many leading bytes match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or_default()
}

/// Who is asking, and the registry that answers for them.
///
/// The actor is a transport fact, never a claim in the payload: a WebSocket
/// connection acts as the credential its handshake presented, and the local
/// socket acts as the machine's owner, whose filesystem permission on the
/// socket is already full authority over this server.
#[derive(Clone, Debug)]
pub(crate) struct CredentialContext {
    registry: SharedCredentialRegistry,
    actor: Actor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Actor {
    /// The Unix socket: authority comes from owning the socket file.
    LocalSocket,
    /// A connection authenticated as this credential at its handshake. The
    /// tier is the one it presented and never changes for this connection.
    Connection {
        credential_id: String,
        tier: CredentialTier,
    },
    /// A connection that presented a credential this server has revoked.
    ///
    /// It is admitted deliberately: a browser cannot read a handshake's
    /// status or body, so a 401 and a dead endpoint look the same to it and
    /// revocation would be invisible. Admitted, it can be told in JSON — and
    /// told nothing else.
    RevokedConnection { credential_id: String },
}

/// The acting credential, resolved against the registry at request time.
struct ResolvedActor {
    credential_id: Option<String>,
    tier: CredentialTier,
}

impl CredentialContext {
    pub(crate) fn local_socket(registry: SharedCredentialRegistry) -> Self {
        Self {
            registry,
            actor: Actor::LocalSocket,
        }
    }

    pub(crate) fn connection(
        registry: SharedCredentialRegistry,
        credential: AuthenticatedCredential,
    ) -> Self {
        Self {
            registry,
            actor: Actor::Connection {
                credential_id: credential.credential_id,
                tier: credential.tier,
            },
        }
    }

    /// A connection admitted only to be told its credential is gone.
    pub(crate) fn revoked_connection(
        registry: SharedCredentialRegistry,
        credential_id: String,
    ) -> Self {
        Self {
            registry,
            actor: Actor::RevokedConnection { credential_id },
        }
    }

    /// The credential this connection presented, revoked or not. For logs.
    pub(crate) fn credential_id(&self) -> Option<&str> {
        match &self.actor {
            Actor::LocalSocket => None,
            Actor::Connection { credential_id, .. }
            | Actor::RevokedConnection { credential_id } => Some(credential_id),
        }
    }

    /// The terminal verdict frame, for a transport that must deliver it
    /// without a request to answer — a held stream, or an idle connection
    /// whose credential was revoked under it. Answered off-id (empty id),
    /// the same shape this API already uses for refusals that belong to no
    /// request; only the code decides what it means.
    pub(crate) fn revocation_verdict(&self) -> String {
        error_json("", CODE_REVOKED, REVOKED_MESSAGE.into())
    }

    /// Re-check the connection's own credential before serving a request,
    /// and mark it seen. `Some(response)` is a refusal to write instead of
    /// serving: the credential was revoked while the connection was open, so
    /// its access ends here rather than at its next reconnect.
    ///
    /// A `credential_revoked` answer is a verdict, not trouble — clients key
    /// their wipe-and-relink path on this code.
    pub(crate) fn revocation_refusal(&self, request_id: &str) -> Option<String> {
        if !self.is_revoked() {
            return None;
        }
        Some(error_json(request_id, CODE_REVOKED, REVOKED_MESSAGE.into()))
    }

    /// Whether the connection's own credential has been revoked, marking it
    /// seen when it has not. Streaming transports poll this so a revoke ends
    /// an open subscription too — "log that browser out" has to mean the
    /// browser goes dark, not that it stops being served new requests.
    pub(crate) fn is_revoked(&self) -> bool {
        match &self.actor {
            Actor::LocalSocket => false,
            Actor::RevokedConnection { .. } => true,
            // A managing connection keeps its grant for its lifetime. The
            // only thing that removes its entry is a re-pair, and rotation
            // has never severed connections that already authenticated
            // (ADR-0003); revocation targets limited credentials.
            Actor::Connection {
                credential_id,
                tier: CredentialTier::Managing,
            } => {
                self.registry.mark_seen(credential_id);
                false
            }
            Actor::Connection { credential_id, .. } => {
                self.registry.mark_seen(credential_id).is_none()
            }
        }
    }

    /// Resolve who is acting. `acting_token` is a local-socket affordance —
    /// a caller that already owns the socket may act as one of the registry's
    /// credentials, which is how tooling and tests exercise limited-tier
    /// behavior over a transport that has no handshake. An authenticated
    /// connection cannot claim to be someone else.
    fn resolve(&self, acting_token: Option<&str>) -> Result<ResolvedActor, (&'static str, String)> {
        match (&self.actor, acting_token) {
            (Actor::RevokedConnection { .. }, _) => Err((CODE_REVOKED, REVOKED_MESSAGE.into())),
            (Actor::Connection { .. }, Some(_)) => Err((
                "invalid_params",
                "acting_token is not accepted on an authenticated connection; \
                 the credential presented at the handshake is the actor"
                    .into(),
            )),
            (
                Actor::Connection {
                    credential_id,
                    tier: CredentialTier::Managing,
                },
                None,
            ) => {
                self.registry.mark_seen(credential_id);
                Ok(ResolvedActor {
                    credential_id: Some(credential_id.clone()),
                    tier: CredentialTier::Managing,
                })
            }
            (Actor::Connection { credential_id, .. }, None) => {
                match self.registry.mark_seen(credential_id) {
                    Some(tier) => Ok(ResolvedActor {
                        credential_id: Some(credential_id.clone()),
                        tier,
                    }),
                    None => Err((CODE_REVOKED, REVOKED_MESSAGE.into())),
                }
            }
            (Actor::LocalSocket, Some(token)) => {
                match self.registry.authenticate_handshake(token) {
                    HandshakeOutcome::Live(credential) => Ok(ResolvedActor {
                        credential_id: Some(credential.credential_id),
                        tier: credential.tier,
                    }),
                    // Both refusals carry the same code on purpose: to the
                    // holder of a token this server will not honor, "revoked"
                    // and "never mine" call for the same wipe.
                    HandshakeOutcome::Revoked { .. } => Err((CODE_REVOKED, REVOKED_MESSAGE.into())),
                    HandshakeOutcome::Unknown => Err((
                        CODE_REVOKED,
                        "the presented credential is not in this server's registry".into(),
                    )),
                }
            }
            // Owning the socket file is managing authority by itself: it is
            // the same access `herdr pair` and `herdr server stop` already
            // have, so there is nothing weaker to fall back to.
            (Actor::LocalSocket, None) => Ok(ResolvedActor {
                credential_id: None,
                tier: CredentialTier::Managing,
            }),
        }
    }

    pub(crate) fn serve_mint(&self, request_id: String, params: &CredentialMintParams) -> String {
        let actor = match self.resolve(params.acting_token.as_deref()) {
            Ok(actor) => actor,
            Err((code, message)) => return error_json(&request_id, code, message),
        };
        if actor.tier != CredentialTier::Managing {
            return forbidden(&request_id, "mint credentials");
        }
        let label = match normalize_label(params.label.as_deref()) {
            Ok(label) => label,
            Err(message) => return error_json(&request_id, "invalid_params", message),
        };

        match self.registry.mint(label) {
            Ok((credential, token)) => success(
                request_id,
                ResponseResult::CredentialMinted { credential, token },
            ),
            Err(err) => internal_error(&request_id, &err),
        }
    }

    pub(crate) fn serve_list(&self, request_id: String, acting_token: Option<&str>) -> String {
        let actor = match self.resolve(acting_token) {
            Ok(actor) => actor,
            Err((code, message)) => return error_json(&request_id, code, message),
        };
        if actor.tier != CredentialTier::Managing {
            return forbidden(&request_id, "list credentials");
        }
        success(
            request_id,
            ResponseResult::CredentialList {
                credentials: self.registry.list(),
            },
        )
    }

    pub(crate) fn serve_revoke(
        &self,
        request_id: String,
        params: &CredentialRevokeParams,
    ) -> String {
        let actor = match self.resolve(params.acting_token.as_deref()) {
            Ok(actor) => actor,
            Err((code, message)) => return error_json(&request_id, code, message),
        };

        let target = match params
            .credential_id
            .as_deref()
            .or(actor.credential_id.as_deref())
        {
            Some(target) => target.to_string(),
            None => {
                return error_json(
                    &request_id,
                    "invalid_params",
                    "credential_id is required when the caller is not itself a credential".into(),
                )
            }
        };

        let is_self = actor
            .credential_id
            .as_deref()
            .is_some_and(|id| id == target);
        if actor.tier != CredentialTier::Managing && !is_self {
            return forbidden(&request_id, "revoke another credential");
        }
        if self.registry.is_managing_id(&target) {
            return error_json(
                &request_id,
                CODE_FORBIDDEN,
                "the managing credential cannot be revoked; `herdr pair` rotates it".into(),
            );
        }

        match self.registry.revoke_limited(&target) {
            Ok(true) => success(
                request_id,
                ResponseResult::CredentialRevoked {
                    revoked: vec![target],
                },
            ),
            Ok(false) => error_json(
                &request_id,
                CODE_NOT_FOUND,
                format!("no credential {target} in this server's registry"),
            ),
            Err(err) => internal_error(&request_id, &err),
        }
    }

    pub(crate) fn serve_revoke_all(
        &self,
        request_id: String,
        acting_token: Option<&str>,
    ) -> String {
        let actor = match self.resolve(acting_token) {
            Ok(actor) => actor,
            Err((code, message)) => return error_json(&request_id, code, message),
        };
        if actor.tier != CredentialTier::Managing {
            return forbidden(&request_id, "revoke every credential");
        }
        match self.registry.revoke_all_limited() {
            Ok(revoked) => success(request_id, ResponseResult::CredentialRevoked { revoked }),
            Err(err) => internal_error(&request_id, &err),
        }
    }
}

fn normalize_label(label: Option<&str>) -> Result<Option<String>, String> {
    let Some(label) = label else {
        return Ok(None);
    };
    let label = label.trim();
    if label.is_empty() {
        return Ok(None);
    }
    if label.chars().count() > MAX_LABEL_LEN {
        return Err(format!("label is longer than {MAX_LABEL_LEN} characters"));
    }
    if label.chars().any(|ch| ch.is_control()) {
        return Err("label must not contain control characters".into());
    }
    Ok(Some(label.to_string()))
}

fn forbidden(request_id: &str, action: &str) -> String {
    error_json(
        request_id,
        CODE_FORBIDDEN,
        format!("a limited credential may not {action}; only the managing credential can"),
    )
}

fn internal_error(request_id: &str, err: &io::Error) -> String {
    // Storage trouble is trouble, never a verdict about the credential: a
    // client that wiped itself over this would lose a working grant. The
    // registry is written before it is believed, so this also states a fact:
    // nothing changed, and the same request can simply be retried.
    error_json(
        request_id,
        "internal_error",
        format!("the credential registry could not be updated: {err}; nothing changed"),
    )
}

fn success(request_id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse {
        id: request_id,
        result,
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
            .to_string()
    })
}

fn error_json(request_id: &str, code: &str, message: String) -> String {
    serde_json::to_string(&ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody {
            code: code.to_string(),
            message,
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry_path(name: &str) -> PathBuf {
        test_registry_path(name)
    }

    fn registry_with_managing(name: &str, token: &str) -> (SharedCredentialRegistry, PathBuf) {
        let path = temp_registry_path(name);
        let registry = SharedCredentialRegistry::open(path.clone());
        registry.attach_managing_token(SharedWebSocketToken::new(token.to_string()));
        (registry, path)
    }

    #[test]
    fn the_pairing_token_authenticates_as_the_managing_credential() {
        let (registry, _path) = registry_with_managing("managing", "pair-token");

        let credential = registry.authenticate("pair-token").expect("live token");

        assert_eq!(credential.tier, CredentialTier::Managing);
        assert!(registry.authenticate("some-other-token").is_none());
        assert!(registry.is_managing_id(&credential.credential_id));
    }

    #[test]
    fn a_minted_credential_authenticates_as_limited() {
        let (registry, _path) = registry_with_managing("minted", "pair-token");

        let (info, token) = registry.mint(Some("desk browser".into())).unwrap();

        assert_eq!(info.tier, CredentialTier::Limited);
        assert_eq!(info.label.as_deref(), Some("desk browser"));
        assert_eq!(info.last_seen_unix, None);
        let credential = registry.authenticate(&token).expect("minted token is live");
        assert_eq!(credential.tier, CredentialTier::Limited);
        assert_eq!(credential.credential_id, info.credential_id);
    }

    #[test]
    fn the_registry_file_records_no_secrets() {
        let (registry, path) = registry_with_managing("no-secrets", "pair-token-secret");
        let (_info, token) = registry.mint(Some("browser".into())).unwrap();
        // Authenticating is what writes the managing credential down.
        registry.authenticate("pair-token-secret").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();

        assert!(!content.contains(&token), "{content}");
        assert!(!content.contains("pair-token-secret"), "{content}");
        assert!(content.contains(&fingerprint(&token)), "{content}");
    }

    #[test]
    fn credentials_survive_a_restart() {
        let (registry, path) = registry_with_managing("restart", "pair-token");
        let (info, token) = registry.mint(Some("browser".into())).unwrap();

        // A new process opening the same file is a server restart.
        let restarted = SharedCredentialRegistry::open(path.clone());
        restarted.attach_managing_token(SharedWebSocketToken::new("pair-token".to_string()));

        let credential = restarted.authenticate(&token).expect("survives restart");
        assert_eq!(credential.credential_id, info.credential_id);
        assert_eq!(credential.tier, CredentialTier::Limited);
        let listed = restarted.list();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].tier, CredentialTier::Managing);
        assert_eq!(listed[1].credential_id, info.credential_id);
        assert_eq!(listed[1].label.as_deref(), Some("browser"));
    }

    #[test]
    fn rotating_the_managing_token_leaves_limited_credentials_standing() {
        let path = temp_registry_path("rotate");
        let registry = SharedCredentialRegistry::open(path);
        let managing = SharedWebSocketToken::new("first-token".to_string());
        registry.attach_managing_token(managing.clone());
        let (limited, limited_token) = registry.mint(Some("browser".into())).unwrap();
        let before = registry.authenticate("first-token").unwrap();

        // `herdr pair` rotates the stored token; a config reload applies it.
        managing
            .apply_reloaded_config(&crate::config::WebSocketApiConfig {
                bind: Some("127.0.0.1:4433".into()),
                token: Some("second-token".into()),
                ..Default::default()
            })
            .unwrap();

        assert!(
            registry.authenticate("first-token").is_none(),
            "the rotated-away token must stop authenticating"
        );
        let after = registry.authenticate("second-token").expect("new pairing");
        assert_eq!(after.tier, CredentialTier::Managing);
        assert_ne!(after.credential_id, before.credential_id);

        let survivor = registry.authenticate(&limited_token).expect("survives");
        assert_eq!(survivor.credential_id, limited.credential_id);
    }

    #[test]
    fn revoking_ends_one_credential_and_leaves_the_others() {
        let (registry, _path) = registry_with_managing("revoke", "pair-token");
        let (first, first_token) = registry.mint(None).unwrap();
        let (_second, second_token) = registry.mint(None).unwrap();

        assert!(registry.revoke_limited(&first.credential_id).unwrap());

        assert!(registry.authenticate(&first_token).is_none());
        assert!(registry.authenticate(&second_token).is_some());
        assert!(
            !registry.revoke_limited(&first.credential_id).unwrap(),
            "revoking a gone credential reports not-found, not success"
        );
    }

    #[test]
    fn revoke_all_ends_every_limited_credential_and_keeps_managing() {
        let (registry, _path) = registry_with_managing("revoke-all", "pair-token");
        let (_first, first_token) = registry.mint(None).unwrap();
        let (_second, second_token) = registry.mint(None).unwrap();

        let revoked = registry.revoke_all_limited().unwrap();

        assert_eq!(revoked.len(), 2);
        assert!(registry.authenticate(&first_token).is_none());
        assert!(registry.authenticate(&second_token).is_none());
        assert!(registry.authenticate("pair-token").is_some());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn last_seen_is_recorded_when_a_credential_is_used() {
        let (registry, _path) = registry_with_managing("last-seen", "pair-token");
        let (info, token) = registry.mint(None).unwrap();
        assert_eq!(info.last_seen_unix, None);

        registry.authenticate(&token).unwrap();

        let listed = registry.list();
        let seen = listed
            .iter()
            .find(|credential| credential.credential_id == info.credential_id)
            .and_then(|credential| credential.last_seen_unix)
            .expect("last seen recorded");
        assert!(
            seen >= info.created_at_unix,
            "{seen} < {}",
            info.created_at_unix
        );
    }

    #[test]
    fn mark_seen_reports_a_revoked_credential_as_gone() {
        let (registry, _path) = registry_with_managing("mark-seen", "pair-token");
        let (info, _token) = registry.mint(None).unwrap();

        assert_eq!(
            registry.mark_seen(&info.credential_id),
            Some(CredentialTier::Limited)
        );
        registry.revoke_limited(&info.credential_id).unwrap();
        assert_eq!(registry.mark_seen(&info.credential_id), None);
    }

    /// A browser cannot read a 401 (PROTOCOL-NOTES: a rejected handshake and
    /// a dead endpoint are the same opaque close), so the server has to be
    /// able to tell a credential it revoked from one that never existed.
    #[test]
    fn a_revoked_credential_stays_distinguishable_from_one_that_never_existed() {
        let (registry, _path) = registry_with_managing("tombstone", "pair-token");
        let (info, token) = registry.mint(Some("browser".into())).unwrap();

        registry.revoke_limited(&info.credential_id).unwrap();

        match registry.authenticate_handshake(&token) {
            HandshakeOutcome::Revoked { credential_id } => {
                assert_eq!(credential_id, info.credential_id)
            }
            other => panic!("a revoked credential must be recognized: {other:?}"),
        }
        assert!(matches!(
            registry.authenticate_handshake("never-minted-token"),
            HandshakeOutcome::Unknown
        ));
        // It is recognized, not honored: nothing about it is live.
        assert!(registry.authenticate(&token).is_none());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn revoke_all_tombstones_every_credential_it_ends() {
        let (registry, _path) = registry_with_managing("tombstone-all", "pair-token");
        let (_first, first_token) = registry.mint(None).unwrap();
        let (_second, second_token) = registry.mint(None).unwrap();

        registry.revoke_all_limited().unwrap();

        for token in [&first_token, &second_token] {
            assert!(matches!(
                registry.authenticate_handshake(token),
                HandshakeOutcome::Revoked { .. }
            ));
        }
    }

    /// Bulk revocation is the stolen-phone case, and every browser it logs
    /// out has to be able to find that out. A count cap that evicted recent
    /// tombstones would turn "log them all out" into "log them all out and
    /// tell none of them", leaving each retrying against a silent 401.
    #[test]
    fn revoking_far_more_credentials_than_the_old_cap_leaves_them_all_discoverable() {
        let path = temp_registry_path("tombstone-bulk");
        let registry = SharedCredentialRegistry::open(path.clone());
        registry.attach_managing_token(SharedWebSocketToken::new("pair-token".to_string()));

        let mut tokens = Vec::new();
        for _ in 0..70 {
            let (_info, token) = registry.mint(None).unwrap();
            tokens.push(token);
        }
        assert_eq!(registry.revoke_all_limited().unwrap().len(), 70);

        for (index, token) in tokens.iter().enumerate() {
            assert!(
                matches!(
                    registry.authenticate_handshake(token),
                    HandshakeOutcome::Revoked { .. }
                ),
                "credential {index} of a bulk revoke lost its tombstone"
            );
        }

        // ...and still after a restart, which is when the holder usually
        // comes back.
        let restarted = SharedCredentialRegistry::open(path);
        for token in &tokens {
            assert!(matches!(
                restarted.authenticate_handshake(token),
                HandshakeOutcome::Revoked { .. }
            ));
        }
    }

    /// The other way a FIFO cap loses a revocation: not one big batch, but
    /// a steady trickle of later ones behind it.
    #[test]
    fn later_revocations_do_not_push_out_an_earlier_one() {
        let (registry, _path) = registry_with_managing("tombstone-trickle", "pair-token");
        let (first, first_token) = registry
            .mint(Some("the browser that matters".into()))
            .unwrap();
        registry.revoke_limited(&first.credential_id).unwrap();

        for _ in 0..80 {
            let (info, _token) = registry.mint(None).unwrap();
            registry.revoke_limited(&info.credential_id).unwrap();
        }

        match registry.authenticate_handshake(&first_token) {
            HandshakeOutcome::Revoked { credential_id } => {
                assert_eq!(credential_id, first.credential_id)
            }
            other => panic!("an earlier revocation was pushed out: {other:?}"),
        }
    }

    #[test]
    fn tombstones_past_the_ttl_are_evicted_and_recent_ones_are_kept() {
        let now = 10 * TOMBSTONE_TTL_SECS;
        let entry = |name: &str, revoked_at_unix: u64| RevokedCredential {
            id: name.to_string(),
            fingerprint: fingerprint(name),
            revoked_at_unix,
        };
        let mut revoked = vec![
            entry("ancient", now - TOMBSTONE_TTL_SECS - 1),
            entry("just_expired", now - TOMBSTONE_TTL_SECS),
            entry("just_inside", now - TOMBSTONE_TTL_SECS + 1),
            entry("fresh", now),
        ];

        prune_tombstones(&mut revoked, now);

        let kept: Vec<&str> = revoked.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(kept, vec!["just_inside", "fresh"]);
    }

    /// The ceiling is a backstop against unbounded growth, not a working
    /// limit. If it is ever reached with everything recent, the newest are
    /// what survive — and [`prune_tombstones`] logs that it is dropping
    /// revocations rather than doing it quietly.
    #[test]
    fn the_tombstone_ceiling_keeps_the_newest_when_everything_is_recent() {
        let now = TOMBSTONE_TTL_SECS;
        let mut revoked: Vec<RevokedCredential> = (0..MAX_TOMBSTONES + 10)
            .map(|index| RevokedCredential {
                id: format!("cred_{index}"),
                fingerprint: fingerprint(&format!("token-{index}")),
                revoked_at_unix: now,
            })
            .collect();

        prune_tombstones(&mut revoked, now);

        assert_eq!(revoked.len(), MAX_TOMBSTONES);
        assert_eq!(
            revoked.first().map(|entry| entry.id.as_str()),
            Some("cred_10")
        );
        assert_eq!(
            revoked.last().map(|entry| entry.id.as_str()),
            Some(format!("cred_{}", MAX_TOMBSTONES + 9).as_str())
        );
    }

    /// A revoke that cannot be persisted must not half-happen: the on-disk
    /// grant would outlive the in-memory one and come back on restart, while
    /// the caller's retry would read `credential_not_found` as confirmation.
    #[cfg(unix)]
    #[test]
    fn a_revoke_that_cannot_be_saved_changes_nothing() {
        use std::os::unix::fs::PermissionsExt as _;

        let (registry, path) = registry_with_managing("revoke-io-fail", "pair-token");
        let (first, first_token) = registry.mint(Some("first".into())).unwrap();
        let (_second, second_token) = registry.mint(Some("second".into())).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let dir = path.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let single = registry.revoke_limited(&first.credential_id);
        let all = registry.revoke_all_limited();
        // Read while the directory is still unwritable, so nothing this test
        // does afterwards can be mistaken for the revoke's own writes.
        let after = std::fs::read_to_string(&path).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(single.is_err(), "an unwritable registry must fail loudly");
        assert!(all.is_err(), "{all:?}");
        assert_eq!(after, before, "a failed revoke must not touch the file");
        // Nothing moved in memory either: both credentials still
        // authenticate and neither was tombstoned.
        assert!(registry.authenticate(&first_token).is_some());
        assert!(registry.authenticate(&second_token).is_some());
        assert!(matches!(
            registry.authenticate_handshake(&first_token),
            HandshakeOutcome::Live(_)
        ));
        assert_eq!(registry.list().len(), 3);

        // And once the disk is writable again the revoke goes through.
        assert!(registry.revoke_limited(&first.credential_id).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_mint_that_cannot_be_saved_leaves_no_live_credential() {
        use std::os::unix::fs::PermissionsExt as _;

        let (registry, path) = registry_with_managing("mint-io-fail", "pair-token");
        registry.mint(None).unwrap();
        let dir = path.parent().unwrap();

        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let minted = registry.mint(Some("doomed".into()));
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(minted.is_err(), "an unwritable registry must fail loudly");
        assert_eq!(
            registry.list().len(),
            2,
            "a mint that was never stored must not be live"
        );
    }

    /// A temporary downgrade must not eat the newer server's registry: the
    /// file is left exactly as found and every write is refused.
    #[test]
    fn a_registry_from_a_newer_herdr_is_never_overwritten() {
        let path = temp_registry_path("newer-format");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let newer = serde_json::json!({
            "version": REGISTRY_VERSION + 7,
            "limited": [{
                "id": "cred_from_the_future",
                "fingerprint": fingerprint("future-token"),
                "created_at_unix": 1,
                "unknown_future_field": true,
            }],
        })
        .to_string();
        std::fs::write(&path, &newer).unwrap();

        let registry = SharedCredentialRegistry::open(path.clone());
        registry.attach_managing_token(SharedWebSocketToken::new("pair-token".to_string()));

        // The pairing still works — it authenticates from config, not here.
        assert!(registry.authenticate("pair-token").is_some());
        // Every write is refused, and the file is untouched.
        assert!(registry.mint(Some("nope".into())).is_err());
        assert!(registry.revoke_all_limited().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn an_unreadable_registry_starts_empty_instead_of_failing() {
        let path = temp_registry_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let registry = SharedCredentialRegistry::open(path);
        registry.attach_managing_token(SharedWebSocketToken::new("pair-token".to_string()));

        assert!(registry.authenticate("pair-token").is_some());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn labels_are_trimmed_and_bounded() {
        assert_eq!(normalize_label(None).unwrap(), None);
        assert_eq!(normalize_label(Some("   ")).unwrap(), None);
        assert_eq!(
            normalize_label(Some("  desk  ")).unwrap().as_deref(),
            Some("desk")
        );
        assert!(normalize_label(Some(&"x".repeat(MAX_LABEL_LEN + 1))).is_err());
        assert!(normalize_label(Some("bad\nlabel")).is_err());
    }
}
