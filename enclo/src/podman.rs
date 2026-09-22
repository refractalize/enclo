use std::{ffi::OsString, net::IpAddr};
use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::debug;

pub async fn run(args: &[OsString]) -> Result<tokio::process::Child> {
    debug!("podman run {:?}", args);
    let child = Command::new("podman")
        .arg("run")
        .args(args)
        .spawn()
        .context("failed to spawn podman run")?;
    Ok(child)
}

pub async fn wait_running(container: &str, timeout_secs: u64) -> Result<()> {
    wait_running_with(container, timeout_secs, || container_state(container)).await
}

async fn wait_running_with<F, Fut>(container: &str, timeout_secs: u64, mut state: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let state = state().await?;
        if state == "running" {
            return Ok(());
        }
        if state == "exited" || state == "stopped" {
            anyhow::bail!("container {} exited before becoming ready", container);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("container {} did not reach running state within {}s", container, timeout_secs);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub async fn exec_as(container: &str, user: &str, args: &[String]) -> Result<()> {
    debug!("podman exec --user {} {} {:?}", user, container, args);
    let status = Command::new("podman")
        .arg("exec")
        .arg(format!("--user={}", user))
        .arg(container)
        .args(args)
        .status()
        .await
        .context("failed to spawn podman exec")?;
    if !status.success() {
        anyhow::bail!("podman exec exited with {}", status);
    }
    Ok(())
}

pub async fn exec(
    container: &str,
    shell: &str,
    shell_args: &[String],
    env_file: &std::path::Path,
    workdir: &str,
    user: Option<&str>,
    extra_env: &[String],
    extra_args: &[String],
) -> Result<()> {
    debug!("podman exec into {}", container);
    let mut cmd = Command::new("podman");
    cmd.arg("exec")
        .arg("--interactive")
        .arg("--tty")
        .arg(format!("--env-file={}", env_file.display()))
        .arg(format!("--workdir={}", workdir));
    if let Some(u) = user {
        cmd.arg(format!("--user={u}"));
    }
    for e in extra_env {
        cmd.arg(format!("--env={e}"));
    }
    cmd.args(extra_args);
    cmd.arg(container).arg(shell).args(shell_args);
    let status = cmd.status().await.context("failed to spawn podman exec")?;
    if !status.success() {
        anyhow::bail!("podman exec exited with {}", status);
    }
    Ok(())
}

pub async fn exec_as_with_stdin(container: &str, user: &str, args: &[&str], stdin_data: Vec<u8>) -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    debug!("podman exec --user {} {} {:?} (with stdin)", user, container, args);
    let mut child = Command::new("podman")
        .arg("exec")
        .arg("--interactive")
        .arg(format!("--user={}", user))
        .arg(container)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn podman exec")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&stdin_data).await?;
    }

    let status = child.wait().await.context("failed to wait for podman exec")?;
    if !status.success() {
        anyhow::bail!("podman exec exited with {}", status);
    }
    Ok(())
}

pub async fn stop(container: &str) -> Result<()> {
    debug!("podman stop {}", container);
    let out = Command::new("podman")
        .args(["stop", container])
        .output()
        .await
        .context("failed to spawn podman stop")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("podman stop exited with {}: {}", out.status, stderr.trim());
    }
    Ok(())
}

pub async fn remove(container: &str) -> Result<()> {
    debug!("podman rm --force --ignore {}", container);
    let status = Command::new("podman")
        .args(["rm", "--force", "--ignore", container])
        .status()
        .await
        .context("failed to spawn podman rm")?;
    if !status.success() {
        anyhow::bail!("podman rm exited with {}", status);
    }
    Ok(())
}

pub async fn exec_ids(container: &str) -> Result<Vec<String>> {
    let out = Command::new("podman")
        .args(["inspect", "--format", "{{json .ExecIDs}}", container])
        .output()
        .await
        .context("failed to spawn podman inspect")?;

    if !out.status.success() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }

    serde_json::from_str::<Vec<String>>(trimmed)
        .context("failed to parse ExecIDs from podman inspect")
}

pub async fn user_exec_sessions(container: &str, managed: &[String]) -> Result<Vec<String>> {
    let all = exec_ids(container).await?;
    Ok(all.into_iter().filter(|id| !managed.contains(id)).collect())
}

pub async fn network_gateway(network: &str) -> Result<IpAddr> {
    let out = Command::new("podman")
        .args(["network", "inspect", network, "--format", "{{range .Subnets}}{{.Gateway}}{{end}}"])
        .output()
        .await
        .context("failed to spawn podman network inspect")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("podman network inspect failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no gateway found for network '{network}'"))?
        .parse::<IpAddr>()
        .map_err(|e| anyhow::anyhow!("failed to parse gateway IP for network '{network}': {e}"))
}

pub async fn container_state(container: &str) -> Result<String> {
    let out = Command::new("podman")
        .args(["inspect", "--format", "{{.State.Status}}", container])
        .output()
        .await
        .context("failed to spawn podman inspect")?;

    if !out.status.success() {
        return Ok("not found".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn readiness_can_take_longer_than_old_timeout() {
        let start = tokio::time::Instant::now();
        wait_running_with("test", 60, || async {
            Ok(if start.elapsed().as_secs() >= 45 { "running" } else { "created" }.into())
        }).await.unwrap();
        assert!(start.elapsed().as_secs() >= 45);
    }

    #[tokio::test(start_paused = true)]
    async fn readiness_uses_custom_timeout() {
        let start = tokio::time::Instant::now();
        let error = wait_running_with("test", 180, || async { Ok("created".into()) })
            .await.unwrap_err();
        assert!(error.to_string().contains("within 180s"));
        assert!(start.elapsed().as_secs() >= 180);
        assert!(start.elapsed().as_secs() <= 181);
    }

    #[tokio::test(start_paused = true)]
    async fn early_exit_fails_without_waiting() {
        for state in ["exited", "stopped"] {
            let start = tokio::time::Instant::now();
            let error = wait_running_with("test", 60, || async { Ok(state.into()) })
                .await.unwrap_err();
            assert!(error.to_string().contains("exited before becoming ready"));
            assert!(start.elapsed().is_zero());
        }
    }
}
