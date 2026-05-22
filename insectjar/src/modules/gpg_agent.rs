use std::path::{Path, PathBuf};
use async_trait::async_trait;
use crate::{
    config::GpgAgentConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

pub struct GpgAgent {
    config: GpgAgentConfig,
}

impl GpgAgent {
    pub fn new(config: GpgAgentConfig) -> Self {
        Self { config }
    }

    fn extra_socket_path(&self, ctx: &Context) -> PathBuf {
        ctx.runtime_dir.join("gnupg/S.gpg-agent.extra")
    }

    async fn check_connectable(&self, path: &Path) -> anyhow::Result<()> {
        tokio::net::UnixStream::connect(path).await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(
                "gpg-agent socket exists at {} but is not connectable: {}",
                path.display(), e
            ))
    }
}

#[async_trait]
impl Module for GpgAgent {
    fn name(&self) -> &str { "gpg_agent" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        // Prepare container GNUPGHOME in the module runtime dir
        let gnupg_dir = ctx.module_dir("gpg-agent");
        tokio::fs::create_dir_all(&gnupg_dir).await?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gnupg_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        tokio::fs::write(gnupg_dir.join("gpg.conf"), "no-autostart\n").await?;
        import_public_keys(&gnupg_dir).await?;

        // Check the host gpg-agent's extra socket
        let extra_socket = self.extra_socket_path(ctx);
        if !extra_socket.exists() {
            anyhow::bail!(
                "gpg-agent extra socket not found at {}; ensure gpg-agent is running",
                extra_socket.display()
            );
        }
        self.check_connectable(&extra_socket).await?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&gnupg_dir, ctx.home.join(".gnupg")));
        out.volumes.push(VolumeMount::new(&extra_socket, ctx.home.join(".gnupg/S.gpg-agent")));

        Ok(out)
    }
}

async fn import_public_keys(gnupg_dir: &Path) -> anyhow::Result<()> {
    use tokio::process::Command;
    use std::process::Stdio;

    let export = Command::new("gpg")
        .args(["--export", "--armor"])
        .stdout(Stdio::piped())
        .spawn()?;

    let output = export.wait_with_output().await?;
    if output.stdout.is_empty() {
        tracing::debug!("no public keys to export");
        return Ok(());
    }

    let mut import = Command::new("gpg")
        .args(["--import"])
        .env("GNUPGHOME", gnupg_dir)
        .stdin(Stdio::piped())
        .spawn()?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = import.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("gpg --import stdin not available"))?;
        stdin.write_all(&output.stdout).await?;
        // stdin dropped here, closing the write end before wait()
    }

    let status = import.wait().await?;
    if !status.success() {
        anyhow::bail!("gpg --import failed with {}", status);
    }

    trust_imported_keys(gnupg_dir).await
}

async fn trust_imported_keys(gnupg_dir: &Path) -> anyhow::Result<()> {
    use tokio::process::Command;
    use std::process::Stdio;

    let fp_output = Command::new("gpg")
        .args(["--batch", "--with-colons", "--fingerprint"])
        .env("GNUPGHOME", gnupg_dir)
        .stdout(Stdio::piped())
        .spawn()?
        .wait_with_output()
        .await?;

    let trust_input: String = String::from_utf8_lossy(&fp_output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            if fields.next()? == "fpr" {
                let fp = fields.nth(8)?;
                if !fp.is_empty() {
                    return Some(format!("{fp}:6:\n"));
                }
            }
            None
        })
        .collect();

    if trust_input.is_empty() {
        return Ok(());
    }

    let mut trust_cmd = Command::new("gpg")
        .args(["--batch", "--import-ownertrust"])
        .env("GNUPGHOME", gnupg_dir)
        .stdin(Stdio::piped())
        .spawn()?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = trust_cmd.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("gpg --import-ownertrust stdin not available"))?;
        stdin.write_all(trust_input.as_bytes()).await?;
    }

    let status = trust_cmd.wait().await?;
    if !status.success() {
        anyhow::bail!("gpg --import-ownertrust failed with {}", status);
    }
    Ok(())
}
