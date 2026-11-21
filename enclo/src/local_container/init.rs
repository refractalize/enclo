//! Initialization scoped to the container instance, not the persistent home.
use super::*;
use std::num::NonZeroU32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(super) struct InitConfig {
    command: Option<String>,
    user: String,
    timeout_secs: NonZeroU32,
}
impl Default for InitConfig {
    fn default() -> Self {
        Self {
            command: None,
            user: "root".into(),
            timeout_secs: NonZeroU32::new(60).unwrap(),
        }
    }
}
impl InitConfig {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            !self.user.is_empty()
                && !self.user.starts_with('-')
                && self
                    .user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "container.init.user must be an account name or UID"
        );
        if let Some(script) = &self.command {
            ensure!(
                !script.contains('\0'),
                "container.init.command must not contain NUL"
            );
        }
        Ok(())
    }
}

// Always access the checkpoint as container root, independently of the hook user.
// Including the instance ID also prevents a mounted/copied marker surviving recreation.
const CHECKPOINT: &str = r#"umask 077
d=/var/lib/enclo
test ! -L "$d"
mkdir -p "$d"
test ! -L "$d/container-init"
case "$1" in
read) if [ -f "$d/container-init" ]; then cat "$d/container-init"; fi ;;
clear) rm -f "$d/container-init" ;;
write) tmp=$(mktemp "$d/container-init.XXXXXX"); printf '%s' "$2" > "$tmp"; mv "$tmp" "$d/container-init" ;;
esac
"#;

impl LocalContainer {
    async fn init_checkpoint(&self, id: &str, action: &str, stamp: &str) -> Result<Vec<u8>> {
        output(podman().args([
            "exec",
            "--user=0",
            id,
            "sh",
            "-eu",
            "-c",
            CHECKPOINT,
            "enclo-container-checkpoint",
            action,
            stamp,
        ]))
        .await
    }

    pub(super) async fn initialize_container(
        &self,
        config: &Config,
        value: &serde_json::Value,
        rerun: bool,
    ) -> Result<()> {
        let init = &config.container.init;
        let Some(script) = &init.command else {
            ensure!(!rerun, "no container.init.command is configured");
            return Ok(());
        };
        let id = value["Id"].as_str().context("missing container ID")?;
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(&init.user, script))?)
        );
        let stamp = format!("{id}:{digest}");
        let result: Result<()> = async {
            let old = self.init_checkpoint(id, "read", "").await?;
            let old = String::from_utf8(old)?;
            if !rerun && old.starts_with(&format!("{id}:")) {
                if old != stamp {
                    eprintln!("Container initialization changed; keeping completed state. Use enclo host init NAME --rerun to apply it.");
                }
                return Ok(());
            }
            self.init_checkpoint(id, "clear", "").await?;
            let account = output(podman().args(["exec", "--user=0", id, "getent", "passwd", &init.user])).await?;
            let account = String::from_utf8(account)?;
            let fields: Vec<_> = account.trim_end().split(':').collect();
            ensure!(fields.len() == 7 && fields[5].starts_with('/'), "container.init.user must identify an existing account with an absolute home");
            let uid: u32 = fields[2].parse()?;
            let gid: u32 = fields[3].parse()?;
            let mut cmd = podman();
            cmd.args(["exec", "--user"]).arg(format!("{uid}:{gid}"))
                .arg("--env").arg(format!("HOME={}", fields[5]))
                .arg("--env").arg(format!("USER={}", fields[0]))
                .arg("--env").arg(format!("LOGNAME={}", fields[0]))
                .args(["--workdir", "/", id, "sh", "-eu", "-c", script, "enclo-container-init"])
                .stdin(Stdio::null()).kill_on_drop(true);
            let out = tokio::time::timeout(Duration::from_secs(init.timeout_secs.get().into()), cmd.output())
                .await.context("container initialization timed out")??;
            ensure!(out.status.success(), "container initialization failed ({}):\n{}", out.status, String::from_utf8_lossy(&out.stderr));
            if !out.stdout.is_empty() { eprint!("{}", String::from_utf8_lossy(&out.stdout)); }
            if !out.stderr.is_empty() { eprint!("{}", String::from_utf8_lossy(&out.stderr)); }
            self.init_checkpoint(id, "write", &stamp).await?;
            Ok(())
        }.await;
        if let Err(error) = result {
            output(podman().args(["stop", id]))
                .await
                .context("container initialization failed and stopping the container failed")?;
            return Err(error).context("container initialization failed; container stopped; initialization will be retried on the next start");
        }
        Ok(())
    }

    pub async fn container_init(&self, rerun: bool) -> Result<Endpoint> {
        let config = self.config()?;
        ensure!(
            config.container.init.command.is_some(),
            "no container.init.command is configured"
        );
        let _lock = self.lock()?;
        let value = self
            .inspect()
            .await?
            .context("container does not exist; start it first")?;
        ensure!(
            value["State"]["Running"] == true,
            "container is stopped; start it first"
        );
        let revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&config)?));
        ensure!(
            value["Config"]["Labels"]["io.enclo.config"] == revision,
            "container configuration changed; recreate before initializing it"
        );
        if let Some(home) = self.plan_home(&config, Some(&value)).await? {
            self.initialize_home(&config, &home, &value, false).await?;
        }
        self.initialize_container(&config, &value, rerun).await?;
        self.endpoint(&value)
    }
}
