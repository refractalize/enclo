//! Restricted workspace configuration and host-side, content-addressed approvals.
use crate::{config::Config, context::Context as ContainerContext};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::{self, IsTerminal, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Debug)]
pub struct Workspace {
    pub profile: String,
    pub root: PathBuf,
    pub destination: PathBuf,
    pub fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConfig {
    container: LocalContainer,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalContainer {
    from: String,
    workspace_network: Option<bool>,
    directory: Option<String>,
    #[serde(default)]
    home: LocalHome,
}
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LocalHome {
    #[serde(default)]
    init: LocalInit,
}
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LocalInit {
    command_after: Option<String>,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn discover(start: &Path) -> Result<Option<PathBuf>> {
    for directory in start.canonicalize()?.ancestors() {
        let path = directory.join(".enclo.toml");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Ok(Some(path)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn parse(bytes: &[u8]) -> Result<LocalConfig> {
    let local: LocalConfig = toml::from_str(std::str::from_utf8(bytes)?)?;
    let name = &local.container.from;
    anyhow::ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "container.from must be a simple central profile name (letters, numbers, - and _)"
    );
    Ok(local)
}

fn destination(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    anyhow::ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && !value.contains([':', '%', '\n']),
        "container.directory must be an absolute path without .., : or specifiers"
    );
    anyhow::ensure!(path != Path::new("/"), "container.directory cannot be /");
    Ok(path.to_path_buf())
}

pub fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

pub fn load(path: &Path, interactive: bool) -> Result<Config> {
    // Canonicalize the containing directory, not the file: symlinks must not change the workspace root.
    let root = path.parent().unwrap_or(Path::new(".")).canonicalize()?;
    let path = root.join(".enclo.toml");
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let local = parse(&bytes)?;
    let central_path = crate::config_dir()
        .join("containers")
        .join(format!("{}.toml", local.container.from));
    let central = std::fs::read(&central_path)?;
    let mut config = Config::load(&central_path)?;
    anyhow::ensure!(
        std::fs::read(&central_path)? == central,
        "central configuration changed while loading; retry"
    );
    if let Some(enabled) = local.container.workspace_network {
        config.container.workspace_network = enabled;
    }
    config.container.home.command_after = local.container.home.init.command_after;
    anyhow::ensure!(
        config.container.home.enabled || config.container.home.command_after.is_none(),
        "local command_after requires container.home.enabled in the central profile"
    );
    let dest = destination(
        local
            .container
            .directory
            .as_deref()
            .or(config.container.directory.as_deref())
            .unwrap_or("/workspace"),
    )?;
    let instance = format!(
        "enclo-workspace-{}",
        &hash(root.as_os_str().as_encoded_bytes())[..32]
    );
    let ctx = ContainerContext::new(instance.clone())?;
    if config.container.home.enabled {
        anyhow::ensure!(
            !overlaps(&dest, &ctx.home),
            "workspace mount conflicts with managed home"
        );
    }
    for volume in &config.container.volumes {
        if let Some(target) = volume.split(':').nth(1) {
            anyhow::ensure!(
                !overlaps(&dest, Path::new(&ctx.expand(target))),
                "workspace mount conflicts with central mount at {target}"
            );
        }
    }
    // These contain system files or module-provided executables and sockets.
    for protected in [
        "/bin",
        "/sbin",
        "/usr",
        "/etc",
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/tmp/enclo-secrets.sock",
    ] {
        anyhow::ensure!(
            !overlaps(&dest, Path::new(protected)),
            "workspace mount conflicts with protected path {protected}"
        );
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.home.join(".local/state"))
        .join("enclo/trust");
    let approval = state.join(hash(root.as_os_str().as_encoded_bytes()));
    let digest = hash(&bytes);
    if std::fs::read_to_string(&approval).ok().as_deref() != Some(&digest) {
        if !interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!(
                "untrusted workspace configuration {}; run enclo enter interactively to approve it",
                path.display()
            );
        }
        eprintln!("Workspace configuration: {}\nCentral profile: {}\nWorkspace mount: {} → {}\n\nThis container receives the capabilities and secrets of the central profile.", path.display(), local.container.from, root.display(), dest.display());
        if let Some(script) = &config.container.home.command_after {
            eprintln!("\nLocal initialization script (runs inside the container):\n{script}");
        }
        eprint!("Trust this configuration? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        anyhow::ensure!(
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            "workspace configuration was not trusted"
        );
        std::fs::create_dir_all(&state)?;
        // Separate records avoid lost approvals from concurrent workspace invocations.
        std::fs::write(&approval, &digest)?;
    }
    let source = root.to_str().context("workspace directory must be UTF-8")?;
    anyhow::ensure!(
        !source.contains([':', '\n', '%']),
        "unsupported character in workspace directory"
    );
    config.container.name = instance;
    config
        .container
        .volumes
        .push(format!("{}:{}", root.display(), dest.display()));
    // Fingerprint the effective central configuration as well as the local file.
    let mut fingerprint = Sha256::new();
    fingerprint.update((bytes.len() as u64).to_le_bytes());
    fingerprint.update(&bytes);
    fingerprint.update(&central);
    config.workspace = Some(Workspace {
        profile: local.container.from,
        root,
        destination: dest,
        fingerprint: format!("{:x}", fingerprint.finalize()),
    });
    Ok(config)
}

pub fn untrust(start: &Path) -> Result<()> {
    let path = discover(start)?.context("no .enclo.toml found")?;
    let root = path.parent().unwrap().canonicalize()?;
    let ctx = ContainerContext::new(String::new())?;
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.home.join(".local/state"))
        .join("enclo/trust");
    match std::fs::remove_file(state.join(hash(root.as_os_str().as_encoded_bytes()))) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    println!("Forgot trust for {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restricted_schema() {
        assert!(parse(b"[container]\nfrom='default'\ndirectory='/workspace'").is_ok());
        for text in [
            "from='../default'",
            "from='/default'",
            "from='default'\nimage='evil'",
            "from='default'\nvolumes=[]",
            "name='default'",
        ] {
            assert!(parse(format!("[container]\n{text}").as_bytes()).is_err());
        }
    }
    #[test]
    fn local_network_setting_requires_a_boolean() {
        for enabled in ["true", "false"] {
            let local = parse(
                format!("[container]\nfrom='default'\nworkspace_network={enabled}").as_bytes(),
            )
            .unwrap();
            assert_eq!(local.container.workspace_network, Some(enabled == "true"));
        }
        assert!(parse(b"[container]\nfrom='default'\nworkspace_network='host'").is_err());
    }

    #[test]
    fn safe_destinations() {
        for value in ["/", "relative", "/a/../etc", "/a:b", "/%h"] {
            assert!(destination(value).is_err());
        }
        assert_eq!(destination("/workspace").unwrap(), Path::new("/workspace"));
        assert!(overlaps(Path::new("/work"), Path::new("/work/src")));
        assert!(!overlaps(Path::new("/work"), Path::new("/workspace")));
    }
}
