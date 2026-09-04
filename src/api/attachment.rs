//! Server-scoped attachment storage for the `attachment.*` methods.
//!
//! Bytes arrive base64-encoded, are validated (size cap, and a magic-byte
//! sniff whenever the client names no file), and land atomically in a
//! herdr-owned scratch directory with owner-only permissions and
//! whitespace-free names. A file beyond one message's carry arrives instead
//! through `attachment.begin` / `attachment.append` / `attachment.commit`,
//! whose whole state is the reserved partial file on disk — no upload table,
//! so a restart forgets nothing it needs and an abandoned upload is swept
//! like any other scratch file. A TTL sweep runs at listener start and hourly
//! so the scratch dir never becomes long-term storage. The whole path is pure
//! file I/O on the connection thread: no app state, so behavior is identical
//! over every transport.

use std::fs;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

use crate::api::schema::{
    AttachmentAppendParams, AttachmentBeginParams, AttachmentCommitParams, AttachmentCreateParams,
    FileAttachmentsCapability, ResponseResult, SuccessResponse,
};

pub(crate) const ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Room reserved out of the message cap for the request JSON around the
/// payload (id, method, params key) — generous on purpose.
const REQUEST_ENVELOPE_HEADROOM_BYTES: usize = 4096;

/// Decoded size cap, derived so that a max-size attachment still fits the
/// unchanged 1 MiB message cap after base64's 4/3 expansion plus the request
/// envelope. The headroom keeps a real band of payloads that are over this
/// cap yet under the transport cap, so an oversize upload reaches the method
/// and earns its distinct `attachment_too_large` error in-band instead of
/// always dying as a framing-level connection drop.
pub(crate) const MAX_ATTACHMENT_BYTES: usize =
    (super::server::MAX_INITIAL_REQUEST_BYTES - REQUEST_ENVELOPE_HEADROOM_BYTES) / 4 * 3;

/// Ceiling on a chunked upload's declared size. A policy number, not a
/// format limit: the scratch dir holds these for the whole TTL, and on a box
/// whose temp dir is a memory filesystem that is RAM.
const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Free space `attachment.begin` demands beyond the declared size, so an
/// upload that would leave the volume with nothing to spare is refused
/// before its first byte rather than after filling it.
const FREE_SPACE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// Longest sanitized client name carried into a stored file's name.
const MAX_SAFE_NAME_CHARS: usize = 80;

/// And its budget in bytes, because a filesystem counts a name in those. A
/// name whose letters are three bytes each would otherwise pass the
/// character cap and still be too long to create, leaving a perfectly
/// ordinary filename refused as a storage failure.
const MAX_SAFE_NAME_BYTES: usize = 180;

/// Longest `upload_id` the server accepts back, matching the contract.
const MAX_UPLOAD_ID_CHARS: usize = 128;

/// Every minted `upload_id` starts here. Requiring it on the way back keeps
/// an echoed id from ever naming a `.partial` some other code path staged —
/// the id only ever resolves to an upload this module reserved.
const UPLOAD_ID_PREFIX: &str = "upload-";

/// Suffix of the file holding an upload's bytes while it is in flight.
const PARTIAL_SUFFIX: &str = ".partial";

/// Suffix of the file holding what `begin` was told: the sanitized name the
/// finished file will wear and the size it declared. It lives beside the
/// partial because the partial is the only upload state there is, and an
/// `upload_id` restricted to the contract's ASCII charset cannot itself
/// carry a name whose letters may be anything Unicode calls a letter.
const RECORD_SUFFIX: &str = ".upload";

/// What this server accepts as a file attachment, for the pong capabilities.
/// `chunk_bytes` is the message cap's own derived limit rather than a second
/// number to keep in step with it.
pub(crate) fn file_attachments_capability() -> FileAttachmentsCapability {
    FileAttachmentsCapability {
        max_bytes: MAX_UPLOAD_BYTES,
        chunk_bytes: MAX_ATTACHMENT_BYTES as u64,
    }
}

#[derive(Debug)]
pub(super) enum AttachmentError {
    InvalidBase64(String),
    TooLarge { size: u64, limit: u64 },
    UnsupportedFormat,
    UnknownUpload,
    OffsetMismatch { received: u64 },
    SizeMismatch { received: u64, declared: u64 },
    Storage(io::Error),
}

impl AttachmentError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            AttachmentError::InvalidBase64(_) => "invalid_params",
            AttachmentError::TooLarge { .. } => "attachment_too_large",
            AttachmentError::UnsupportedFormat => "attachment_unsupported_format",
            AttachmentError::UnknownUpload => "attachment_unknown_upload",
            AttachmentError::OffsetMismatch { .. } => "attachment_offset_mismatch",
            AttachmentError::SizeMismatch { .. } => "attachment_size_mismatch",
            AttachmentError::Storage(_) => "attachment_storage_failed",
        }
    }

    pub(super) fn message(&self) -> String {
        match self {
            AttachmentError::InvalidBase64(err) => {
                format!("bytes_b64 is not valid base64: {err}")
            }
            AttachmentError::TooLarge { size, limit } => {
                format!("attachment is {size} bytes; the limit is {limit} bytes")
            }
            AttachmentError::UnsupportedFormat => {
                "attachment bytes are not a recognized image format (JPEG, PNG, or WebP)".into()
            }
            AttachmentError::UnknownUpload => {
                "no upload is in progress under that upload_id".into()
            }
            AttachmentError::OffsetMismatch { received } => {
                format!("offset must equal the bytes already stored; received {received} bytes")
            }
            AttachmentError::SizeMismatch { received, declared } => {
                format!("upload holds {received} bytes; commit declared {declared} bytes")
            }
            AttachmentError::Storage(err) => format!("failed to store attachment: {err}"),
        }
    }
}

pub(super) struct CreatedAttachment {
    pub(super) path: PathBuf,
    pub(super) expires_at: u64,
}

/// Handle one `attachment.create` request end to end, returning the encoded
/// response. Called from `handle_request`, so both transports share it.
pub(super) fn handle_create(id: String, params: &AttachmentCreateParams) -> String {
    respond(id, create_attachment(params).map(created_result))
}

/// Handle one `attachment.begin` request: reserve a partial for a file that
/// does not fit a single message, and mint the id that names it.
pub(super) fn handle_begin(id: String, params: &AttachmentBeginParams) -> String {
    respond(
        id,
        begin_upload(params).map(|upload_id| ResponseResult::AttachmentUploadStarted { upload_id }),
    )
}

/// Handle one `attachment.append` request: write a chunk onto the partial at
/// the offset the client believes it reached.
pub(super) fn handle_append(id: String, params: &AttachmentAppendParams) -> String {
    respond(
        id,
        append_upload(params).map(|received| ResponseResult::AttachmentAppended { received }),
    )
}

/// Handle one `attachment.commit` request: rename the finished partial into
/// place, answering exactly as `attachment.create` does.
pub(super) fn handle_commit(id: String, params: &AttachmentCommitParams) -> String {
    respond(id, commit_upload(params).map(created_result))
}

fn created_result(created: CreatedAttachment) -> ResponseResult {
    ResponseResult::AttachmentCreated {
        path: created.path.to_string_lossy().into_owned(),
        expires_at: created.expires_at,
    }
}

fn respond(id: String, result: Result<ResponseResult, AttachmentError>) -> String {
    match result {
        Ok(result) => serde_json::to_string(&SuccessResponse { id, result }).unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
                .to_string()
        }),
        Err(err) => super::server::error_response_json(id, err.code(), err.message()),
    }
}

