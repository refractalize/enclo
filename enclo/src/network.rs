//! Persistent, identity-scoped bridge networks shared by container and secrets setup.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::config::Config;

pub fn effective_name(config: &Config) -> String {
    if config.container.workspace_network {
        let digest = format!("{:x}", Sha256::digest(config.container.name.as_bytes()));
        format!("enclo-network-{}", &digest[..32])
    } else {
        config.container.network.clone()
    }
}

pub async fn prepare(config: &Config) -> Result<()> {
    if !config.container.workspace_network {
        return Ok(());
    }
    let name = effective_name(config);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Command::new("podman")
            .args([
                "network",
                "create",
                "--ignore",
                "--driver",
                "bridge",
                "--label",
                "org.enclo.managed=true",
                &name,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("workspace network creation timed out")?
    .context("failed to spawn podman network create")?;
    anyhow::ensure!(
        output.status.success(),
        "creating workspace network {name} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_network_overrides_host_and_is_instance_scoped() {
        let mut config: Config = toml::from_str(
            "[container]\nname='enclo-workspace-first'\nimage='test'\nworkspace_network=true\n[modules]",
        ).unwrap();
        let first = effective_name(&config);
        assert!(first.starts_with("enclo-network-"));
        assert_eq!(first, effective_name(&config));
        config.container.name = "enclo-workspace-second".into();
        assert_ne!(first, effective_name(&config));
        config.container.workspace_network = false;
        assert_eq!(effective_name(&config), "host");
        config.container.network = "custom".into();
        assert_eq!(effective_name(&config), "custom");
    }
}
