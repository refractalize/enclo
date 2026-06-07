use std::{
    os::unix::{
        io::{AsFd, FromRawFd, OwnedFd},
        net::UnixListener,
    },
    path::PathBuf,
};

use async_trait::async_trait;
use tokio::sync::Mutex;
use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1::{self, WpSecurityContextManagerV1},
    wp_security_context_v1::{self, WpSecurityContextV1},
};

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
        Self { config, close_fd: Mutex::new(None) }
    }
}

struct SetupState {
    manager: Option<WpSecurityContextManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for SetupState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "wp_security_context_manager_v1" {
                state.manager = Some(registry.bind(name, version.min(1), qh, ()));
            }
        }
    }
}

impl Dispatch<WpSecurityContextManagerV1, ()> for SetupState {
    fn event(_: &mut Self, _: &WpSecurityContextManagerV1, event: wp_security_context_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event { _ => unreachable!() }
    }
}

impl Dispatch<WpSecurityContextV1, ()> for SetupState {
    fn event(_: &mut Self, _: &WpSecurityContextV1, event: wp_security_context_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event { _ => unreachable!() }
    }
}

/// Connects to the host compositor, registers a security context bound to
/// `socket_path`, and returns the write end of the close pipe.
/// Dropping the returned fd revokes the security context.
fn register_security_context(socket_path: PathBuf, app_id: String) -> anyhow::Result<OwnedFd> {
    let conn = Connection::connect_to_env().map_err(|e| {
        anyhow::anyhow!("failed to connect to Wayland display: {e} (is WAYLAND_DISPLAY set?)")
    })?;

    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = SetupState { manager: None };
    queue.roundtrip(&mut state)
        .map_err(|e| anyhow::anyhow!("Wayland roundtrip failed: {e}"))?;

    let manager = state.manager.take().ok_or_else(|| anyhow::anyhow!(
        "compositor does not support wp_security_context_manager_v1 — \
         upgrade your compositor (KWin 6+, sway, Hyprland) or disable the wayland module"
    ))?;

    // Bound listening socket — the compositor will accept() on its dup of this fd.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| anyhow::anyhow!("failed to bind wayland socket at {}: {e}", socket_path.display()))?;
    let listen_fd: OwnedFd = listener.into();

    // Pipe: compositor holds the read end and polls it; dropping our write end revokes the context.
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
    }
    let close_read = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
    let close_write = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };

    let security_ctx = manager.create_listener(listen_fd.as_fd(), close_read.as_fd(), &qh, ());
    security_ctx.set_sandbox_engine("enclo".into());
    security_ctx.set_app_id(app_id.clone());
    security_ctx.set_instance_id(app_id);
    security_ctx.commit();

    queue.roundtrip(&mut state)
        .map_err(|e| anyhow::anyhow!("Wayland roundtrip failed after commit: {e}"))?;

    // listen_fd and close_read drop here; compositor holds its own dups.
    Ok(close_write)
}

#[async_trait]
impl Module for Wayland {
    fn name(&self) -> &str { "wayland" }
    fn enabled(&self) -> bool { self.config.enabled }

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

        tracing::info!("wayland security context registered, socket at {}", socket_path.display());

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket_path, &container_socket));
        out.env_vars.push(("WAYLAND_DISPLAY".into(), container_socket.clone()));
        out.exec_env_vars.push(("WAYLAND_DISPLAY".into(), container_socket));

        Ok(out)
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        // Closing the write end of the pipe signals the compositor to revoke the security context.
        self.close_fd.lock().await.take();
        Ok(())
    }
}
