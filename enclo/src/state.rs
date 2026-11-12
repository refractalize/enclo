//! Persistent application state, distinct from disposable connection runtime.
use std::path::{Path, PathBuf};

pub fn directory(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_owned)
        .or_else(|| {
            std::env::var_os("ENCLO_STATE_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()))
                        .join(".local/state")
                })
                .join("enclo")
        })
}
