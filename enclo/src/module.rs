use std::ffi::OsString;
use async_trait::async_trait;
use crate::context::Context;

#[derive(Debug, Default)]
pub struct ModuleOutput {
    pub env_vars: Vec<(String, String)>,
    pub volumes: Vec<VolumeMount>,
    pub podman_args: Vec<OsString>,
    pub exec_env_vars: Vec<(String, String)>,
    pub post_start_cmds: Vec<PostStartCmd>,
}

impl ModuleOutput {
    pub fn merge(&mut self, other: ModuleOutput) {
        self.env_vars.extend(other.env_vars);
        self.volumes.extend(other.volumes);
        self.podman_args.extend(other.podman_args);
        self.exec_env_vars.extend(other.exec_env_vars);
        self.post_start_cmds.extend(other.post_start_cmds);
    }
}

#[derive(Debug)]
pub struct PostStartCmd {
    pub args: Vec<String>,
    pub user: Option<String>,
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

    fn enabled(&self) -> bool { true }

    fn can_reload(&self) -> bool { false }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput>;

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

    async fn reload(&self, ctx: &Context) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus { process_alive: None, managed_exec_ids: Vec::new() }
    }
}
