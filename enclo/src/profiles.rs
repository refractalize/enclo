//! Trusted central profile selection; names are never filesystem paths.
use anyhow::{ensure, Result};
use std::path::{Path, PathBuf};

/// Explicit and enclo-specific overrides already name the application root.
pub fn config_dir(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os("ENCLO_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()))
                        .join(".config")
                });
            base.join("enclo")
        })
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "profile names must contain 1–32 letters, numbers, hyphens or underscores"
    );
    Ok(())
}
pub fn connection(
    config_dir: &Path,
    name: Option<&str>,
    explicit: Option<&Path>,
) -> Result<PathBuf> {
    match (name, explicit) {
        (Some(name), None) => {
            validate_name(name)?;
            Ok(config_dir.join("connections").join(format!("{name}.toml")))
        }
        (None, Some(path)) => Ok(path.to_path_buf()),
        (Some(_), Some(_)) => anyhow::bail!("choose a connection name or --profile, not both"),
        (None, None) => anyhow::bail!("specify a connection name or --profile <file>"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_profiles_are_confined_and_explicit_files_are_unambiguous() {
        let root = Path::new("/config/enclo");
        assert_eq!(
            connection(root, Some("work"), None).unwrap(),
            root.join("connections/work.toml")
        );
        assert!(connection(root, Some("../work"), None).is_err());
        assert!(connection(root, Some("work"), Some(Path::new("x.toml"))).is_err());
        assert!(connection(root, None, None).is_err());
        assert_eq!(
            connection(root, None, Some(Path::new("../x.toml"))).unwrap(),
            Path::new("../x.toml")
        );
    }
}
