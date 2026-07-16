//! The running server executable advertised in API pongs.
//!
//! The process snapshots its executable pathname and file identity before
//! dispatching commands. A later ping reports that pathname only while it
//! still names the same file, including after listeners are rebound during a
//! failed live handoff.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug)]
struct RunningExecutable {
    path: PathBuf,
    identity: crate::platform::ExecutableFileIdentity,
}

impl RunningExecutable {
    fn capture() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        if !path.is_absolute() {
            return None;
        }
        let identity = crate::platform::executable_file_identity(&path).ok()?;
        Some(Self { path, identity })
    }

    fn path_if_unchanged(&self) -> Option<String> {
        let current_identity = crate::platform::executable_file_identity(&self.path).ok()?;
        if current_identity != self.identity {
            return None;
        }
        self.path.to_str().map(str::to_owned)
    }
}

static RUNNING_EXECUTABLE: OnceLock<Option<RunningExecutable>> = OnceLock::new();

pub(crate) fn initialize() {
    RUNNING_EXECUTABLE.get_or_init(RunningExecutable::capture);
}

pub(crate) fn path_for_pong() -> Option<String> {
    RUNNING_EXECUTABLE
        .get_or_init(RunningExecutable::capture)
        .as_ref()
        .and_then(RunningExecutable::path_if_unchanged)
}
