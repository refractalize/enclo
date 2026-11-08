//! Persistent per-instance homes and independently checkpointed initialization.
use crate::{config::Config, context::Context};
use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use std::{fs::File, os::fd::AsRawFd, path::Path, time::Duration};
use tokio::process::Command;

pub fn volume_name(ctx: &Context) -> String {
    let digest = format!("{:x}", Sha256::digest(ctx.container_name.as_bytes()));
    format!("enclo-home-{}", &digest[..32])
}

pub fn configure(config: &mut Config, ctx: &Context) -> Result<()> {
    let home = &config.container.home;
    anyhow::ensure!(
        home.enabled || home.init.command.is_none(),
        "container.home.init.command requires container.home.enabled = true"
    );
    if !home.enabled {
        return Ok(());
    }
    anyhow::ensure!(
        config.container.userns == "keep-id" || config.container.userns.starts_with("keep-id:"),
        "managed homes require container.userns = 'keep-id' (optionally with mapping options)"
    );
    for volume in &config.container.volumes {
        if let Some(target) = volume.split(':').nth(1) {
            let target = ctx.expand(target);
            anyhow::ensure!(
                !ctx.home.starts_with(Path::new(&target)),
                "managed home conflicts with volume at {target}; remove the existing home mount"
            );
        }
    }
    config.container.volumes.push(format!(
        "{}:{}:nocopy",
        volume_name(ctx),
        ctx.home.display()
    ));
    Ok(())
}

pub fn startup_budget(config: &Config) -> Duration {
    if !config.container.home.enabled {
        return Duration::ZERO;
    }
    // Both scripts have their own timeout, plus bounded volume preparation.
    Duration::from_secs(120 + 2 * u64::from(config.container.home.init.timeout_secs.get()))
}

fn userns(config: &Config, ctx: &Context) -> String {
    #[cfg(target_os = "macos")]
    if config.container.userns == "keep-id" {
        return format!("keep-id:uid={},gid={}", ctx.uid, ctx.gid);
    }
    let _ = ctx;
    config.container.userns.clone()
}

async fn output(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<std::process::Output> {
    command.kill_on_drop(true);
    let out = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| format!("{label} timed out after {} seconds", timeout.as_secs()))?
        .with_context(|| format!("failed to run {label}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{label} failed ({}):\n{}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out)
}

/// Create a genuinely empty volume (no image copy-up) and give its root to the user.
pub async fn prepare(config: &Config, ctx: &Context) -> Result<()> {
    if !config.container.home.enabled {
        return Ok(());
    }
    let mut command = Command::new("podman");
    command
        .args([
            "run",
            "--rm",
            "--network=none",
            "--user=0",
            "--entrypoint=sh",
        ])
        .arg(format!("--userns={}", userns(config, ctx)))
        .arg(format!("--volume={}:/enclo-home:nocopy", volume_name(ctx)));
    if config.container.security_label_disable {
        command.arg("--security-opt=label=disable");
    }
    command.arg(&config.container.image).args([
        "-eu",
        "-c",
        "chown \"$1:$2\" /enclo-home",
        "enclo-home",
        &ctx.uid.to_string(),
        &ctx.gid.to_string(),
    ]);
    output(command, Duration::from_secs(60), "home volume preparation").await?;
    Ok(())
}

fn checkpoint_exec(ctx: &Context) -> Command {
    let mut command = Command::new("podman");
    command
        .arg("exec")
        .arg(format!("--user={}:{}", ctx.uid, ctx.gid))
        .args([&ctx.container_name, "sh", "-eu", "-c"]);
    command
}

fn lock(ctx: &Context) -> Result<File> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ctx.home.join(".local/state"))
        .join("enclo/home-locks");
    std::fs::create_dir_all(&state)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join(volume_name(ctx)))?;
    // Nonblocking: never stall the daemon's async executor on a competing initializer.
    anyhow::ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "home initialization is already in progress for {}",
        ctx.container_name
    );
    Ok(file)
}