fn create_attachment(
    params: &AttachmentCreateParams,
) -> Result<CreatedAttachment, AttachmentError> {
    let dir = scratch_dir().map_err(AttachmentError::Storage)?;
    create_attachment_in(&dir, params)
}

fn begin_upload(params: &AttachmentBeginParams) -> Result<String, AttachmentError> {
    let dir = scratch_dir().map_err(AttachmentError::Storage)?;
    begin_upload_in(&dir, params)
}

fn append_upload(params: &AttachmentAppendParams) -> Result<u64, AttachmentError> {
    let dir = scratch_dir().map_err(AttachmentError::Storage)?;
    append_upload_in(&dir, params)
}

fn commit_upload(params: &AttachmentCommitParams) -> Result<CreatedAttachment, AttachmentError> {
    let dir = scratch_dir().map_err(AttachmentError::Storage)?;
    commit_upload_in(&dir, params)
}

/// Validation order per the contract: decode base64, enforce the size cap,
/// sniff magic bytes, then write. A rejection or write failure leaves no
/// file and no temp artifact behind.
///
/// The sniff stays mandatory exactly when the client names no file: that is
/// the photo contract, unchanged. A named file is stored verbatim, and only
/// its extension answers to the sniff.
fn create_attachment_in(
    dir: &Path,
    params: &AttachmentCreateParams,
) -> Result<CreatedAttachment, AttachmentError> {
    let bytes = decode_payload(&params.bytes_b64)?;

    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            size: bytes.len() as u64,
            limit: MAX_ATTACHMENT_BYTES as u64,
        });
    }

    let sniffed = sniff_image_extension(&bytes);
    let safe = match params.name.as_deref() {
        None => {
            if sniffed.is_none() {
                return Err(AttachmentError::UnsupportedFormat);
            }
            None
        }
        Some(name) => Some(sanitize_name(name)),
    };

    ensure_scratch_dir(dir).map_err(AttachmentError::Storage)?;
    let name = FinalName::new(unique_seed(), safe.as_deref(), sniffed);
    write_atomically(dir, &name, &bytes).map_err(AttachmentError::Storage)
}

/// Reserve the partial a chunked upload writes into, refusing anything the
/// ceiling or the volume cannot take before a byte is written.
fn begin_upload_in(dir: &Path, params: &AttachmentBeginParams) -> Result<String, AttachmentError> {
    begin_upload_in_with_free_space(dir, params, crate::platform::available_bytes_on_volume)
}

fn begin_upload_in_with_free_space(
    dir: &Path,
    params: &AttachmentBeginParams,
    available_bytes: impl Fn(&Path) -> io::Result<u64>,
) -> Result<String, AttachmentError> {
    if params.size > MAX_UPLOAD_BYTES {
        return Err(AttachmentError::TooLarge {
            size: params.size,
            limit: MAX_UPLOAD_BYTES,
        });
    }

    ensure_scratch_dir(dir).map_err(AttachmentError::Storage)?;

    let required = params.size.saturating_add(FREE_SPACE_MARGIN_BYTES);
    let available = available_bytes(dir).map_err(AttachmentError::Storage)?;
    if available < required {
        return Err(AttachmentError::Storage(io::Error::other(format!(
            "the attachment scratch volume has {available} bytes free; \
             this upload needs {required} bytes including its margin"
        ))));
    }

    let record = UploadRecord {
        name: sanitize_name(&params.name),
        size: params.size,
    };
    reserve_upload(dir, unique_seed(), &record).map_err(AttachmentError::Storage)
}

/// Write one chunk onto a reserved partial. The partial's own size is the
/// upload's state: an offset that does not match it is refused rather than
/// reconciled, so a duplicated or reordered append can never corrupt a file.
fn append_upload_in(dir: &Path, params: &AttachmentAppendParams) -> Result<u64, AttachmentError> {
    let upload_id = validated_upload_id(&params.upload_id).ok_or(AttachmentError::UnknownUpload)?;
    let bytes = decode_payload(&params.bytes_b64)?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            size: bytes.len() as u64,
            limit: MAX_ATTACHMENT_BYTES as u64,
        });
    }

    ensure_scratch_dir(dir).map_err(AttachmentError::Storage)?;
    let record = read_upload_record(dir, upload_id)?;
    let partial_path = dir.join(format!("{upload_id}{PARTIAL_SUFFIX}"));
    let received = partial_size(&partial_path)?;

    if params.offset != received {
        return Err(AttachmentError::OffsetMismatch { received });
    }
    let after = received.saturating_add(bytes.len() as u64);
    if after > record.size {
        return Err(AttachmentError::TooLarge {
            size: after,
            limit: record.size,
        });
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&partial_path)
        .map_err(AttachmentError::Storage)?;
    file.seek(SeekFrom::Start(received))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|()| file.flush())
        .map_err(AttachmentError::Storage)?;
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(AttachmentError::Storage)
}

/// Rename a finished partial into its final name, which is where the client
/// name and the sniffed extension meet. A size that does not match leaves
/// the partial exactly as it was, so the client can keep appending.
fn commit_upload_in(
    dir: &Path,
    params: &AttachmentCommitParams,
) -> Result<CreatedAttachment, AttachmentError> {
    let upload_id = validated_upload_id(&params.upload_id).ok_or(AttachmentError::UnknownUpload)?;

    ensure_scratch_dir(dir).map_err(AttachmentError::Storage)?;
    let record = read_upload_record(dir, upload_id)?;
    let partial_path = dir.join(format!("{upload_id}{PARTIAL_SUFFIX}"));
    let received = partial_size(&partial_path)?;

    if received != params.size {
        return Err(AttachmentError::SizeMismatch {
            received,
            declared: params.size,
        });
    }

    let sniffed = sniff_stored_image_extension(&partial_path).map_err(AttachmentError::Storage)?;
    let name = FinalName::new(unique_seed(), Some(&record.name), sniffed);
    let created = place_partial(dir, &name, &partial_path).map_err(AttachmentError::Storage)?;

    let record_path = dir.join(format!("{upload_id}{RECORD_SUFFIX}"));
    if let Err(err) = fs::remove_file(&record_path) {
        debug!(path = %record_path.display(), err = %err, "failed to remove a committed upload record");
    }
    Ok(created)
}

fn decode_payload(bytes_b64: &str) -> Result<Vec<u8>, AttachmentError> {
    base64::engine::general_purpose::STANDARD
        .decode(bytes_b64.trim())
        .map_err(|err| AttachmentError::InvalidBase64(err.to_string()))
}

/// The bytes a reserved partial already holds. A partial that is missing —
/// or is anything but a plain file — is an unknown upload, never a path the
/// server follows.
fn partial_size(partial_path: &Path) -> Result<u64, AttachmentError> {
    match fs::symlink_metadata(partial_path) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => {
            warn!(path = %partial_path.display(), "refusing an upload path that is not a plain file");
            Err(AttachmentError::UnknownUpload)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AttachmentError::UnknownUpload),
        Err(err) => Err(AttachmentError::Storage(err)),
    }
}

/// What `begin` recorded for one upload. Written next to the partial, read
/// back on every append and commit: the disk is the whole upload table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UploadRecord {
    name: String,
    size: u64,
}

