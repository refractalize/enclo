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
        Arc::new(PodmanSocket::new(PodmanSocketConfig::clone_from(&config.podman_socket))),
        Arc::new(DockerSocket::new(DockerSocketConfig::clone_from(&config.docker_socket))),
        Arc::new(DbusProxy::new(DbusProxyConfig::clone_from(&config.dbus_proxy))),
        Arc::new(GpgAgent::new(GpgAgentConfig::clone_from(&config.gpg_agent))),
        Arc::new(SshAgent::new(SshAgentConfig::clone_from(&config.ssh_agent))),
        Arc::new(Nvidia::new(NvidiaConfig::clone_from(&config.nvidia))),
        Arc::new(InsectjarProxy::new(config.proxy.clone())),
        Arc::new(Secrets::new(config.secrets.clone())),
        Arc::new(AwsCredentials::new(config.aws_credentials.clone())),
    ]
}

use crate::config::{
    DbusProxyConfig, DockerSocketConfig, GpgAgentConfig, NvidiaConfig,
    PodmanSocketConfig, SshAgentConfig,
};

trait CloneConfig: Sized {
    fn clone_from(src: &Self) -> Self;
}

macro_rules! impl_clone {
    ($t:ty { $($field:ident),+ $(,)? }) => {
        impl CloneConfig for $t {
            fn clone_from(src: &Self) -> Self {
                Self { $( $field: src.$field.clone() ),+ }
            }
        }
    };
}

impl_clone!(DbusProxyConfig { enabled, allowed_interfaces, call_rules, broadcast_rules });
impl_clone!(DockerSocketConfig { enabled, socket_path });
impl_clone!(PodmanSocketConfig { enabled, socket_path });
impl_clone!(GpgAgentConfig { enabled });
impl_clone!(SshAgentConfig { enabled, backend, socket_path });
impl_clone!(NvidiaConfig { enabled });
