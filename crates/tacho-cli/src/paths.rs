//! Filesystem path resolution for tachograph CLI.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Resolves `$TACHO_HOME`:
/// 1. `$TACHO_HOME`
/// 2. `$XDG_STATE_HOME/tacho`
/// 3. `$HOME/.local/state/tacho`
pub fn resolve_home() -> PathBuf {
    if let Ok(val) = env::var("TACHO_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val);
        }
    }
    if let Ok(val) = env::var("XDG_STATE_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val).join("tacho");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/state/tacho")
}

/// Resolves `$TACHO_RUNTIME`:
/// 1. `$TACHO_RUNTIME`
/// 2. `$XDG_RUNTIME_DIR/tacho`
/// 3. `$TACHO_HOME/run`
/// 4. `$HOME/.local/state/tacho/run`
pub fn resolve_runtime() -> PathBuf {
    if let Ok(val) = env::var("TACHO_RUNTIME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val);
        }
    }
    if let Ok(val) = env::var("XDG_RUNTIME_DIR") {
        if !val.trim().is_empty() {
            return PathBuf::from(val).join("tacho");
        }
    }
    resolve_home().join("run")
}

/// Creates a directory recursively with mode 0700 if it does not exist,
/// or sets mode 0700 on existing directories.
pub fn ensure_dir_0700(path: &Path) -> io::Result<()> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !path.exists() {
        DirBuilder::new().mode(0o700).recursive(true).create(path)?;
    } else {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