fn read_upload_record(dir: &Path, upload_id: &str) -> Result<UploadRecord, AttachmentError> {
    let path = dir.join(format!("{upload_id}{RECORD_SUFFIX}"));
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(AttachmentError::UnknownUpload)
        }
        Err(err) => return Err(AttachmentError::Storage(err)),
    };
    serde_json::from_slice(&raw).map_err(|err| {
        warn!(path = %path.display(), err = %err, "unreadable upload record");
        AttachmentError::UnknownUpload
    })
}

/// Reserve an id, its empty partial, and its record together, so a returned
/// id always names an upload every later call can find.
fn reserve_upload(dir: &Path, unique: u128, record: &UploadRecord) -> io::Result<String> {
    let encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    for attempt in 0..100 {
        let upload_id = format!("{UPLOAD_ID_PREFIX}{unique}-{attempt}");

        let partial_path = dir.join(format!("{upload_id}{PARTIAL_SUFFIX}"));
        match open_exclusive(&partial_path) {
            Ok(partial) => drop(partial),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }

        let record_path = dir.join(format!("{upload_id}{RECORD_SUFFIX}"));
        let written = open_exclusive(&record_path).and_then(|mut file| {
            file.write_all(&encoded)?;
            file.flush()
        });
        if let Err(err) = written {
            let _ = fs::remove_file(&record_path);
            let _ = fs::remove_file(&partial_path);
            if err.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(err);
        }

        return Ok(upload_id);
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a unique upload id",
    ))
}

/// The id back, only when this server could have minted it. Pure string
/// work: an id that fails here never reaches the filesystem at all. The
/// charset excludes both separators, and the mandatory prefix rules out the
/// `.` and `..` names, so no id can name anything but a file in the scratch
/// dir itself.
fn validated_upload_id(upload_id: &str) -> Option<&str> {
    if upload_id.len() > MAX_UPLOAD_ID_CHARS || !upload_id.starts_with(UPLOAD_ID_PREFIX) {
        return None;
    }
    upload_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        .then_some(upload_id)
}

/// The client's name, cut down to what a stored file may wear: its basename
/// only, NFC so a decomposed accent stays one letter, no leading dot, every
/// letter and digit any language writes kept, everything else folded to a
/// single `_`, and short enough to leave the name readable.
fn sanitize_name(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let normalized: String = basename.nfc().collect();

    let mut safe = String::with_capacity(normalized.len());
    for character in normalized.trim_start_matches('.').chars() {
        let kept = character.is_alphanumeric() || matches!(character, '.' | '_' | '-');
        let mapped = if kept { character } else { '_' };
        if mapped == '_' && safe.ends_with('_') {
            continue;
        }
        safe.push(mapped);
    }

    // A trailing dot is not a name every filesystem can open, and no
    // extension hides behind one.
    let capped = cap_name_length(safe.trim_end_matches('.'));
    if capped.is_empty() {
        "file".to_string()
    } else {
        capped
    }
}

/// Shorten a long name from the stem, so the extension an agent reads the
/// file by survives.
fn cap_name_length(safe: &str) -> String {
    if safe.chars().count() <= MAX_SAFE_NAME_CHARS && safe.len() <= MAX_SAFE_NAME_BYTES {
        return safe.to_string();
    }

    let (stem, extension) = split_extension(safe);
    let suffix = extension.map(|ext| format!(".{ext}")).unwrap_or_default();
    let head = trimmed_to_fit(
        stem,
        MAX_SAFE_NAME_CHARS.saturating_sub(suffix.chars().count()),
        MAX_SAFE_NAME_BYTES.saturating_sub(suffix.len()),
    );
    if head.is_empty() {
        // An extension long enough to fill the whole budget is no longer an
        // extension worth keeping whole.
        return trimmed_to_fit(safe, MAX_SAFE_NAME_CHARS, MAX_SAFE_NAME_BYTES);
    }
    format!("{head}{suffix}")
}

/// The longest prefix of `text` inside both budgets, cut where a character
/// ends so the name stays the text it was.
fn trimmed_to_fit(text: &str, max_chars: usize, max_bytes: usize) -> String {
    let mut fitted = String::new();
    for (index, character) in text.chars().enumerate() {
        if index >= max_chars || fitted.len() + character.len_utf8() > max_bytes {
            break;
        }
        fitted.push(character);
    }
    fitted
}

/// Split a name at its extension. A dot that opens the name or closes it is
/// no extension.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => {
            (&name[..index], Some(&name[index + 1..]))
        }
        _ => (name, None),
    }
}

/// How a finished attachment is named: a stem no other file can hold, then
/// the sanitized client name when there is one, then the extension.
struct FinalName {
    unique: u128,
    safe: Option<String>,
    extension: Option<String>,
}

impl FinalName {
    /// The naming rule in one place: sniffed image bytes always dictate the
    /// extension, so a client can never make a PNG into a `.txt`; otherwise
    /// the client name's own extension — or none — stands.
    fn new(unique: u128, safe: Option<&str>, sniffed: Option<&'static str>) -> Self {
        match (safe, sniffed) {
            (None, sniffed) => FinalName {
                unique,
                safe: None,
                extension: sniffed.map(str::to_string),
            },
            (Some(safe), Some(sniffed)) => FinalName {
                unique,
                safe: Some(split_extension(safe).0.to_string()),
                extension: Some(sniffed.to_string()),
            },
            (Some(safe), None) => FinalName {
                unique,
                safe: Some(safe.to_string()),
                extension: None,
            },
        }
    }

    fn basename(&self, attempt: usize) -> String {
        let mut basename = format!("attachment-{}-{attempt}", self.unique);
        if let Some(safe) = &self.safe {
            basename.push('-');
            basename.push_str(safe);
        }
        if let Some(extension) = &self.extension {
            basename.push('.');
            basename.push_str(extension);
        }
        basename
    }
}

fn unique_seed() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Write under a temporary name, then rename into place. The rename happens
/// before the success response is encoded, so a returned path never names a
/// partial file; any failure removes the temp artifact.
fn write_atomically(dir: &Path, name: &FinalName, bytes: &[u8]) -> io::Result<CreatedAttachment> {
    place(dir, name, |staged_path| {
        let mut file = open_exclusive(staged_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        Ok(())
    })
}

/// Move a committed upload's partial onto its final name. The bytes are
/// already on disk, so staging is a rename rather than a write; everything
/// else — the reservation, the retry, the failure cleanup — is what any
/// other attachment gets.
fn place_partial(
    dir: &Path,
    name: &FinalName,
    partial_path: &Path,
) -> io::Result<CreatedAttachment> {
    place(dir, name, |staged_path| {
        fs::rename(partial_path, staged_path)
    })
}

/// The final name is reserved exclusively (`create_new`) before the bytes
/// are staged, so two attachments that share a timestamp stem can never
/// receive the same path, and a path already handed to one client is never
/// silently replaced by a later upload — the rename only ever lands on this
/// call's own zero-byte reservation.
fn place(
    dir: &Path,
    name: &FinalName,
    stage: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<CreatedAttachment> {
    for attempt in 0..100 {
        let basename = name.basename(attempt);

        let path = dir.join(&basename);
        match open_exclusive(&path) {
            Ok(reservation) => drop(reservation),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }

        let staged_path = dir.join(format!("{basename}{PARTIAL_SUFFIX}"));
        if let Err(err) = stage(&staged_path) {
            let _ = fs::remove_file(&path);
            if err.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(err);
        }

        if let Err(err) = fs::rename(&staged_path, &path) {
            let _ = fs::remove_file(&staged_path);
            let _ = fs::remove_file(&path);
            return Err(err);
        }

        // Expiry derives from the same clock the sweep reads (file mtime),
        // so the response's expires_at is the sweep's actual deadline.
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or_else(|_| SystemTime::now());
        let expires_at = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_add(ATTACHMENT_TTL)
            .as_secs();

        return Ok(CreatedAttachment { path, expires_at });
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a unique attachment path",
    ))
}

/// The extension the magic bytes dictate — never a client-declared one.
fn sniff_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// The same sniff for a file already on disk, which only ever needs its
/// head: the whole upload is never read back into memory.
fn sniff_stored_image_extension(path: &Path) -> io::Result<Option<&'static str>> {
    let mut head = [0_u8; 12];
    let mut file = fs::File::open(path)?;
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(sniff_image_extension(&head[..filled]))
}

