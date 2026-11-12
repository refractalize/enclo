use crate::{
    config::{Config, NestedPodmanConfig},
    context::Context,
    module::{Module, ModuleOutput, PostStartCmd},
    platform::Platform,
};
use async_trait::async_trait;

pub const SETUP: &str = include_str!("nested_podman.py");
pub struct NestedPodman {
    config: NestedPodmanConfig,
    userns: String,
    host_socket: bool,
    security_opts: Vec<String>,
    devices: Vec<String>,
    label_disabled: bool,
    remote_env: bool,
    systemd: bool,
}
impl NestedPodman {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.modules.nested_podman.clone(),
            systemd: config.modules.systemd.enabled,
            userns: config.container.userns.clone(),
            host_socket: config.modules.podman_socket.enabled
                || config.modules.docker_socket.enabled,
            security_opts: config.container.security_opts.clone(),
            devices: config.container.devices.clone(),
            label_disabled: config.container.security_label_disable,
            remote_env: config.container.env.iter().any(|entry| {
                ["CONTAINER_HOST=", "CONTAINER_CONNECTION=", "DOCKER_HOST="]
                    .iter()
                    .any(|key| entry.starts_with(key))
            }),
        }
    }
}
#[async_trait]
impl Module for NestedPodman {
    fn name(&self) -> &str {
        "nested_podman"
    }
    fn dependencies(&self) -> &[&str] {
        &["systemd"]
    }
    fn enabled(&self) -> bool {
        self.config.enabled
    }
    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        anyhow::ensure!(
            ctx.platform == Platform::Linux,
            "nested_podman currently requires a Linux host"
        );
        anyhow::ensure!(
            ctx.uid != 0,
            "nested_podman requires a non-root workspace user"
        );
        anyhow::ensure!(
            self.userns == "keep-id",
            "nested_podman requires container.userns = \"keep-id\""
        );
        anyhow::ensure!(
            !self.host_socket,
            "nested_podman cannot be combined with podman_socket or docker_socket forwarding"
        );
        anyhow::ensure!(
            !self
                .security_opts
                .iter()
                .any(|option| option == "no-new-privileges" || option == "no-new-privileges=true"),
            "nested_podman needs privilege gains for the UID/GID helpers; remove no-new-privileges"
        );
        anyhow::ensure!(!self.remote_env,"nested_podman cannot use CONTAINER_HOST, CONTAINER_CONNECTION or DOCKER_HOST in container.env");
        let mut out = ModuleOutput::default();
        for option in ["seccomp=unconfined", "unmask=ALL", "label=disable"] {
            let key = option.split('=').next().unwrap();
            anyhow::ensure!(!self.security_opts.iter().any(|existing|existing.starts_with(&format!("{key}=")) && existing!=option),
                "nested_podman requires {option}; remove the conflicting container.security_opts setting");
            if !self.security_opts.iter().any(|existing| existing == option)
                && !(option == "label=disable" && self.label_disabled)
            {
                out.podman_args
                    .push(format!("--security-opt={option}").into());
            }
        }
        for device in ["/dev/fuse", "/dev/net/tun"] {
            let present = self.devices.iter().any(|entry| {
                let mut parts = entry.split(':');
                parts.next() == Some(device) && parts.next().is_none_or(|target| target == device)
            });
            if !present {
                out.podman_args.push(format!("--device={device}").into());
            }
        }
        if !self.systemd {
            out.podman_args.push(format!("--user={}:{}", ctx.uid, ctx.gid).into());
        }
        let runtime = ("XDG_RUNTIME_DIR".into(), format!("/run/user/{}", ctx.uid));
        out.env_vars.push(runtime.clone());
        out.exec_env_vars.push(runtime);
        // Podman treats even an empty CONTAINER_HOST as remote mode.
        for key in ["CONTAINER_HOST", "CONTAINER_CONNECTION", "DOCKER_HOST"] {
            out.podman_args.push(format!("--unsetenv={key}").into());
        }
        out.required_post_start_cmds.push(PostStartCmd {
            args: vec![
                "python3".into(),
                "-c".into(),
                SETUP.into(),
                ctx.uid.to_string(),
                ctx.gid.to_string(),
            ],
            user: Some("root".into()),
        });
        // Validate the full local engine and mapping helpers before readiness.
        out.required_post_start_cmds.push(PostStartCmd {
            args: vec![
                "podman".into(),
                "--remote=false".into(),
                "unshare".into(),
                "true".into(),
            ],
            user: Some(format!("{}:{}", ctx.uid, ctx.gid)),
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(container: &str, modules: &str) -> Config {
        toml::from_str(&format!("[container]\nimage='fixture'\n{container}\n[modules.nested_podman]\nenabled=true\n{modules}")).unwrap()
    }
    fn context() -> Context {
        let mut c = Context::new("fixture".into()).unwrap();
        c.uid = 1000;
        c.gid = 1000;
        c.platform = Platform::Linux;
        c
    }
    #[tokio::test]
    async fn module_supplies_runtime_wiring_and_required_root_setup() {
        let c = config("", "");
        let module = NestedPodman::new(&c);
        let out = module.setup(&context()).await.unwrap();
        for arg in [
            "--device=/dev/fuse",
            "--device=/dev/net/tun",
            "--security-opt=seccomp=unconfined",
            "--security-opt=unmask=ALL",
            "--security-opt=label=disable",
            "--user=1000:1000",
        ] {
            assert!(out.podman_args.iter().any(|a| a == arg));
        }
        assert!(out
            .env_vars
            .contains(&("XDG_RUNTIME_DIR".into(), "/run/user/1000".into())));
        assert_eq!(out.required_post_start_cmds.len(), 2);
        assert_eq!(
            out.required_post_start_cmds[0].user.as_deref(),
            Some("root")
        );
        let disabled: Config = toml::from_str("[container]\nimage='fixture'\n[modules]").unwrap();
        assert!(!NestedPodman::new(&disabled).enabled());
    }
    #[tokio::test]
    async fn incompatible_modes_are_rejected_before_launch() {
        for (container, modules) in [
            ("userns='host'", ""),
            ("security_opts=['seccomp=/custom.json']", ""),
            ("security_opts=['no-new-privileges']", ""),
            ("env=['CONTAINER_HOST=']", ""),
            ("", "[modules.podman_socket]\nenabled=true"),
            ("", "[modules.docker_socket]\nenabled=true"),
        ] {
            assert!(NestedPodman::new(&config(container, modules))
                .setup(&context())
                .await
                .is_err());
        }
        let mut c = context();
        c.platform = Platform::MacOs;
        assert!(NestedPodman::new(&config("", "")).setup(&c).await.is_err());
    }
    #[test]
    fn subordinate_ranges_respect_actual_mapping_and_exclude_root_and_workspace_ids() {
        let assertions = r#"
assert mapped_ranges('0 100000 1000\n1000 1000 1\n1001 101000 64536',1000)==[(1,999),(1001,64536)]
assert mapped_ranges('0 100000 2\n10 100010 3',11)==[(1,1),(10,1),(12,1)]
try:mapped_ranges('0 100000 1',1000)
except RuntimeError:pass
else:raise AssertionError('no usable ranges')
"#;
        let code = format!("__name__='test'\n{SETUP}\n{assertions}");
        assert!(std::process::Command::new("python3")
            .args(["-c", &code])
            .status()
            .unwrap()
            .success());
    }
}
