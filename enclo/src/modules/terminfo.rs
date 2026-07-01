use async_trait::async_trait;

use crate::{
    config::TerminfoConfig,
    context::Context,
    module::{Module, ModuleOutput, PreExecOutput},
    podman,
};

const TERMINFO_DIR: &str = "/run/enclo/terminfo";

pub struct Terminfo {
    config: TerminfoConfig,
}

impl Terminfo {
    pub fn new(config: TerminfoConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Module for Terminfo {
    fn name(&self) -> &str { "terminfo" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        Ok(ModuleOutput::default())
    }

    async fn pre_exec(&self, ctx: &Context) -> anyhow::Result<PreExecOutput> {
        let term = match std::env::var("TERM") {
            Ok(t) if !t.is_empty() => t,
            _ => return Ok(PreExecOutput::default()),
        };

        let mut out = PreExecOutput::default();
        out.env_vars.push(("TERM".into(), term.clone()));

        let infocmp = tokio::process::Command::new("infocmp")
            .args(["-x", &term])
            .output()
            .await;

        let infocmp_output = match infocmp {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => o.stdout,
            Ok(o) => {
                tracing::debug!("infocmp -x {} failed ({}), skipping terminfo install", term, o.status);
                return Ok(out);
            }
            Err(e) => {
                tracing::debug!("infocmp not available: {e}, skipping terminfo install");
                return Ok(out);
            }
        };

        let install_cmd = format!("mkdir -p {TERMINFO_DIR} && tic -o {TERMINFO_DIR} -x - 2>/dev/null");

        match podman::exec_as_with_stdin(
            &ctx.container_name,
            "root",
            &["sh", "-c", &install_cmd],
            infocmp_output,
        ).await {
            Ok(()) => {
                tracing::debug!("installed terminfo for {} at {}", term, TERMINFO_DIR);
                out.env_vars.push(("TERMINFO".into(), TERMINFO_DIR.into()));
            }
            Err(e) => {
                tracing::warn!("failed to install terminfo for {}: {:#}", term, e);
            }
        }

        Ok(out)
    }
}
