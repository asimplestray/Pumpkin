//! Bounds for plugin-controlled strings and file names.
//!
//! Names reaching the filesystem (`config_path`, the rooted `io.open`) must
//! be plain file names: no separators, no parent traversal, no dotfiles,
//! and a small length cap. Symlink escapes are a known follow-up.

use std::path::{Path, PathBuf};

/// Maximum config/plugin-controlled file name length.
const MAX_NAME_LEN: usize = 64;

/// Checks a string against a length cap.
pub fn bounded_string(value: &str, max: usize) -> Result<(), String> {
    if value.len() <= max {
        Ok(())
    } else {
        Err(format!("value exceeds {max} characters"))
    }
}

/// Validates a data-folder file name.
pub fn validate_config_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err("config name must be 1-64 characters".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("config name must not contain path separators".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err("config name must match [a-zA-Z0-9._-]".to_string());
    }
    if name.starts_with('.') {
        return Err("config name must not start with '.'".to_string());
    }
    Ok(())
}

/// Joins an already-validated name onto a data directory.
pub fn config_path_for(data_dir: &Path, name: &str) -> PathBuf {
    debug_assert!(validate_config_name(name).is_ok());
    data_dir.join(name)
}

/// Resolves a user-supplied IO path against the sandbox root.
///
/// Absolute paths and `..` components are rejected lexically.
pub fn resolve_io_path(root: &Path, name: &str) -> Result<PathBuf, mlua::Error> {
    validate_config_name(name).map_err(mlua::Error::external)?;
    Ok(config_path_for(root, name))
}