fn scratch_dir() -> io::Result<PathBuf> {
    scratch_dir_under(&std::env::temp_dir())
}

/// The scratch dir under a given temp root, holding the path half of the
/// contract: the returned dir — and so every `AttachmentCreated.path` under
/// it — is absolute and whitespace-free, safe to paste unquoted.
fn scratch_dir_under(root: &Path) -> io::Result<PathBuf> {
    // Canonicalize so a relative TMPDIR (or one behind a symlink, like
    // macOS /tmp) still yields the absolute path the contract promises.
    let root = fs::canonicalize(root).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot resolve temp dir {}: {err}", root.display()),
        )
    })?;

    // Unix temp dirs are shared across users, so the euid keeps scratch dirs
    // apart; Windows %TEMP% is already per-user, and a stable name lets the
    // hourly sweeper find files from earlier server runs.
    #[cfg(unix)]
    let name = format!("herdr-attachments-{}", unsafe { libc::geteuid() });
    #[cfg(windows)]
    let name = "herdr-attachments".to_string();
    let dir = root.join(name);

    if dir.to_string_lossy().contains(char::is_whitespace) {
        return Err(io::Error::other(format!(
            "attachment scratch dir {} contains whitespace, so returned paths \
             would not be paste-safe; point the temp dir at a whitespace-free \
             location",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Refuse a scratch path that is not a plain directory owned by herdr's own
/// user. `metadata` must come from `symlink_metadata`, so a pre-planted
/// symlink at the predictable path shows up as a symlink (not a directory)
/// and is refused — otherwise a hostile local user could redirect the chmod,
/// the photo writes, and the sweep's deletions anywhere they like.
fn verify_scratch_dir(dir: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "attachment scratch path is not a directory: {}",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid {
            return Err(io::Error::other(format!(
                "attachment scratch dir at {} is owned by uid {}, not {euid}; refusing to use it",
                dir.display(),
                metadata.uid(),
            )));
        }
    }
    Ok(())
}

fn ensure_scratch_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    verify_scratch_dir(dir, &fs::symlink_metadata(dir)?)?;
    restrict_dir_permissions(dir)
}

/// Create a brand-new owner-only file, failing if the name is taken.
fn open_exclusive(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    restrict_file_options(&mut options);
    options.open(path)
}

#[cfg(unix)]
fn restrict_file_options(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(windows)]
fn restrict_file_options(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn restrict_dir_permissions(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn restrict_dir_permissions(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Start the TTL sweeper: one sweep now, then hourly. Called from every
/// listener start; the guard makes repeated starts share one sweeper.
pub(crate) fn spawn_ttl_sweeper() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            match scratch_dir() {
                Ok(dir) => sweep_expired(&dir),
                Err(err) => debug!(err = %err, "attachment sweep skipped"),
            }
            std::thread::sleep(SWEEP_INTERVAL);
        });
    });
}

