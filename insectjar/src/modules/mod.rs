mod dbus_proxy;
mod docker_socket;
mod gpg_agent;
mod insectjar_proxy;
mod nvidia;
mod podman_socket;
mod ssh_agent;

pub use dbus_proxy::DbusProxy;
pub use docker_socket::DockerSocket;
pub use gpg_agent::GpgAgent;
pub use insectjar_proxy::InsectjarProxy;
pub use nvidia::Nvidia;
pub use podman_socket::PodmanSocket;
pub use ssh_agent::SshAgent;

use crate::{config::ModulesConfig, module::Module};

pub fn build_registry(config: &ModulesConfig) -> Vec<Box<dyn Module>> {
    vec![
        Box::new(PodmanSocket::new(PodmanSocketConfig::clone_from(&config.podman_socket))),
        Box::new(DockerSocket::new(DockerSocketConfig::clone_from(&config.docker_socket))),
        Box::new(DbusProxy::new(DbusProxyConfig::clone_from(&config.dbus_proxy))),
        Box::new(GpgAgent::new(GpgAgentConfig::clone_from(&config.gpg_agent))),
        Box::new(SshAgent::new(SshAgentConfig::clone_from(&config.ssh_agent))),
        Box::new(Nvidia::new(NvidiaConfig::clone_from(&config.nvidia))),
        Box::new(InsectjarProxy::new(config.proxy.clone())),
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
