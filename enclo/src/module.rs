use crate::context::Context;
use async_trait::async_trait;
use std::ffi::OsString;

#[derive(Debug, Default)]
pub struct ModuleOutput {
    /// Activate modules after Podman has initialized the real container network.
    pub requires_container_init: bool,
    /// Override the default container command (for an init system).
    pub container_command: Option<Vec<String>>,
    pub env_vars: Vec<(String, String)>,
    pub volumes: Vec<VolumeMount>,
    pub podman_args: Vec<OsString>,
    pub exec_env_vars: Vec<(String, String)>,
    pub post_start_cmds: Vec<PostStartCmd>,
    /// Required module setup, run before central hooks and home initialization.
    pub required_post_start_cmds: Vec<PostStartCmd>,
}

impl ModuleOutput {
    pub fn merge(&mut self, other: ModuleOutput) {
        self.requires_container_init |= other.requires_container_init;
        if other.container_command.is_some() {
            self.container_command = other.container_command;
        }
        self.env_vars.extend(other.env_vars);
        self.volumes.extend(other.volumes);
        self.podman_args.extend(other.podman_args);
        self.exec_env_vars.extend(other.exec_env_vars);
        self.post_start_cmds.extend(other.post_start_cmds);
        self.required_post_start_cmds.extend(other.required_post_start_cmds);
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostStartCmd {
    pub args: Vec<String>,
    pub user: Option<String>,
}

#[derive(Debug, Default)]
pub struct PreExecOutput {
    pub env_vars: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct VolumeMount {
    pub host_path: OsString,
    pub container_path: OsString,
    pub options: Option<String>,
}

impl VolumeMount {
    pub fn new(host: impl Into<OsString>, container: impl Into<OsString>) -> Self {
        Self {
            host_path: host.into(),
            container_path: container.into(),
            options: None,
        }
    }

    pub fn with_options(mut self, options: impl Into<String>) -> Self {
        self.options = Some(options.into());
        self
    }

    pub fn to_arg(&self) -> OsString {
        let mut arg = self.host_path.clone();
        let colon: OsString = ":".into();
        arg.push(&colon);
        arg.push(&self.container_path);
        if let Some(opts) = &self.options {
            arg.push(":");
            arg.push(opts);
        }
        arg
    }
}

pub struct ModuleStatus {
    /// None = no managed process; Some(true) = process alive; Some(false) = process exited
    pub process_alive: Option<bool>,
    /// Exec session IDs (from `podman inspect .ExecIDs`) that this module started and manages.
    pub managed_exec_ids: Vec<String>,
}

#[async_trait]
pub trait Module: Send + Sync {
    fn name(&self) -> &str;

    fn dependencies(&self) -> &[&str] {
        &[]
    }

    fn enabled(&self) -> bool {
        true
    }

    fn can_reload(&self) -> bool {
        false
    }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput>;

    /// Called after container init and before its command is started.
    async fn activate(&self, _ctx: &Context) -> anyhow::Result<()> {
        Ok(())
    }

    async fn teardown(&self, ctx: &Context) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Called after the container is confirmed running. Use this for anything
    /// that requires the container to be up (e.g. podman exec).
    async fn post_start(&self, ctx: &Context) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    async fn pre_exec(&self, ctx: &Context) -> anyhow::Result<PreExecOutput> {
        let _ = ctx;
        Ok(PreExecOutput::default())
    }

    async fn reload(&self, ctx: &Context) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus {
            process_alive: None,
            managed_exec_ids: Vec::new(),
        }
    }
}
