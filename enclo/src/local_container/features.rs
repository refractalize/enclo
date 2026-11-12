use super::*;
use std::collections::BTreeMap;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NestedPodman {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "ProcMount::is_private")]
    proc_mount: ProcMount,
}
#[derive(Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ProcMount {
    #[default]
    Private,
    Host,
}
impl ProcMount {
    fn is_private(&self) -> bool {
        *self == Self::Private
    }
    fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Host => "host",
        }
    }
}
impl NestedPodman {
    pub fn disabled(&self) -> bool {
        !self.enabled
    }
}
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Gpu {
    #[serde(default)]
    pub nvidia: Vec<String>,
    #[serde(default)]
    pub drm: bool,
    #[serde(default)]
    pub drm_nodes: Vec<String>,
}
impl Gpu {
    pub fn disabled(&self) -> bool {
        !self.drm && self.nvidia.is_empty() && self.drm_nodes.is_empty()
    }
}
impl Config {
    pub(super) fn validate_features(&self) -> Result<()> {
        self.desktop.validate()?;
        ensure!(
            self.nested_podman.enabled || self.nested_podman.proc_mount.is_private(),
            "nested_podman.proc_mount requires nested_podman.enabled=true"
        );
        ensure!(
            self.gpu.drm || self.gpu.drm_nodes.is_empty(),
            "gpu.drm_nodes requires drm=true"
        );
        for node in &self.gpu.drm_nodes {
            ensure!(
                node.strip_prefix("renderD")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
                "gpu.drm_nodes must name render nodes such as renderD128"
            );
        }
        for gpu in &self.gpu.nvidia {
            ensure!(
                !gpu.is_empty()
                    && gpu
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b)),
                "gpu.nvidia must contain CDI device names such as all, 0, or a GPU UUID"
            );
        }
        if self.nested_podman.enabled {
            ensure!(
                self.home.enabled,
                "nested_podman requires home.enabled=true for persistent storage"
            );
            for entry in &self.container.env {
                ensure!(
                    ![
                        "CONTAINER_HOST",
                        "CONTAINER_CONNECTION",
                        "DOCKER_HOST",
                        "XDG_RUNTIME_DIR"
                    ]
                    .iter()
                    .any(|key| entry.split('=').next() == Some(key)),
                    "nested_podman manages local-engine environment; remove {entry}"
                );
            }
        }
        if !self.desktop.disabled() {
            for mount in &self.container.volumes {
                let target = mount.split(':').nth(1).unwrap_or(mount);
                let path = Path::new(target);
                ensure!(
                    path.is_absolute()
                        && !path
                            .components()
                            .any(|c| c == std::path::Component::ParentDir),
                    "desktop mount targets must be absolute without '..'"
                );
                ensure!(
                    !Path::new("/run/enclo-desktop").starts_with(path)
                        && !path.starts_with("/run/enclo-desktop"),
                    "volume conflicts with desktop socket directory"
                );
            }
        }
        Ok(())
    }
}
impl LocalContainer {
    pub(super) fn feature_args(&self, config: &Config, cmd: &mut Command) -> Result<()> {
        for device in &config.gpu.nvidia {
            cmd.arg(format!("--device=nvidia.com/gpu={device}"));
        }
        if config.gpu.drm {
            let nodes = if config.gpu.drm_nodes.is_empty() {
                let mut nodes = Vec::new();
                for entry in
                    fs::read_dir("/dev/dri").context("GPU DRM needs host /dev/dri render nodes")?
                {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name
                        .strip_prefix("renderD")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    {
                        nodes.push(name);
                    }
                }
                nodes.sort();
                ensure!(!nodes.is_empty(), "no host DRM render nodes found");
                nodes
            } else {
                config.gpu.drm_nodes.clone()
            };
            for node in nodes {
                cmd.arg(format!("--device=/dev/dri/{node}"));
            }
        }
        if config.gpu.drm && !config.container.keep_groups {
            cmd.arg("--group-add=keep-groups");
        }
        let uid = unsafe { libc::geteuid() };
        let mut env = BTreeMap::new();
        if config.nested_podman.enabled {
            cmd.args([
                "--security-opt=seccomp=unconfined",
                "--security-opt=unmask=ALL",
                "--device=/dev/fuse",
                "--device=/dev/net/tun",
            ]);
            for key in ["CONTAINER_HOST", "CONTAINER_CONNECTION", "DOCKER_HOST"] {
                cmd.arg(format!("--unsetenv={key}"));
            }
            cmd.arg("--env")
                .arg(format!("XDG_RUNTIME_DIR=/run/user/{uid}"));
            env.insert("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
        }
        if !config.desktop.disabled() {
            let directory = self.state.join("desktop");
            ensure!(
                directory
                    .join("sockets/wayland")
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    < 104,
                "desktop socket path too long; use a shorter --state-dir or container name"
            );
            private_dir(&directory)?;
            private_dir(&directory.join("sockets"))?;
            cmd.arg("--volume").arg(format!(
                "{}:/run/enclo-desktop:ro",
                directory.join("sockets").display()
            ));
            let plan = crate::desktop::Plan {
                directory,
                app_id: self.podman_name.clone(),
                config: config.desktop.clone(),
            };
            cmd.arg("--label").arg(format!(
                "io.enclo.desktop={}",
                serde_json::to_string(&plan)?
            ));
            if config.desktop.wayland {
                env.insert("WAYLAND_DISPLAY", "/run/enclo-desktop/wayland".into());
            }
            if config.desktop.dbus.enabled {
                env.insert(
                    "DBUS_SESSION_BUS_ADDRESS",
                    "unix:path=/run/enclo-desktop/bus".into(),
                );
            }
        }
        if !env.is_empty() {
            cmd.arg("--label").arg(format!(
                "io.enclo.session-env={}",
                serde_json::to_string(&env)?
            ));
        }
        Ok(())
    }
    pub(super) async fn setup_nested(
        &self,
        config: &Config,
        home: Option<&managed_home::HomeSpec>,
        id: &str,
    ) -> Result<()> {
        if !config.nested_podman.enabled {
            return Ok(());
        }
        let home = home.context("nested Podman needs a managed home")?;
        let result: Result<()> = async {
            output(podman().args([
                "exec",
                "--user=0",
                id,
                "python3",
                "-c",
                include_str!("../modules/nested_podman.py"),
                &home.uid.to_string(),
                &home.gid.to_string(),
                config.nested_podman.proc_mount.as_str(),
            ]))
            .await?;
            output(podman().args([
                "exec",
                "--user",
                &format!("{}:{}", home.uid, home.gid),
                "--env",
                &format!("HOME={}", home.path),
                "--env",
                &format!("XDG_RUNTIME_DIR=/run/user/{}", home.uid),
                id,
                "podman",
                "--remote=false",
                "unshare",
                "sh",
                "-eu",
                "-c",
                if config.nested_podman.proc_mount.is_private() {
                    "exec unshare --mount --pid --fork --mount-proc true"
                } else {
                    // Keep the existing proc tree, including NVIDIA's protected params.
                    r#"d=$(mktemp -d); trap 'rmdir "$d"' EXIT; unshare --mount --pid --fork sh -eu -c 'mount --rbind /proc "$1"; mount -o remount,bind,ro "$1"' sh "$d""#
                },
            ]))
            .await.context(if config.nested_podman.proc_mount.is_private() {
                "nested Podman proc mount check failed; NVIDIA or other mounts under /proc can block private proc mounts. To accept outer-container proc visibility, explicitly set [nested_podman] proc_mount = \"host\" and recreate the container"
            } else {
                "nested Podman proc mount check failed in host compatibility mode; check that unshare and mount are installed and that the outer proc tree can be bound read-only"
            })?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            output(podman().args(["stop", id]))
                .await
                .context("nested Podman setup failed and stopping the container failed")?;
            return Err(error).context("nested Podman setup failed; container stopped");
        }
        Ok(())
    }
    pub(super) fn resolve_features(
        &self,
        value: &serde_json::Value,
        profile: &mut crate::connection::ConnectionProfile,
    ) -> Result<()> {
        if let Some(label) = value["Config"]["Labels"]["io.enclo.session-env"].as_str() {
            let env: BTreeMap<String, String> = serde_json::from_str(label)?;
            for (key, value) in env {
                profile.session.env.entry(key).or_insert(value);
            }
        }
        if let Some(label) = value["Config"]["Labels"]["io.enclo.desktop"].as_str() {
            let plan: crate::desktop::Plan = serde_json::from_str(label)?;
            ensure!(
                plan.directory == self.state.join("desktop"),
                "desktop directory belongs to another state root"
            );
            plan.config.validate()?;
            profile.local_desktop = Some(plan);
        }
        Ok(())
    }
}