/// Remove scratch-dir files older than the TTL by mtime. Only files directly
/// inside the scratch dir are touched — never anything outside it: the path
/// passes the same symlink/ownership guard as writes before it is read.
fn sweep_expired(dir: &Path) {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            debug!(dir = %dir.display(), err = %err, "attachment sweep skipped");
            return;
        }
    };
    if let Err(err) = verify_scratch_dir(dir, &metadata) {
        warn!(err = %err, "attachment sweep refused an untrusted scratch path");
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            debug!(dir = %dir.display(), err = %err, "attachment sweep skipped");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > ATTACHMENT_TTL {
            if let Err(err) = fs::remove_file(&path) {
                warn!(path = %path.display(), err = %err, "failed to sweep expired attachment");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "herdr-attachment-test-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// The photo contract's shape: bytes with no client name.
    fn photo(bytes_b64: &str) -> AttachmentCreateParams {
        AttachmentCreateParams {
            bytes_b64: bytes_b64.to_string(),
            name: None,
        }
    }

    fn named(bytes_b64: &str, name: &str) -> AttachmentCreateParams {
        AttachmentCreateParams {
            bytes_b64: bytes_b64.to_string(),
            name: Some(name.to_string()),
        }
    }

    fn png_bytes() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(b"herdr-test-image-payload");
        bytes
    }

    fn jpeg_bytes() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend_from_slice(b"herdr-test-image-payload");
        bytes
    }

    fn webp_bytes() -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"herdr-test-image-payload");
        bytes
    }

    fn dir_entries(dir: &Path) -> Vec<PathBuf> {
        match fs::read_dir(dir) {
            Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn sniffs_only_the_supported_image_formats() {
        assert_eq!(sniff_image_extension(&jpeg_bytes()), Some("jpg"));
        assert_eq!(sniff_image_extension(&png_bytes()), Some("png"));
        assert_eq!(sniff_image_extension(&webp_bytes()), Some("webp"));

        assert_eq!(sniff_image_extension(b"GIF89a...."), None);
        assert_eq!(sniff_image_extension(b"plain text"), None);
        assert_eq!(sniff_image_extension(b""), None);
        // RIFF container that is not WebP (e.g. WAVE audio).
        assert_eq!(sniff_image_extension(b"RIFF\x24\x00\x00\x00WAVEdata"), None);
        // A RIFF prefix too short to carry the WEBP tag.
        assert_eq!(sniff_image_extension(b"RIFF\x24\x00"), None);
    }

    #[test]
    fn creates_the_file_with_matching_bytes_and_owner_only_permissions() {
        let dir = unique_test_dir("happy");
        let bytes = png_bytes();

        let created = create_attachment_in(&dir, &photo(&encode(&bytes))).unwrap_or_else(|err| {
            panic!("attachment must be created: {}", err.message());
        });

        assert!(created.path.is_absolute());
        assert_eq!(created.path.parent(), Some(dir.as_path()));
        assert_eq!(fs::read(&created.path).unwrap(), bytes);

        let name = created.path.file_name().unwrap().to_string_lossy();
        assert!(!name.contains(' '), "name must be space-free: {name}");
        assert!(
            name.ends_with(".png"),
            "extension follows magic bytes: {name}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&created.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        // No temp artifact stays behind after success.
        assert_eq!(dir_entries(&dir), vec![created.path.clone()]);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ttl = ATTACHMENT_TTL.as_secs();
        assert!(
            created.expires_at >= now + ttl - 60 && created.expires_at <= now + ttl + 60,
            "expires_at must be about now + TTL, got {}",
            created.expires_at
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_follows_magic_bytes_for_every_supported_format() {
        let dir = unique_test_dir("formats");
        for (bytes, extension) in [
            (jpeg_bytes(), "jpg"),
            (png_bytes(), "png"),
            (webp_bytes(), "webp"),
        ] {
            let created =
                create_attachment_in(&dir, &photo(&encode(&bytes))).unwrap_or_else(|err| {
                    panic!("{extension} attachment must be created: {}", err.message());
                });
            assert_eq!(
                created.path.extension().and_then(|ext| ext.to_str()),
                Some(extension)
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejections_leave_no_file_or_temp_artifact() {
        let dir = unique_test_dir("reject");

        let malformed = create_attachment_in(&dir, &photo("not base64!!"));
        assert!(matches!(malformed, Err(AttachmentError::InvalidBase64(_))));

        let mut oversize = png_bytes();
        oversize.resize(MAX_ATTACHMENT_BYTES + 1, 0);
        let too_large = create_attachment_in(&dir, &photo(&encode(&oversize)));
        match too_large {
            Err(AttachmentError::TooLarge { size, limit }) => {
                assert_eq!(size, MAX_ATTACHMENT_BYTES as u64 + 1);
                assert_eq!(limit, MAX_ATTACHMENT_BYTES as u64);
            }
            _ => panic!("oversize payload must be rejected as too large"),
        }

        let unsupported =
            create_attachment_in(&dir, &photo(&encode(b"GIF89a not an image we accept")));
        assert!(matches!(
            unsupported,
            Err(AttachmentError::UnsupportedFormat)
        ));

        assert_eq!(dir_entries(&dir), Vec::<PathBuf>::new());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_size_requests_fit_the_transport_and_an_in_band_oversize_band_exists() {
        let envelope =
            r#"{"id":"req_attach_max","method":"attachment.create","params":{"bytes_b64":""}}"#
                .len();
        let encoded_len = |decoded: usize| decoded.div_ceil(3) * 4;

        // A payload at the cap must ride the unchanged 1 MiB message cap.
        assert!(
            encoded_len(MAX_ATTACHMENT_BYTES) + envelope
                <= crate::api::server::MAX_INITIAL_REQUEST_BYTES,
            "a max-size attachment request must fit the transport cap"
        );
        // And one just past the cap must fit too, so attachment_too_large is
        // reachable in-band rather than only ever a framing-level drop.
        assert!(
            encoded_len(MAX_ATTACHMENT_BYTES + 1) + envelope
                <= crate::api::server::MAX_INITIAL_REQUEST_BYTES,
            "an over-cap payload must be deliverable to earn the distinct error"
        );
    }

    #[test]
    fn creates_sharing_a_timestamp_stem_get_distinct_paths_and_never_clobber() {
        let dir = unique_test_dir("stem-collision");
        fs::create_dir_all(&dir).unwrap();

        let name = FinalName::new(42, None, Some("png"));
        let first = write_atomically(&dir, &name, b"first").unwrap();
        let second = write_atomically(&dir, &name, b"second").unwrap();

        assert_ne!(
            first.path, second.path,
            "two creates must never receive the same path"
        );
        assert_eq!(
            fs::read(&first.path).unwrap(),
            b"first",
            "an already-returned path must never be silently replaced"
        );
        assert_eq!(fs::read(&second.path).unwrap(), b"second");
        // No reservation or partial artifacts stay behind.
        let mut entries = dir_entries(&dir);
        entries.sort();
        let mut expected = vec![first.path.clone(), second.path.clone()];
        expected.sort();
        assert_eq!(entries, expected);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_cap_admits_exactly_the_limit() {
        let dir = unique_test_dir("cap-edge");
        let mut at_limit = png_bytes();
        at_limit.resize(MAX_ATTACHMENT_BYTES, 0);
        let created = create_attachment_in(&dir, &photo(&encode(&at_limit)));
        assert!(created.is_ok(), "a payload at the cap must be accepted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_codes_are_distinct_per_failure() {
        assert_eq!(
            AttachmentError::InvalidBase64("bad".into()).code(),
            "invalid_params"
        );
        assert_eq!(
            AttachmentError::TooLarge { size: 1, limit: 0 }.code(),
            "attachment_too_large"
        );
        assert_eq!(
            AttachmentError::UnknownUpload.code(),
            "attachment_unknown_upload"
        );
        assert_eq!(
            AttachmentError::OffsetMismatch { received: 0 }.code(),
            "attachment_offset_mismatch"
        );
        assert_eq!(
            AttachmentError::SizeMismatch {
                received: 0,
                declared: 1,
            }
            .code(),
            "attachment_size_mismatch"
        );
        assert_eq!(
            AttachmentError::UnsupportedFormat.code(),
            "attachment_unsupported_format"
        );
        assert_eq!(
            AttachmentError::Storage(io::Error::other("disk full")).code(),
            "attachment_storage_failed"
        );
    }

    #[test]
    fn sweep_removes_expired_files_and_spares_fresh_ones() {
        let dir = unique_test_dir("sweep");
        fs::create_dir_all(&dir).unwrap();

        let expired = dir.join("attachment-1-0.png");
        fs::write(&expired, b"old").unwrap();
        let old_mtime = SystemTime::now() - (ATTACHMENT_TTL + Duration::from_secs(60 * 60));
        fs::File::options()
            .write(true)
            .open(&expired)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();

        let fresh = dir.join("attachment-2-0.png");
        fs::write(&fresh, b"new").unwrap();

        sweep_expired(&dir);

        assert!(!expired.exists(), "expired file must be swept");
        assert!(fresh.exists(), "fresh file must survive the sweep");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_tolerates_a_missing_scratch_dir() {
        sweep_expired(&unique_test_dir("sweep-missing"));
    }

    #[cfg(unix)]
    #[test]
    fn sweep_refuses_a_scratch_path_behind_a_pre_planted_symlink() {
        let base = unique_test_dir("sweep-symlink");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        let victim = target.join("victim.png");
        fs::write(&victim, b"precious").unwrap();
        // Old enough that a sweep which followed the link would delete it.
        fs::File::options()
            .write(true)
            .open(&victim)
            .unwrap()
            .set_modified(SystemTime::now() - (ATTACHMENT_TTL + Duration::from_secs(3600)))
            .unwrap();
        let link = base.join("scratch");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        sweep_expired(&link);

        assert!(
            victim.exists(),
            "the sweep must never follow a symlinked scratch path"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scratch_dir_under_a_relative_temp_root_is_absolute() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let name = format!("herdr-attachment-relroot-{}-{nanos}", std::process::id());
        let absolute = std::env::current_dir().unwrap().join("target").join(&name);
        fs::create_dir_all(&absolute).unwrap();
        let relative = Path::new("target").join(&name);
        assert!(relative.is_relative());

        let dir = scratch_dir_under(&relative).unwrap_or_else(|err| {
            panic!("a relative temp root must resolve: {err}");
        });

        assert!(
            dir.is_absolute(),
            "scratch dir must be absolute, got {}",
            dir.display()
        );
        assert!(dir.starts_with(fs::canonicalize(&absolute).unwrap()));

        let _ = fs::remove_dir_all(&absolute);
    }

    #[test]
    fn scratch_dir_under_a_whitespace_temp_root_is_refused() {
        let base = unique_test_dir("whitespace-root");
        let root = base.join("with space");
        fs::create_dir_all(&root).unwrap();

        let err = scratch_dir_under(&root).expect_err("a whitespace root breaks paste-safety");
        assert!(
            err.to_string().contains("whitespace"),
            "unexpected error: {err}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    // ---- Naming a client's file ----

    #[test]
    fn sanitizes_a_client_name_down_to_what_a_stored_file_may_wear() {
        // Directories never survive: only the basename is a name.
        assert_eq!(sanitize_name("/var/log/system/crash.log"), "crash.log");
        assert_eq!(
            sanitize_name(r"C:\Users\pat\Desktop\crash.log"),
            "crash.log"
        );
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");

        // Whitespace and punctuation fold to one underscore each run; the
        // letters, digits, dots, dashes and underscores stay.
        assert_eq!(
            sanitize_name("my report (final), v2.txt"),
            "my_report_final_v2.txt"
        );
        assert_eq!(sanitize_name("a___b"), "a_b");

        // Letters are letters in every language.
        assert_eq!(sanitize_name("崩溃日志.log"), "崩溃日志.log");
        assert_eq!(sanitize_name("отчёт.txt"), "отчёт.txt");

        // A leading dot is not a name a stored file wears.
        assert_eq!(sanitize_name(".bashrc"), "bashrc");
        assert_eq!(sanitize_name("...hidden.txt"), "hidden.txt");

        // Nothing usable left is still a name.
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name("/"), "file");
        assert_eq!(sanitize_name("."), "file");
        assert_eq!(sanitize_name(".."), "file");
    }

    #[test]
    fn a_decomposed_accent_survives_sanitizing_as_one_letter() {
        // What a filesystem that hands out decomposed names gives a client:
        // `e` plus a combining acute. Folding the mark away would spell the
        // name `re_sume`, so the name is normalized before it is filtered.
        let decomposed = "re\u{0301}sume\u{0301}.txt";
        assert!(decomposed.chars().count() > "résumé.txt".chars().count());
        assert_eq!(sanitize_name(decomposed), "résumé.txt");
    }

    #[test]
    fn an_over_long_name_is_shortened_from_the_stem_and_keeps_its_extension() {
        let long = format!("{}.tar.gz", "n".repeat(200));

        let safe = sanitize_name(&long);

        assert_eq!(safe.chars().count(), 80);
        assert!(safe.ends_with(".gz"), "the extension must survive: {safe}");
        assert!(safe.starts_with("nnn"));
    }

    #[test]
    fn a_long_name_of_wide_letters_still_names_a_file_a_filesystem_can_hold() {
        let dir = unique_test_dir("wide-name");
        // Every one of these letters is three bytes, so a cap counted only
        // in characters would ask for a name no filesystem would create.
        let long = format!("{}.log", "崩".repeat(100));

        let safe = sanitize_name(&long);
        assert!(safe.chars().count() <= MAX_SAFE_NAME_CHARS);
        assert!(safe.len() <= MAX_SAFE_NAME_BYTES, "{} bytes", safe.len());
        assert!(
            safe.starts_with("崩崩崩"),
            "the letters must survive: {safe}"
        );
        assert!(safe.ends_with(".log"), "the extension must survive: {safe}");

        let created = create_attachment_in(&dir, &named(&encode(b"wide"), &long))
            .unwrap_or_else(|err| panic!("a long wide name must still store: {}", err.message()));
        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.len() <= 255,
            "a stored name must fit what a filesystem takes: {} bytes",
            name.len()
        );
        assert_eq!(fs::read(&created.path).unwrap(), b"wide");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_attachment_stores_any_bytes_verbatim_under_its_own_name() {
        let dir = unique_test_dir("named");
        let bytes = b"herdr crash log\nline two\n".to_vec();

        let created = create_attachment_in(&dir, &named(&encode(&bytes), "my crash log.txt"))
            .unwrap_or_else(|err| panic!("a named attachment must be created: {}", err.message()));

        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with("attachment-") && name.ends_with("-my_crash_log.txt"),
            "unexpected name: {name}"
        );
        assert!(!name.contains(' '), "name must be space-free: {name}");
        assert_eq!(fs::read(&created.path).unwrap(), bytes);
        assert_eq!(dir_entries(&dir), vec![created.path.clone()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_attachment_with_no_extension_keeps_having_none() {
        let dir = unique_test_dir("named-bare");

        let created = create_attachment_in(&dir, &named(&encode(b"just text"), "NOTES"))
            .unwrap_or_else(|err| panic!("must be created: {}", err.message()));

        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.ends_with("-NOTES"), "unexpected name: {name}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniffed_image_bytes_overrule_a_misleading_client_extension() {
        let dir = unique_test_dir("misleading");

        let created = create_attachment_in(&dir, &named(&encode(&png_bytes()), "screenshot.txt"))
            .unwrap_or_else(|err| panic!("must be created: {}", err.message()));

        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.ends_with("-screenshot.png"),
            "the magic bytes must dictate the extension: {name}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_never_makes_an_unnamed_upload_skip_the_sniff() {
        let dir = unique_test_dir("photo-contract");

        // Same bytes, twice: refused without a name, stored with one. This
        // is the whole difference the `name` field makes.
        let text = encode(b"plain text, not an image");
        assert!(matches!(
            create_attachment_in(&dir, &photo(&text)),
            Err(AttachmentError::UnsupportedFormat)
        ));
        assert!(create_attachment_in(&dir, &named(&text, "notes.txt")).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- Chunked uploads ----

    fn begin_params(name: &str, size: u64) -> AttachmentBeginParams {
        AttachmentBeginParams {
            name: name.to_string(),
            size,
        }
    }

    fn append_params(upload_id: &str, offset: u64, bytes: &[u8]) -> AttachmentAppendParams {
        AttachmentAppendParams {
            upload_id: upload_id.to_string(),
            offset,
            bytes_b64: encode(bytes),
        }
    }

    fn commit_params(upload_id: &str, size: u64) -> AttachmentCommitParams {
        AttachmentCommitParams {
            upload_id: upload_id.to_string(),
            size,
        }
    }

    /// Free space is not something a test can arrange on a real volume, so
    /// only the figure the OS reports is stood in for.
    fn room_for_anything(_dir: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }

    fn begin_in(dir: &Path, name: &str, size: u64) -> Result<String, AttachmentError> {
        begin_upload_in_with_free_space(dir, &begin_params(name, size), room_for_anything)
    }

    #[test]
    fn a_chunked_upload_lands_the_whole_file_under_its_own_name() {
        let dir = unique_test_dir("chunked");
        let chunks: Vec<Vec<u8>> = (0..5u8)
            .map(|index| vec![index; 4096 + usize::from(index)])
            .collect();
        let whole: Vec<u8> = chunks.concat();

        let upload_id = begin_in(&dir, "logs/session output.log", whole.len() as u64).unwrap();
        assert!(
            upload_id.starts_with("upload-"),
            "unexpected upload id: {upload_id}"
        );

        let mut offset = 0u64;
        for chunk in &chunks {
            let received =
                append_upload_in(&dir, &append_params(&upload_id, offset, chunk)).unwrap();
            offset += chunk.len() as u64;
            assert_eq!(received, offset, "received must be the bytes on disk");
        }

        let created = commit_upload_in(&dir, &commit_params(&upload_id, whole.len() as u64))
            .unwrap_or_else(|err| panic!("commit must succeed: {}", err.message()));

        assert!(created.path.is_absolute());
        assert_eq!(created.path.parent(), Some(dir.as_path()));
        assert_eq!(fs::read(&created.path).unwrap(), whole);
        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.ends_with("-session_output.log"),
            "unexpected name: {name}"
        );
        assert!(!name.contains(' '), "name must be space-free: {name}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&created.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "a committed upload is owner-only like any file"
            );
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ttl = ATTACHMENT_TTL.as_secs();
        assert!(created.expires_at >= now + ttl - 60 && created.expires_at <= now + ttl + 60);

        // The partial and its record are gone: a finished upload leaves only
        // the file it produced.
        assert_eq!(dir_entries(&dir), vec![created.path.clone()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_committed_upload_sniffs_its_own_bytes_for_the_extension() {
        let dir = unique_test_dir("chunked-sniff");
        let bytes = png_bytes();

        let upload_id = begin_in(&dir, "not-really.txt", bytes.len() as u64).unwrap();
        append_upload_in(&dir, &append_params(&upload_id, 0, &bytes)).unwrap();
        let created =
            commit_upload_in(&dir, &commit_params(&upload_id, bytes.len() as u64)).unwrap();

        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.ends_with("-not-really.png"),
            "the stored bytes must dictate the extension: {name}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_refuses_a_size_over_the_ceiling_and_leaves_no_file() {
        let dir = unique_test_dir("ceiling");

        let refused = begin_in(&dir, "huge.bin", MAX_UPLOAD_BYTES + 1);

        match refused {
            Err(AttachmentError::TooLarge { size, limit }) => {
                assert_eq!(size, MAX_UPLOAD_BYTES + 1);
                assert_eq!(limit, MAX_UPLOAD_BYTES);
            }
            _ => panic!("an over-ceiling size must be refused as too large"),
        }
        assert_eq!(dir_entries(&dir), Vec::<PathBuf>::new());

        // And the size exactly at the ceiling is accepted.
        assert!(begin_in(&dir, "huge.bin", MAX_UPLOAD_BYTES).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_refuses_a_volume_without_room_and_leaves_no_file() {
        let dir = unique_test_dir("no-room");
        let size = 8 * 1024 * 1024;

        // One byte short of the size plus its margin.
        let cramped = |_dir: &Path| Ok(size + FREE_SPACE_MARGIN_BYTES - 1);
        let refused =
            begin_upload_in_with_free_space(&dir, &begin_params("big.log", size), cramped);
        assert_eq!(
            refused.err().map(|err| err.code()),
            Some("attachment_storage_failed")
        );
        assert_eq!(dir_entries(&dir), Vec::<PathBuf>::new());

        // A volume that cannot even be measured is not one to write onto.
        let unmeasurable = |_dir: &Path| Err(io::Error::other("no such volume"));
        let refused =
            begin_upload_in_with_free_space(&dir, &begin_params("big.log", size), unmeasurable);
        assert_eq!(
            refused.err().map(|err| err.code()),
            Some("attachment_storage_failed")
        );
        assert_eq!(dir_entries(&dir), Vec::<PathBuf>::new());

        // Exactly the size plus its margin is room enough.
        let exact = |_dir: &Path| Ok(size + FREE_SPACE_MARGIN_BYTES);
        assert!(
            begin_upload_in_with_free_space(&dir, &begin_params("big.log", size), exact).is_ok()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_scratch_volume_reports_a_real_free_space_figure() {
        // The stand-in above only replaces the OS query, so the query itself
        // is exercised here against the real temp volume.
        let dir = unique_test_dir("free-space");
        fs::create_dir_all(&dir).unwrap();

        let available = crate::platform::available_bytes_on_volume(&dir)
            .unwrap_or_else(|err| panic!("the temp volume must report free space: {err}"));
        assert!(available > 0, "a writable volume reports some room");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_upload_id_the_server_never_minted_is_refused_without_touching_the_disk() {
        let dir = unique_test_dir("bad-id");
        fs::create_dir_all(&dir).unwrap();

        // A file the ids below would reach if the server let them.
        let victim = dir.join("victim.txt");
        fs::write(&victim, b"precious").unwrap();

        for id in [
            "upload-../victim",
            "upload-/etc/passwd",
            r"upload-..\victim",
            "..",
            ".",
            "victim.txt",
            "attachment-1-0.png",
            "upload-has spaces",
            "upload-nonexistent-9",
            "",
            &format!("upload-{}", "9".repeat(MAX_UPLOAD_ID_CHARS)),
        ] {
            assert_eq!(
                append_upload_in(&dir, &append_params(id, 0, b"x"))
                    .err()
                    .map(|err| err.code()),
                Some("attachment_unknown_upload"),
                "append must refuse {id:?}"
            );
            assert_eq!(
                commit_upload_in(&dir, &commit_params(id, 0))
                    .err()
                    .map(|err| err.code()),
                Some("attachment_unknown_upload"),
                "commit must refuse {id:?}"
            );
        }

        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(dir_entries(&dir), vec![victim.clone()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_minted_upload_id_matches_the_contracts_charset() {
        let dir = unique_test_dir("id-shape");

        let upload_id = begin_in(&dir, "崩溃 日志.log", 4).unwrap();

        assert!(upload_id.len() <= MAX_UPLOAD_ID_CHARS, "{upload_id}");
        assert!(
            upload_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
            "an upload id must stay inside its charset: {upload_id}"
        );
        // A name whose letters are outside that charset still survives to
        // the finished file, so the id is never where the name is kept.
        append_upload_in(&dir, &append_params(&upload_id, 0, b"logs")).unwrap();
        let created = commit_upload_in(&dir, &commit_params(&upload_id, 4)).unwrap();
        let name = created
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.ends_with("-崩溃_日志.log"), "unexpected name: {name}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_out_of_order_or_duplicated_append_is_refused_and_changes_nothing() {
        let dir = unique_test_dir("offset");
        let upload_id = begin_in(&dir, "log.txt", 32).unwrap();
        append_upload_in(&dir, &append_params(&upload_id, 0, b"first")).unwrap();
        let partial = dir.join(format!("{upload_id}.partial"));

        for offset in [0, 4, 6, 31] {
            let refused = append_upload_in(&dir, &append_params(&upload_id, offset, b"again"));
            match refused {
                Err(AttachmentError::OffsetMismatch { received }) => assert_eq!(received, 5),
                Err(err) => panic!("expected an offset mismatch, got {}", err.message()),
                Ok(_) => panic!("offset {offset} must not be accepted"),
            }
            assert_eq!(
                fs::read(&partial).unwrap(),
                b"first",
                "a refused append must not change the partial"
            );
        }

        // The offset the refusal named is the one that works.
        assert_eq!(
            append_upload_in(&dir, &append_params(&upload_id, 5, b"-second")).unwrap(),
            12
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_append_past_the_declared_size_is_refused() {
        let dir = unique_test_dir("over-declared");
        let upload_id = begin_in(&dir, "log.txt", 8).unwrap();
        append_upload_in(&dir, &append_params(&upload_id, 0, b"1234")).unwrap();

        let refused = append_upload_in(&dir, &append_params(&upload_id, 4, b"56789"));
        match refused {
            Err(AttachmentError::TooLarge { size, limit }) => {
                assert_eq!(size, 9);
                assert_eq!(limit, 8);
            }
            _ => panic!("an append past the declared size must be refused as too large"),
        }
        assert_eq!(
            fs::read(dir.join(format!("{upload_id}.partial"))).unwrap(),
            b"1234"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_append_over_the_chunk_cap_is_refused() {
        let dir = unique_test_dir("over-chunk");
        let upload_id = begin_in(&dir, "log.bin", MAX_UPLOAD_BYTES).unwrap();

        let refused = append_upload_in(
            &dir,
            &append_params(&upload_id, 0, &vec![0u8; MAX_ATTACHMENT_BYTES + 1]),
        );
        match refused {
            Err(AttachmentError::TooLarge { size, limit }) => {
                assert_eq!(size, MAX_ATTACHMENT_BYTES as u64 + 1);
                assert_eq!(limit, MAX_ATTACHMENT_BYTES as u64);
            }
            _ => panic!("an over-cap chunk must be refused as too large"),
        }
        assert_eq!(
            fs::metadata(dir.join(format!("{upload_id}.partial")))
                .unwrap()
                .len(),
            0
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_commit_whose_size_disagrees_leaves_the_partial_alone() {
        let dir = unique_test_dir("size-mismatch");
        let upload_id = begin_in(&dir, "log.txt", 16).unwrap();
        append_upload_in(&dir, &append_params(&upload_id, 0, b"half")).unwrap();

        let refused = commit_upload_in(&dir, &commit_params(&upload_id, 16));
        match refused {
            Err(AttachmentError::SizeMismatch { received, declared }) => {
                assert_eq!(received, 4);
                assert_eq!(declared, 16);
            }
            _ => panic!("a size that disagrees must be refused"),
        }

        // Still appendable, so a client can finish what it started.
        assert_eq!(
            append_upload_in(&dir, &append_params(&upload_id, 4, b" rest")).unwrap(),
            9
        );
        let created = commit_upload_in(&dir, &commit_params(&upload_id, 9)).unwrap();
        assert_eq!(fs::read(&created.path).unwrap(), b"half rest");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_upload_is_only_committed_once() {
        let dir = unique_test_dir("commit-twice");
        let upload_id = begin_in(&dir, "log.txt", 2).unwrap();
        append_upload_in(&dir, &append_params(&upload_id, 0, b"ok")).unwrap();
        commit_upload_in(&dir, &commit_params(&upload_id, 2)).unwrap();

        assert_eq!(
            commit_upload_in(&dir, &commit_params(&upload_id, 2))
                .err()
                .map(|err| err.code()),
            Some("attachment_unknown_upload")
        );
        assert_eq!(
            append_upload_in(&dir, &append_params(&upload_id, 2, b"more"))
                .err()
                .map(|err| err.code()),
            Some("attachment_unknown_upload")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_upload_is_a_file_like_any_other() {
        let dir = unique_test_dir("empty-upload");
        let upload_id = begin_in(&dir, "empty.txt", 0).unwrap();

        let created = commit_upload_in(&dir, &commit_params(&upload_id, 0)).unwrap();

        assert_eq!(fs::read(&created.path).unwrap(), Vec::<u8>::new());
        assert_eq!(dir_entries(&dir), vec![created.path.clone()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_uploads_in_flight_never_share_a_partial_or_a_final_path() {
        let dir = unique_test_dir("two-uploads");

        let first = begin_in(&dir, "log.txt", 5).unwrap();
        let second = begin_in(&dir, "log.txt", 6).unwrap();
        assert_ne!(first, second);

        append_upload_in(&dir, &append_params(&first, 0, b"aaaaa")).unwrap();
        append_upload_in(&dir, &append_params(&second, 0, b"bbbbbb")).unwrap();

        let first_path = commit_upload_in(&dir, &commit_params(&first, 5))
            .unwrap()
            .path;
        let second_path = commit_upload_in(&dir, &commit_params(&second, 6))
            .unwrap()
            .path;

        assert_ne!(first_path, second_path);
        assert_eq!(fs::read(&first_path).unwrap(), b"aaaaa");
        assert_eq!(fs::read(&second_path).unwrap(), b"bbbbbb");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_upload_in_flight_survives_a_server_that_forgets_everything() {
        // No in-memory upload table: the same calls answered by a fresh
        // module state — which is all a restart changes — keep working.
        let dir = unique_test_dir("restart");
        let upload_id = begin_in(&dir, "log.txt", 8).unwrap();
        append_upload_in(&dir, &append_params(&upload_id, 0, b"1234")).unwrap();

        // Nothing but the two files on disk describes this upload.
        let mut names: Vec<String> = dir_entries(&dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                format!("{upload_id}.partial"),
                format!("{upload_id}.upload"),
            ]
        );

        append_upload_in(&dir, &append_params(&upload_id, 4, b"5678")).unwrap();
        let created = commit_upload_in(&dir, &commit_params(&upload_id, 8)).unwrap();
        assert_eq!(fs::read(&created.path).unwrap(), b"12345678");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sweep_removes_an_abandoned_upload_and_spares_a_fresh_one() {
        let dir = unique_test_dir("sweep-partials");
        let abandoned = begin_in(&dir, "abandoned.log", 64).unwrap();
        append_upload_in(&dir, &append_params(&abandoned, 0, b"half written")).unwrap();
        let fresh = begin_in(&dir, "fresh.log", 64).unwrap();

        let stale = SystemTime::now() - (ATTACHMENT_TTL + Duration::from_secs(60 * 60));
        for suffix in [PARTIAL_SUFFIX, RECORD_SUFFIX] {
            fs::File::options()
                .write(true)
                .open(dir.join(format!("{abandoned}{suffix}")))
                .unwrap()
                .set_modified(stale)
                .unwrap();
        }

        sweep_expired(&dir);

        for suffix in [PARTIAL_SUFFIX, RECORD_SUFFIX] {
            assert!(
                !dir.join(format!("{abandoned}{suffix}")).exists(),
                "an abandoned upload must be swept like any other file"
            );
            assert!(
                dir.join(format!("{fresh}{suffix}")).exists(),
                "an upload within the TTL must survive the sweep"
            );
        }
        // And the swept id is simply an unknown upload afterwards.
        assert_eq!(
            append_upload_in(&dir, &append_params(&abandoned, 12, b"more"))
                .err()
                .map(|err| err.code()),
            Some("attachment_unknown_upload")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_declared_capability_carries_the_ceiling_and_the_message_caps_own_limit() {
        let capability = file_attachments_capability();

        assert_eq!(capability.max_bytes, 67_108_864);
        assert_eq!(capability.chunk_bytes, MAX_ATTACHMENT_BYTES as u64);
        assert_eq!(capability.chunk_bytes, 783_360);
        assert!(
            capability.chunk_bytes < capability.max_bytes,
            "a chunked upload must be able to carry more than one message"
        );
    }

    #[test]
    fn a_max_size_append_request_fits_the_unchanged_transport_cap() {
        let envelope = format!(
            r#"{{"id":"req_attach_append","method":"attachment.append","params":{{"upload_id":"{}","offset":18446744073709551615,"bytes_b64":""}}}}"#,
            "u".repeat(MAX_UPLOAD_ID_CHARS)
        )
        .len();
        let encoded_len = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4;

        assert!(
            encoded_len + envelope <= crate::api::server::MAX_INITIAL_REQUEST_BYTES,
            "a full chunk plus the widest append envelope must fit the message cap"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_dir_behind_a_pre_planted_symlink_is_refused() {
        let base = unique_test_dir("symlink");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        let link = base.join("scratch");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = create_attachment_in(&link, &photo(&encode(&png_bytes())));

        assert!(matches!(result, Err(AttachmentError::Storage(_))));
        assert_eq!(
            dir_entries(&target),
            Vec::<PathBuf>::new(),
            "nothing may be written through the symlink"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
