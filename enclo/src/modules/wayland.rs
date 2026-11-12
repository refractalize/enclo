use async_trait::async_trait;
use enclo::wayland_security::register_security_context;
use std::os::unix::io::OwnedFd;
use tokio::sync::Mutex;

use crate::{
    config::WaylandConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

const WAYLAND_SOCKET_NAME: &str = "wayland-0";

pub struct Wayland {
    config: WaylandConfig,
    close_fd: Mutex<Option<OwnedFd>>,
}

impl Wayland {
    pub fn new(config: WaylandConfig) -> Self {
        Self {
            config,
            close_fd: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Module for Wayland {
    fn name(&self) -> &str {
        "wayland"
    }
    fn enabled(&self) -> bool {
        self.config.enabled
    }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let socket_path = ctx.enclo_dir.join("wayland.sock");
        let container_socket = match &self.config.socket_path {
            Some(p) => p.clone(),
            None => {
                let xdg = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
                    anyhow::anyhow!(
                        "wayland module requires XDG_RUNTIME_DIR to be set; \
                         set socket_path explicitly to override"
                    )
                })?;
                format!("{xdg}/{WAYLAND_SOCKET_NAME}")
            }
        };
        let app_id = ctx.container_name.clone();

        let socket_path_clone = socket_path.clone();
        let close_write = tokio::task::spawn_blocking(move || {
            register_security_context(socket_path_clone, app_id)
        })
        .await
        .map_err(|e| anyhow::anyhow!("wayland setup task panicked: {e}"))??;

        *self.close_fd.lock().await = Some(close_write);

        tracing::info!(
            "wayland security context registered, socket at {}",
            socket_path.display()
        );

        let mut out = ModuleOutput::default();
        out.volumes
            .push(VolumeMount::new(&socket_path, &container_socket));
        out.env_vars
            .push(("WAYLAND_DISPLAY".into(), container_socket.clone()));
        out.exec_env_vars
            .push(("WAYLAND_DISPLAY".into(), container_socket));

        Ok(out)
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        // Closing the write end of the pipe signals the compositor to revoke the security context.
        self.close_fd.lock().await.take();
        Ok(())
    }
}
