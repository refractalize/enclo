use crate::{
    config::Config,
    context::Context,
    module::{Module, ModuleOutput, PostStartCmd},
    platform::Platform,
};
use async_trait::async_trait;

pub struct Systemd {
    enabled: bool,
    command: Vec<String>,
    userns: String,
    label_disabled: bool,
    security_opts: Vec<String>,
}
impl Systemd {
    pub fn new(config: &Config) -> Self {
        Self {
            enabled: config.modules.systemd.enabled,
            command: config.container.command.clone(),
            userns: config.container.userns.clone(),
            label_disabled: config.container.security_label_disable,
            security_opts: config.container.security_opts.clone(),
        }
    }
}
#[async_trait]
impl Module for Systemd {
    fn name(&self) -> &str {
        "systemd"
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        anyhow::ensure!(
            ctx.platform == Platform::Linux,
            "systemd currently requires a Linux host"
        );
        anyhow::ensure!(
            self.userns == "keep-id",
            "systemd requires container.userns = \"keep-id\""
        );
        anyhow::ensure!(
            self.command == crate::config::default_command() || self.command == ["/sbin/init"],
            "systemd controls PID 1; remove container.command or set it to ['/sbin/init']"
        );
        let mut out = ModuleOutput::default();
        out.container_command = Some(vec!["/sbin/init".into()]);
        out.podman_args = ["--user=0:0", "--systemd=always", "--cgroupns=private"]
            .into_iter()
            .map(Into::into)
            .collect();
        anyhow::ensure!(
            !self.security_opts.iter().any(|option| option.starts_with("label=") && option != "label=disable"),
            "systemd requires disabled SELinux container labeling; remove the conflicting label setting"
        );
        if !self.label_disabled
            && !self
                .security_opts
                .iter()
                .any(|option| option == "label=disable")
        {
            out.podman_args.push("--security-opt=label=disable".into());
        }
        // A container can legitimately be degraded (for example, hardware units).
        // Check the state explicitly: exit code 1 also covers genuine failures.
        out.required_post_start_cmds.push(PostStartCmd {
            user: Some("root".into()),
            args: vec!["sh".into(), "-c".into(), "state=$(systemctl is-system-running --wait); case \"$state\" in running|degraded) exit 0;; *) echo \"systemd did not become ready: $state\" >&2; exit 1;; esac".into()],
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn init_is_root_and_readiness_is_required() {
        let config: Config = toml::from_str("[container]\nimage='fixture'\n[modules.systemd]\nenabled=true\n[modules.nested_podman]\nenabled=true").unwrap();
        let mut ctx = Context::new("fixture".into()).unwrap();
        ctx.platform = Platform::Linux;
        ctx.uid = 1000;
        ctx.gid = 1000;
        let out = Systemd::new(&config).setup(&ctx).await.unwrap();
        assert_eq!(out.container_command.unwrap(), ["/sbin/init"]);
        assert!(out.podman_args.iter().any(|arg| arg == "--user=0:0"));
        assert_eq!(out.required_post_start_cmds.len(), 1);
        let nested = crate::modules::NestedPodman::new(&config)
            .setup(&ctx)
            .await
            .unwrap();
        assert!(!nested
            .podman_args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("--user=")));
        let mut conflicting: Config = toml::from_str("[container]\nimage='fixture'\ncommand=['sleep','infinity']\n[modules.systemd]\nenabled=true").unwrap();
        assert!(Systemd::new(&conflicting).setup(&ctx).await.is_err());
        conflicting.container.command = vec!["/sbin/init".into()];
        assert!(Systemd::new(&conflicting).setup(&ctx).await.is_ok());
        ctx.platform = Platform::MacOs;
        assert!(Systemd::new(&config).setup(&ctx).await.is_err());
    }
}
