use crate::error::{Error, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

/// Return the default data directory for richclip: `$XDG_DATA_HOME/richclip`.
///
/// Uses the [`directories`] crate, which respects `XDG_DATA_HOME` and falls
/// back to `~/.local/share/richclip` on Linux.
pub fn default_data_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "richclip")
        .ok_or_else(|| Error::Other("could not determine home directory".into()))?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Return the default runtime directory for the daemon socket:
/// `$XDG_RUNTIME_DIR/richclip.sock`.
pub fn default_socket_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "richclip")
        .ok_or_else(|| Error::Other("could not determine home directory".into()))?;
    // ProjectDirs doesn't expose XDG_RUNTIME_DIR directly; fall back to /tmp.
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs.cache_dir().to_path_buf());
    Ok(runtime.join("richclip.sock"))
}
