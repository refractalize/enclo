//! Stable, private runtime layout shared by service and session control APIs.
use anyhow::{ensure, Result};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

pub fn directory(profile: &Path, root: Option<&Path>) -> Result<PathBuf> {
    let name = profile
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("profile needs a connection name"))?;
    ensure!(!name.is_empty() && name.len() <= 32 && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "connection name (profile filename without extension) must contain 1–32 letters, numbers, hyphens or underscores");
    let dir = connections_dir(root).join(name);
    ensure!(
        dir.as_os_str().as_encoded_bytes().len() + 42 < 104,
        "connection runtime path too long; use a shorter --runtime-dir or connection name"
    );
    Ok(dir)
}
pub fn connections_dir(root: Option<&Path>) -> PathBuf {
    let base = root
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os("ENCLO_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR").map(|root| PathBuf::from(root).join("enclo"))
        });
    let base = match base {
        Some(root) => root,
        None => std::env::temp_dir().join(format!("enclo-{}", unsafe { libc::geteuid() })),
    };
    base.join("connections")
}

/// Discover private named runtime directories, independent of profile files.
pub fn discover(root: Option<&Path>) -> Result<Vec<String>> {
    let dir = connections_dir(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    validate(dir.parent().unwrap())?;
    validate(&dir)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if crate::profiles::validate_name(&name).is_err() {
            continue;
        }
        validate(&entry.path())?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

pub fn validate(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "runtime directory must be owned by you with mode 0700: {}",
        dir.display()
    );
    Ok(())
}
pub fn prepare(dir: &Path) -> Result<()> {
    // All three components are private; never follow an existing symlink.
    let connections = dir.parent().unwrap();
    let base = connections.parent().unwrap();
    for path in [base, connections, dir] {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        validate(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_stable_and_cannot_escape_runtime() {
        let root = Some(Path::new("/tmp"));
        assert_eq!(
            directory(Path::new("/one/dev.toml"), root).unwrap(),
            directory(Path::new("/two/dev.toml"), root).unwrap()
        );
        assert_eq!(
            directory(Path::new("dev.toml"), root).unwrap(),
            Path::new("/tmp/connections/dev")
        );
        for name in [
            "bad name.toml",
            ".hidden.toml",
            "dev.other.toml",
            "abcdefghijklmnopqrstuvwxyz1234567.toml",
        ] {
            assert!(directory(Path::new(name), root).is_err());
        }
    }
}
