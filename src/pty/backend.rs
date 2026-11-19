#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

#[cfg(windows)]
pub(crate) struct SpawnedPty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

#[cfg(windows)]
pub(crate) fn spawn_with_portable_pty(
    size: PtySize,
    cmd: CommandBuilder,
) -> std::io::Result<SpawnedPty> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(size)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    Ok(SpawnedPty {
        master: pair.master,
        child,
    })
}
