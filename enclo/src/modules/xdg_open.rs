use async_trait::async_trait;
use crate::{
    config::XdgOpenConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

const SCRIPT: &str = r#"#!/bin/bash
arg="$1"
if [[ "$arg" != *://* ]]; then
  exec {fd}< "$arg"
  gdbus call --session \
    --dest org.freedesktop.portal.Desktop \
    --object-path /org/freedesktop/portal/desktop \
    --method org.freedesktop.portal.OpenURI.OpenFile \
    "" "@h $fd" {}
  exec {fd}>&-
else
  gdbus call --session \
    --dest org.freedesktop.portal.Desktop \
    --object-path /org/freedesktop/portal/desktop \
    --method org.freedesktop.portal.OpenURI.OpenURI \
    "" "$arg" {}
fi
"#;

pub struct XdgOpen {
    config: XdgOpenConfig,
}

impl XdgOpen {
    pub fn new(config: XdgOpenConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Module for XdgOpen {
    fn name(&self) -> &str { "xdg_open" }
    fn enabled(&self) -> bool { self.config.enabled }
    fn dependencies(&self) -> &[&str] { &["dbus_proxy"] }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        use std::os::unix::fs::PermissionsExt;

        let dir = ctx.module_dir("xdg-open");
        tokio::fs::create_dir_all(&dir).await?;

        let script_path = dir.join("xdg-open");
        tokio::fs::write(&script_path, SCRIPT).await?;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&script_path, "/usr/local/bin/xdg-open").with_options("ro"),
        );
        Ok(out)
    }
}