pub async fn initialize(config: &Config, ctx: &Context, rerun: bool) -> Result<()> {
    if !config.container.home.enabled {
        anyhow::ensure!(!rerun, "managed home is not enabled for this container");
        return Ok(());
    }
    let mut inspect = Command::new("podman");
    inspect.args([
        "inspect",
        "--format",
        "{{json .Mounts}}",
        &ctx.container_name,
    ]);
    let mounts = output(
        inspect,
        Duration::from_secs(10),
        "checking managed home mount",
    )
    .await?;
    let mounts: Vec<serde_json::Value> =
        serde_json::from_slice(&mounts.stdout).context("invalid mount information from Podman")?;
    let expected = volume_name(ctx);
    anyhow::ensure!(mounts.iter().any(|mount| mount["Type"] == "volume"
        && mount["Name"] == expected && mount["Destination"] == ctx.home.to_string_lossy().as_ref()),
        "running container does not have the expected private home volume; restart the container first");
    let _lock = lock(ctx)?;
    let timeout = Duration::from_secs(u64::from(config.container.home.init.timeout_secs.get()));
    let markers = ctx.home.join(".enclo-init");
    if rerun {
        let mut command = checkpoint_exec(ctx);
        command
            .arg("rm -f -- \"$1/central\" \"$1/local\"")
            .arg("enclo-init")
            .arg(&markers);
        output(
            command,
            Duration::from_secs(10),
            "resetting initialization checkpoints",
        )
        .await?;
    }
    for (stage, script) in [
        ("central", config.container.home.init.command.as_deref()),
        ("local", config.container.home.command_after.as_deref()),
    ] {
        let Some(script) = script else {
            continue;
        };
        let marker = markers.join(stage);
        let digest = format!("{:x}", Sha256::digest(script.as_bytes()));
        let mut read = checkpoint_exec(ctx);
        read.arg("mkdir -p -- \"$1\"; if test -f \"$2\"; then cat -- \"$2\"; fi")
            .arg("enclo-init")
            .arg(&markers)
            .arg(&marker);
        let existing = output(
            read,
            Duration::from_secs(10),
            "reading initialization checkpoint",
        )
        .await?;
        if !existing.stdout.is_empty() {
            if existing.stdout != digest.as_bytes() {
                tracing::warn!("{stage} home initialization script changed; keeping completed state. Run enclo home init --rerun to apply it");
            }
            continue;
        }
        tracing::info!(
            "running {stage} home initialization for {}",
            ctx.container_name
        );
        let mut command = Command::new("podman");
        command
            .arg("exec")
            .arg(format!("--user={}:{}", ctx.uid, ctx.gid))
            .arg(format!(
                "--env-file={}",
                ctx.enclo_dir.join("exec-env").display()
            ))
            .arg(format!("--env=HOME={}", ctx.home.display()))
            .arg(format!("--workdir={}", ctx.home.display()))
            .arg(&ctx.container_name)
            .args(["sh", "-eu", "-c", script]);
        let result = match output(command, timeout, &format!("{stage} home initialization")).await {
            Ok(result) => result,
            Err(error) => {
                // Killing a Podman exec client alone need not kill its process in the container.
                crate::podman::stop(&ctx.container_name).await.ok();
                return Err(error);
            }
        };
        if !result.stdout.is_empty() {
            tracing::info!("{}", String::from_utf8_lossy(&result.stdout));
        }
        if !result.stderr.is_empty() {
            tracing::info!("{}", String::from_utf8_lossy(&result.stderr));
        }
        let mut write = checkpoint_exec(ctx);
        write
            .arg("printf '%s' \"$2\" > \"$1.tmp\"; mv -- \"$1.tmp\" \"$1\"")
            .arg("enclo-init")
            .arg(&marker)
            .arg(digest);
        output(
            write,
            Duration::from_secs(10),
            "saving initialization checkpoint",
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(extra: &str) -> Config {
        toml::from_str(&format!("[container]\nimage='test'\n{extra}\n[modules]\n")).unwrap()
    }
    #[test]
    fn managed_home_is_opt_in_and_instance_scoped() {
        let mut a = Context::new("instance-a".into()).unwrap();
        a.home = "/home/test".into();
        let mut b = a.clone();
        b.container_name = "instance-b".into();
        assert_eq!(volume_name(&a), volume_name(&a));
        assert_ne!(volume_name(&a), volume_name(&b));
        let mut disabled = config("");
        configure(&mut disabled, &a).unwrap();
        assert!(disabled.container.volumes.is_empty());
        let mut enabled = config("[container.home]\nenabled=true");
        configure(&mut enabled, &a).unwrap();
        assert_eq!(
            enabled.container.volumes,
            [format!("{}:/home/test:nocopy", volume_name(&a))]
        );
    }
    #[test]
    fn existing_home_mount_is_never_silently_replaced() {
        let mut ctx = Context::new("test".into()).unwrap();
        ctx.home = "/home/test".into();
        for mount in ["old:%h", "old:/home", "old:/"] {
            let mut c = config(&format!(
                "volumes=['{mount}']\n[container.home]\nenabled=true"
            ));
            assert!(configure(&mut c, &ctx).is_err());
        }
        let mut c = config("volumes=['cache:%h/.cache']\n[container.home]\nenabled=true");
        configure(&mut c, &ctx).unwrap();
        assert_eq!(c.container.volumes.len(), 2);
    }
    #[test]
    fn initialization_requires_enabled_home_and_valid_timeout() {
        let ctx = Context::new("test".into()).unwrap();
        let mut c = config("[container.home.init]\ncommand='echo test'");
        assert!(configure(&mut c, &ctx).is_err());
        assert!(toml::from_str::<Config>(
            "[container]\nimage='test'\n[container.home.init]\ntimeout_secs=0\n[modules]"
        )
        .is_err());
        let c = config("[container.home]\nenabled=true\n[container.home.init]\ntimeout_secs=120");
        assert!(startup_budget(&c).as_secs() >= 240);
    }
}
