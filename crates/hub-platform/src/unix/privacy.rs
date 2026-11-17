//! Data-dir privacy on Unix: a directory with mode 0700 owned by the effective user.
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Exactly `rwx------`, owned by us; false when missing, not a directory or not statable.
pub fn is_private_dir(dir: &Path) -> bool {
    std::fs::metadata(dir)
        .is_ok_and(|m| m.is_dir() && m.uid() == euid() && m.mode() & 0o777 == 0o700)
}

/// Creates `dir` with mode 0700 (missing parents with the default mode, like `mkdir -p`), or chmods
/// an existing dir we own to 0700. A dir owned by anyone else is left alone and is an error.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::metadata(dir)?;
    if !m.is_dir() {
        return Err(io::Error::other(
            "directory is not private: not a directory",
        ));
    }
    if m.uid() != euid() {
        return Err(io::Error::other(
            "directory is not private: owned by another user",
        ));
    }
    if m.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if is_private_dir(dir) {
        Ok(())
    } else {
        Err(io::Error::other(
            "directory is not private: could not verify it",
        ))
    }
}
