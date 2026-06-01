use async_trait::async_trait;

use crate::{
    config::DrmConfig,
    context::Context,
    module::{Module, ModuleOutput},
};

pub struct Drm {
    config: DrmConfig,
}

impl Drm {
    pub fn new(config: DrmConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Module for Drm {
    fn name(&self) -> &str { "drm" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let mut render_nodes = Vec::new();
        let mut dir = tokio::fs::read_dir("/dev/dri").await
            .map_err(|e| anyhow::anyhow!("failed to read /dev/dri: {e}"))?;
        while let Some(entry) = dir.next_entry().await? {
            if entry.file_name().to_string_lossy().starts_with("renderD") {
                render_nodes.push(entry.path());
            }
        }

        if render_nodes.is_empty() {
            anyhow::bail!("no DRM render nodes found in /dev/dri");
        }

        render_nodes.sort();

        let mut out = ModuleOutput::default();
        for path in &render_nodes {
            if std::fs::OpenOptions::new().read(true).open(path).is_err() {
                tracing::warn!(
                    "cannot open {}: user may not be in the 'render' group — \
                     GPU access inside the container may fail",
                    path.display()
                );
            }
            tracing::info!("passing DRM render node: {}", path.display());
            out.podman_args.push(format!("--device={}", path.display()).into());
        }

        Ok(out)
    }
}
