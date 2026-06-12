mod aws_credentials;
mod container_client;
mod dbus_proxy;
mod docker_socket;
mod drm;
mod gpg_agent;
mod nvidia;
mod podman_socket;
mod secrets;
mod ssh_agent;
mod support;
mod wayland;
mod xdg_open;

pub use aws_credentials::AwsCredentials;
pub use container_client::ContainerClient;
pub use dbus_proxy::DbusProxy;
pub use docker_socket::DockerSocket;
pub use drm::Drm;
pub use gpg_agent::GpgAgent;
pub use nvidia::Nvidia;
pub use podman_socket::PodmanSocket;
pub use secrets::Secrets;
pub use ssh_agent::SshAgent;
pub use wayland::Wayland;
pub use xdg_open::XdgOpen;

use std::sync::Arc;
use crate::{config::Config, module::Module};

pub fn build_registry(config: &Config) -> Vec<Arc<dyn Module>> {
    let m = &config.modules;
    vec![
        Arc::new(PodmanSocket::new(m.podman_socket.clone())),
        Arc::new(DockerSocket::new(m.docker_socket.clone())),
        Arc::new(DbusProxy::new(m.dbus_proxy.clone())),
        Arc::new(GpgAgent::new(m.gpg_agent.clone())),
        Arc::new(SshAgent::new(m.ssh_agent.clone())),
        Arc::new(Nvidia::new(m.nvidia.clone())),
        Arc::new(ContainerClient::new(m.container_client.clone())),
        Arc::new(Secrets::new(m.secrets.clone(), config.container.network.clone())),
        Arc::new(AwsCredentials::new(m.aws_credentials.clone())),
        Arc::new(XdgOpen::new(m.xdg_open.clone())),
        Arc::new(Drm::new(m.drm.clone())),
        Arc::new(Wayland::new(m.wayland.clone())),
    ]
}
