//! Machine-scoped persistent homes. Initialization runs before services exist.
use super::*;
use std::{num::NonZeroU32, path::Component};

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HomeConfig {
    #[serde(default)]
    pub enabled: bool,
    // Init edits are applied explicitly without recreating the container.
    #[serde(default, skip_serializing)]
    pub init: InitConfig,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct InitConfig {
    pub command: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: Timeout,
}
// A separate default preserves the same timeout whether the table is omitted or present.
#[derive(Deserialize)]
#[serde(transparent)]
pub(super) struct Timeout(NonZeroU32);
impl Default for Timeout {
    fn default() -> Self {
        default_timeout()
    }
}
fn default_timeout() -> Timeout {
    Timeout(NonZeroU32::new(300).unwrap())
}
impl HomeConfig {
    pub fn disabled(&self) -> bool {
        !self.enabled
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.enabled || self.init.command.is_none(),
            "home.init.command requires home.enabled = true"
        );
        if let Some(script) = &self.init.command {
            ensure!(
                !script.contains('\0'),
                "home initialization must not contain NUL"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HomeSpec {
    pub volume: String,
    pub path: String,
    pub uid: u32,
    pub gid: u32,
}

impl LocalContainer {
    fn home_volume_name(&self) -> String {
        format!("enclo-home-{}-{}", &self.scope[..12], self.name)
    }
    pub(super) async fn plan_home(
        &self,
        config: &Config,
        existing: Option<&serde_json::Value>,
    ) -> Result<Option<HomeSpec>> {
        if !config.home.enabled {
            return Ok(None);
        }
        let spec = if let Some(value) = existing {
            let label = value["Config"]["Labels"]["io.enclo.home"]
                .as_str()
                .context("container has no managed home; recreate it first")?;
            serde_json::from_str::<HomeSpec>(label)?
        } else {
            // Query the image account, not the host's passwd database or HOME.
            let bytes = output(podman().args([
                "run",
                "--rm",
                "--network=none",
                "--userns=keep-id",
                "--user=0:0",
                "--security-opt=label=disable",
                "--entrypoint=getent",
                &config.container.image,
                "passwd",
                &config.ssh.user,
            ]))
            .await
            .context("managed homes require getent and an existing SSH user in the image")?;
            let text = String::from_utf8(bytes)?;
            let fields: Vec<_> = text.trim_end().split(':').collect();
            ensure!(
                fields.len() == 7 && fields[0] == config.ssh.user,
                "invalid SSH account returned by image"
            );
            HomeSpec {
                volume: self.home_volume_name(),
                path: fields[5].into(),
                uid: fields[2].parse()?,
                gid: fields[3].parse()?,
            }
        };
        ensure!(
            spec.volume == self.home_volume_name(),
            "unexpected managed home volume in container metadata"
        );
        ensure!(
            spec.uid == unsafe { libc::geteuid() },
            "managed home SSH user must have the host UID for keep-id mapping"
        );
        let home = Path::new(&spec.path);
        ensure!(
            home.is_absolute()
                && home != Path::new("/")
                && !spec.path.chars().any(|c| c.is_control() || c == ':')
                && home
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
            "invalid image home directory"
        );
        for mount in &config.container.volumes {
            let parts: Vec<_> = mount.split(':').collect();
            let target = if parts.len() == 1 { parts[0] } else { parts[1] };
            let target = Path::new(target);
            ensure!(
                target.is_absolute()
                    && !target
                        .components()
                        .any(|c| matches!(c, Component::ParentDir)),
                "volume targets must be absolute without '..' when managed home is enabled"
            );
            ensure!(
                !home.starts_with(target),
                "managed home conflicts with volume at {}; remove the existing home mount",
                target.display()
            );
        }
        self.home_volume_exists(&spec).await?;
        Ok(Some(spec))
    }
    async fn home_volume_exists(&self, home: &HomeSpec) -> Result<bool> {
        let status = podman()
            .args(["volume", "exists", &home.volume])
            .status()
            .await?;
        if status.code() == Some(1) {
            return Ok(false);
        }
        ensure!(
            status.success(),
            "cannot inspect managed home volume existence"
        );
        let bytes = output(podman().args(["volume", "inspect", &home.volume])).await?;
        let volumes: Vec<serde_json::Value> = serde_json::from_slice(&bytes)?;
        let volume = volumes.first().context("missing home volume metadata")?;
        ensure!(
            volume["Labels"]["io.enclo.scope"].as_str() == Some(&self.scope)
                && volume["Labels"]["io.enclo.home"].as_str() == Some(&self.name),
            "refusing to adopt a home volume not owned by this machine configuration"
        );
        Ok(true)
    }
    pub(super) async fn prepare_home(&self, config: &Config, home: &HomeSpec) -> Result<()> {
        if !self.home_volume_exists(home).await? {
            output(podman().args([
                "volume",
                "create",
                "--label",
                &format!("io.enclo.scope={}", self.scope),
                "--label",
                &format!("io.enclo.home={}", self.name),
                &home.volume,
            ]))
            .await?;
            // Check the returned volume's labels even if another creator raced us.
            self.home_volume_exists(home).await?;
        }
        output(
            podman()
                .args([
                    "run",
                    "--rm",
                    "--network=none",
                    "--userns=keep-id",
                    "--user=0:0",
                    "--security-opt=label=disable",
                    "--entrypoint=/bin/sh",
                    "--volume",
                ])
                .arg(format!("{}:/enclo-home:nocopy", home.volume))
                .arg(&config.container.image)
                .args([
                    "-eu",
                    "-c",
                    "chown \"$1:$2\" /enclo-home; chmod 700 /enclo-home",
                    "enclo-home",
                    &home.uid.to_string(),
                    &home.gid.to_string(),
                ]),
        )
        .await
        .context("prepare managed home ownership")?;
        Ok(())
    }
    pub(super) async fn initialize_home(
        &self,
        config: &Config,
        home: &HomeSpec,
        value: &serde_json::Value,
        rerun: bool,
    ) -> Result<()> {
        let id = value["Id"].as_str().context("missing container ID")?;
        ensure!(
            value["Mounts"].as_array().is_some_and(|mounts| mounts
                .iter()
                .any(|m| m["Type"] == "volume"
                    && m["Name"] == home.volume
                    && m["Destination"] == home.path)),
            "running container does not have the expected managed home volume"
        );
        // An account edited inside an existing container must not initialize a
        // different, nonpersistent home or run with the wrong group.
        let account =
            output(podman().args(["exec", "--user=0", id, "getent", "passwd", &config.ssh.user]))
                .await?;
        let text = String::from_utf8(account)?;
        let fields: Vec<_> = text.trim_end().split(':').collect();
        ensure!(
            fields.len() == 7
                && fields[2] == home.uid.to_string()
                && fields[3] == home.gid.to_string()
                && fields[5] == home.path,
            "SSH account differs from the managed home metadata; recreate the machine"
        );
        let Some(script) = &config.home.init.command else {
            ensure!(!rerun, "no home.init.command is configured");
            return Ok(());
        };
        let digest = format!("{:x}", Sha256::digest(script.as_bytes()));
        let wrapper = r#"umask 077
mkdir -p .enclo-init
if [ "$1" = rerun ]; then rm -f .enclo-init/central; fi
if [ -f .enclo-init/central ]; then
    if [ "$(cat .enclo-init/central)" != "$3" ]; then
        printf '%s\n' 'Home initialization changed; keeping completed state. Use enclo host home-init NAME --rerun to apply it.' >&2
    fi
    exit 0
fi
sh -eu -c "$2"
printf '%s' "$3" > .enclo-init/central.tmp
mv .enclo-init/central.tmp .enclo-init/central
"#;
        let mut command = podman();
        command
            .args(["exec", "--user"])
            .arg(format!("{}:{}", home.uid, home.gid))
            .arg("--env")
            .arg(format!("HOME={}", home.path))
            .arg("--env")
            .arg(format!("USER={}", config.ssh.user))
            .arg("--env")
            .arg(format!("LOGNAME={}", config.ssh.user))
            .arg("--workdir")
            .arg(&home.path)
            .arg(id)
            .args([
                "sh",
                "-eu",
                "-c",
                wrapper,
                "enclo-home-init",
                if rerun { "rerun" } else { "once" },
                script,
                &digest,
            ])
            .kill_on_drop(true)
            .stdin(Stdio::null());
        let result = tokio::time::timeout(
            Duration::from_secs(config.home.init.timeout_secs.0.get().into()),
            command.output(),
        )
        .await;
        let output = match result {
            Ok(Ok(out)) if out.status.success() => out,
            other => {
                // Killing a Podman exec client does not reliably stop its target
                // process. Stop the machine to prevent timed-out init continuing.
                let stopped = output(podman().args(["stop", id])).await;
                if let Err(error) = stopped {
                    bail!("home initialization failed and stopping the machine failed: {error:#}");
                }
                match other {
                    Err(_) => bail!("home initialization timed out; container stopped; no checkpoint saved"),
                    Ok(Err(error)) => return Err(error).context("home initialization failed; container stopped"),
                    Ok(Ok(out)) => bail!("home initialization failed ({}); container stopped; no checkpoint saved:\n{}", out.status, String::from_utf8_lossy(&out.stderr)),
                }
            }
        };
        if !output.stdout.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }
    pub async fn home_init(&self, rerun: bool) -> Result<Endpoint> {
        let config = self.config()?;
        ensure!(config.home.enabled, "managed home is not enabled");
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
            "container configuration changed; recreate before initializing its home"
        );
        let home = self.plan_home(&config, Some(&value)).await?.unwrap();
        self.initialize_home(&config, &home, &value, rerun).await?;
        self.endpoint(&value)
    }
}
