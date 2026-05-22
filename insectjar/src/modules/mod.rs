mod agent_vault;
mod dbus_proxy;
mod docker_socket;
mod gpg_agent;
mod nvidia;
mod podman_socket;
mod ssh_agent;

pub use agent_vault::AgentVault;
pub use dbus_proxy::DbusProxy;
pub use docker_socket::DockerSocket;
pub use gpg_agent::GpgAgent;
pub use nvidia::Nvidia;
pub use podman_socket::PodmanSocket;
pub use ssh_agent::SshAgent;

use crate::{config::ModulesConfig, module::Module};

pub fn build_registry(config: &ModulesConfig) -> Vec<Box<dyn Module>> {
    vec![
        Box::new(PodmanSocket::new(PodmanSocketConfig::clone_from(&config.podman_socket))),
        Box::new(DockerSocket::new(DockerSocketConfig::clone_from(&config.docker_socket))),
        Box::new(AgentVault::new(AgentVaultConfig::clone_from(&config.agent_vault))),
        Box::new(DbusProxy::new(DbusProxyConfig::clone_from(&config.dbus_proxy))),
        Box::new(GpgAgent::new(GpgAgentConfig::clone_from(&config.gpg_agent))),
        Box::new(SshAgent::new(SshAgentConfig::clone_from(&config.ssh_agent))),
        Box::new(Nvidia),
    ]
}

// Bring config types into scope for the registry builder
use crate::config::{
    AgentVaultConfig, DbusProxyConfig, DockerSocketConfig, GpgAgentConfig, PodmanSocketConfig,
    SshAgentConfig,
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

impl_clone!(AgentVaultConfig { image, api_url, proxy_url, master_password_command, token_command, data_volume, ports, env });
impl_clone!(DbusProxyConfig { allowed_interfaces, call_rules, broadcast_rules });
impl_clone!(DockerSocketConfig { socket_path });
impl_clone!(PodmanSocketConfig { socket_path });
impl CloneConfig for GpgAgentConfig {
    fn clone_from(_src: &Self) -> Self { Self {} }
}
impl_clone!(SshAgentConfig { backend, socket_path });
