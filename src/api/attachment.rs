//! Server-scoped attachment storage for `attachment.create`.
//!
//! Photo bytes arrive base64-encoded, are validated (size cap, magic-byte
//! sniff), and land atomically in a herdr-owned scratch directory with
//! owner-only permissions and server-generated space-free names. A TTL sweep
//! runs at listener start and hourly so the scratch dir never becomes
//! long-term storage. The whole path is pure file I/O on the connection
//! thread: no app state, so behavior is identical over every transport.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use base64::Engine as _;

use crate::api::schema::{AttachmentCreateParams, ResponseResult, SuccessResponse};

pub(crate) const ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Decoded size cap: 3/4 of `MAX_INITIAL_REQUEST_BYTES`, the most base64
/// payload that can ride a request under the unchanged 1 MiB message cap.
/// The transport cap rejects anything larger before dispatch; this in-band
/// check keeps an honest, distinct error code if the caps ever diverge.
pub(crate) const MAX_ATTACHMENT_BYTES: usize = super::server::MAX_INITIAL_REQUEST_BYTES / 4 * 3;

pub(super) enum AttachmentError {
    InvalidBase64(String),
    TooLarge(usize),
    UnsupportedFormat,
    Storage(io::Error),
}

impl AttachmentError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            AttachmentError::InvalidBase64(_) => "invalid_params",
            AttachmentError::TooLarge(_) => "attachment_too_large",
            AttachmentError::UnsupportedFormat => "attachment_unsupported_format",
            AttachmentError::Storage(_) => "attachment_storage_failed",
        }
    }

    pub(super) fn message(&self) -> String {
        match self {
            AttachmentError::InvalidBase64(err) => {
                format!("bytes_b64 is not valid base64: {err}")
            }
            AttachmentError::TooLarge(size) => {
                format!("attachment is {size} bytes; the limit is {MAX_ATTACHMENT_BYTES} bytes")
            }
            AttachmentError::UnsupportedFormat => {
                "attachment bytes are not a recognized image format (JPEG, PNG, or WebP)".into()
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
    match create_attachment(&params.bytes_b64) {
        Ok(created) => serde_json::to_string(&SuccessResponse {
            id,
            result: ResponseResult::AttachmentCreated {
                path: created.path.to_string_lossy().into_owned(),
                expires_at: created.expires_at,
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
                .to_string()
        }),
        Err(err) => super::server::error_response_json(id, err.code(), err.message()),
    }
}

fn create_attachment(bytes_b64: &str) -> Result<CreatedAttachment, AttachmentError> {
    let dir = scratch_dir().map_err(AttachmentError::Storage)?;
    create_attachment_in(&dir, bytes_b64)
}

/// Validation order per the contract: decode base64, enforce the size cap,
/// sniff magic bytes, then write. A rejection or write failure leaves no
/// file and no temp artifact behind.
fn create_attachment_in(dir: &Path, bytes_b64: &str) -> Result<CreatedAttachment, AttachmentError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(bytes_b64.trim())
        .map_err(|err| AttachmentError::InvalidBase64(err.to_string()))?;

    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge(bytes.len()));
    }

    let extension = sniff_image_extension(&bytes).ok_or(AttachmentError::UnsupportedFormat)?;

    ensure_scratch_dir(dir).map_err(AttachmentError::Storage)?;
    write_atomically(dir, extension, &bytes).map_err(AttachmentError::Storage)
}

/// Write under a temporary name, then rename into place. The rename happens
/// before the success response is encoded, so a returned path never names a
/// partial file; any failure removes the temp artifact.
fn write_atomically(
    dir: &Path,
    extension: &'static str,
    bytes: &[u8],
) -> io::Result<CreatedAttachment> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100 {
        let stem = format!("attachment-{unique}-{attempt}");
        let partial_path = dir.join(format!("{stem}.{extension}.partial"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        restrict_file_options(&mut options);
        let mut file = match options.open(&partial_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };

        let path = dir.join(format!("{stem}.{extension}"));
        let written = file
            .write_all(bytes)
            .and_then(|()| file.flush())
            .and_then(|()| {
                drop(file);
                fs::rename(&partial_path, &path)
            });
        if let Err(err) = written {
            let _ = fs::remove_file(&partial_path);
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

        let created = create_attachment_in(&dir, &encode(&bytes)).unwrap_or_else(|err| {
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
            let created = create_attachment_in(&dir, &encode(&bytes)).unwrap_or_else(|err| {
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

        let malformed = create_attachment_in(&dir, "not base64!!");
        assert!(matches!(malformed, Err(AttachmentError::InvalidBase64(_))));

        let mut oversize = png_bytes();
        oversize.resize(MAX_ATTACHMENT_BYTES + 1, 0);
        let too_large = create_attachment_in(&dir, &encode(&oversize));
        match too_large {
            Err(AttachmentError::TooLarge(size)) => assert_eq!(size, MAX_ATTACHMENT_BYTES + 1),
            _ => panic!("oversize payload must be rejected as too large"),
        }

        let unsupported = create_attachment_in(&dir, &encode(b"GIF89a not an image we accept"));
        assert!(matches!(
            unsupported,
            Err(AttachmentError::UnsupportedFormat)
        ));

        assert_eq!(dir_entries(&dir), Vec::<PathBuf>::new());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_cap_admits_exactly_the_limit() {
        let dir = unique_test_dir("cap-edge");
        let mut at_limit = png_bytes();
        at_limit.resize(MAX_ATTACHMENT_BYTES, 0);
        let created = create_attachment_in(&dir, &encode(&at_limit));
        assert!(created.is_ok(), "a payload at the cap must be accepted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_codes_are_distinct_per_failure() {
        assert_eq!(
            AttachmentError::InvalidBase64("bad".into()).code(),
            "invalid_params"
        );
        assert_eq!(AttachmentError::TooLarge(1).code(), "attachment_too_large");
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

    #[cfg(unix)]
    #[test]
    fn a_scratch_dir_behind_a_pre_planted_symlink_is_refused() {
        let base = unique_test_dir("symlink");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        let link = base.join("scratch");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = create_attachment_in(&link, &encode(&png_bytes()));

        assert!(matches!(result, Err(AttachmentError::Storage(_))));
        assert_eq!(
            dir_entries(&target),
            Vec::<PathBuf>::new(),
            "nothing may be written through the symlink"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
