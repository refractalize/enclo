use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub container: ContainerConfig,
    pub modules: ModulesConfig,
}

#[derive(Debug, Deserialize)]
pub struct ContainerConfig {
    pub name: String,
    pub image: String,
    #[serde(default = "default_network")]
    pub network: String,
    #[serde(default = "default_userns")]
    pub userns: String,
    #[serde(default)]
    pub keep_groups: bool,
    #[serde(default = "default_exec_shell")]
    pub exec_shell: String,
    #[serde(default)]
    pub security_label_disable: bool,
    /// Additional volume mounts in `host:container[:options]` format.
    /// Supports %h (home dir) and %t (XDG_RUNTIME_DIR) specifiers.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Command to run as PID 1 in the container.
    /// Defaults to a shell that exits cleanly on SIGTERM.
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    /// Working directory for `exec`. Supports %h and %t specifiers.
    /// Defaults to the current working directory on the host.
    pub exec_workdir: Option<String>,
}

fn default_network() -> String { "host".into() }
fn default_command() -> Vec<String> {
    vec!["sh".into(), "-c".into(), "trap 'exit 0' TERM INT; while true; do sleep 1 & wait $!; done".into()]
}
fn default_userns() -> String { "keep-id".into() }
fn default_exec_shell() -> String { "bash".into() }

#[derive(Debug, Deserialize, Default)]
pub struct ModulesConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub agent_vault: AgentVaultConfig,
    #[serde(default)]
    pub gpg_agent: GpgAgentConfig,
    #[serde(default)]
    pub ssh_agent: SshAgentConfig,
    #[serde(default)]
    pub dbus_proxy: DbusProxyConfig,
    #[serde(default)]
    pub docker_socket: DockerSocketConfig,
    #[serde(default)]
    pub podman_socket: PodmanSocketConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct PodmanSocketConfig {
    /// Overrides the default of $XDG_RUNTIME_DIR/podman/podman.sock
    pub socket_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DockerSocketConfig {
    #[serde(default = "default_docker_socket_path")]
    pub socket_path: String,
}

impl Default for DockerSocketConfig {
    fn default() -> Self {
        Self { socket_path: default_docker_socket_path() }
    }
}

fn default_docker_socket_path() -> String {
    "/var/run/docker.sock".into()
}

#[derive(Debug, Deserialize, Default)]
pub struct AgentVaultConfig {
    #[serde(default = "default_agent_vault_image")]
    pub image: String,
    /// HTTP URL of the management API (health check, CA cert). Default: http://localhost:14321
    #[serde(default = "default_agent_vault_api_url")]
    pub api_url: String,
    /// HTTPS URL of the transparent proxy, injected as HTTP_PROXY/HTTPS_PROXY. Default: https://localhost:14322
    #[serde(default = "default_agent_vault_proxy_url")]
    pub proxy_url: String,
    /// Shell command whose stdout is the master password (e.g. a pass/secret-tool lookup).
    /// Required to unlock the vault on startup.
    pub master_password_command: Option<String>,
    /// Shell command whose stdout is the agent-vault API token.
    /// When set, the token is injected into the proxy URL as https://<token>@host:port.
    pub token_command: Option<String>,
    /// Named podman volume for vault data persistence.
    #[serde(default = "default_agent_vault_volume")]
    pub data_volume: String,
    /// Localhost ports to publish: management port and proxy port.
    #[serde(default = "default_agent_vault_ports")]
    pub ports: Vec<String>,
    /// Additional environment variables to pass to the agent-vault container.
    #[serde(default)]
    pub env: Vec<String>,
}

fn default_agent_vault_image() -> String { "docker.io/infisical/agent-vault".into() }
fn default_agent_vault_api_url() -> String { "http://localhost:14321".into() }
fn default_agent_vault_proxy_url() -> String { "https://localhost:14322".into() }
fn default_agent_vault_volume() -> String { "agent-vault-data".into() }
fn default_agent_vault_ports() -> Vec<String> {
    vec![
        "127.0.0.1:14321:14321".into(),
        "127.0.0.1:14322:14322".into(),
    ]
}

#[derive(Debug, Deserialize, Default)]
pub struct GpgAgentConfig {}

#[derive(Debug, Deserialize, Default)]
pub struct SshAgentConfig {
    /// Which SSH agent backend to expose into the container.
    /// "gpg_agent" (default) uses the GPG agent's SSH socket.
    /// "standalone" uses a regular ssh-agent socket.
    #[serde(default)]
    pub backend: SshBackend,
    /// Override the host socket path.
    /// gpg_agent default: $XDG_RUNTIME_DIR/gnupg/S.gpg-agent.ssh
    /// standalone default: $SSH_AUTH_SOCK environment variable
    pub socket_path: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SshBackend {
    #[default]
    GpgAgent,
    Standalone,
}



#[derive(Debug, Deserialize, Default)]
pub struct DbusProxyConfig {
    /// Names granted TALK policy (--talk=NAME). Allows sending method calls and receiving broadcasts.
    #[serde(default = "default_dbus_interfaces")]
    pub allowed_interfaces: Vec<String>,
    /// Fine-grained call rules (--call=NAME=RULE). Format: "org.example.Name=method@/path".
    #[serde(default)]
    pub call_rules: Vec<String>,
    /// Fine-grained broadcast rules (--broadcast=NAME=RULE). Format: "org.example.Name=signal@/path".
    #[serde(default)]
    pub broadcast_rules: Vec<String>,
}

fn default_dbus_interfaces() -> Vec<String> {
    vec!["org.freedesktop.portal.Desktop".into()]
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {}", path.display(), e))?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {}", path.display(), e))?;
        Ok(config)
    }

}
