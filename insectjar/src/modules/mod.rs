mod aws_credentials;
mod dbus_proxy;
mod docker_socket;
mod gpg_agent;
mod insectjar_proxy;
mod nvidia;
mod podman_socket;
mod secrets;
mod ssh_agent;

pub use aws_credentials::AwsCredentials;
pub use dbus_proxy::DbusProxy;
pub use docker_socket::DockerSocket;
pub use gpg_agent::GpgAgent;
pub use insectjar_proxy::InsectjarProxy;
pub use nvidia::Nvidia;
pub use podman_socket::PodmanSocket;
pub use secrets::Secrets;
pub use ssh_agent::SshAgent;

use std::sync::Arc;
use crate::{config::ModulesConfig, module::Module};

pub fn build_registry(config: &ModulesConfig) -> Vec<Arc<dyn Module>> {
    vec![
        Arc::new(PodmanSocket::new(config.podman_socket.clone())),
        Arc::new(DockerSocket::new(config.docker_socket.clone())),
        Arc::new(DbusProxy::new(config.dbus_proxy.clone())),
        Arc::new(GpgAgent::new(config.gpg_agent.clone())),
        Arc::new(SshAgent::new(config.ssh_agent.clone())),
        Arc::new(Nvidia::new(config.nvidia.clone())),
        Arc::new(InsectjarProxy::new(config.proxy.clone())),
        Arc::new(Secrets::new(config.secrets.clone())),
        Arc::new(AwsCredentials::new(config.aws_credentials.clone())),
    ]
}
