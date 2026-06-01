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
    pub gpg_agent: GpgAgentConfig,
    #[serde(default)]
    pub ssh_agent: SshAgentConfig,
    #[serde(default)]
    pub dbus_proxy: DbusProxyConfig,
    #[serde(default)]
    pub docker_socket: DockerSocketConfig,
    #[serde(default)]
    pub podman_socket: PodmanSocketConfig,
    #[serde(default)]
    pub nvidia: NvidiaConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub aws_credentials: AwsCredentialsConfig,
    #[serde(default)]
    pub container_client: ContainerClientConfig,
    #[serde(default)]
    pub xdg_open: XdgOpenConfig,
}

#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Set $ENCLO_PROXY; traffic is only routed through the proxy when the
    /// user explicitly opts in.
    #[default]
    Explicit,
    /// Set $HTTP_PROXY/$HTTPS_PROXY directly so all HTTP traffic goes through
    /// the proxy automatically.
    Implicit,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContainerClientConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ContainerClientConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool { true }

#[derive(Debug, Deserialize, Default, Clone)]
pub struct NvidiaConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct PodmanSocketConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Overrides the default of $XDG_RUNTIME_DIR/podman/podman.sock
    pub socket_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DockerSocketConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_docker_socket_path")]
    pub socket_path: String,
}

impl Default for DockerSocketConfig {
    fn default() -> Self {
        Self { enabled: false, socket_path: default_docker_socket_path() }
    }
}

fn default_docker_socket_path() -> String {
    "/var/run/docker.sock".into()
}


#[derive(Debug, Deserialize, Default, Clone)]
pub struct GpgAgentConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SshAgentConfig {
    #[serde(default)]
    pub enabled: bool,
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



#[derive(Debug, Deserialize, Default, Clone)]
pub struct DbusProxyConfig {
    #[serde(default)]
    pub enabled: bool,
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

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SecretsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to the unified enclo-secrets config file.
    #[serde(default)]
    pub config: String,
    /// Controls how the proxy URL is exposed inside the container (if proxy is enabled in config).
    #[serde(default)]
    pub mode: ProxyMode,
    /// Socket path inside the container for the secrets bridge (macOS only).
    /// Defaults to /tmp/enclo-secrets.sock.
    pub bridge_socket_path: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AwsCredentialsConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct XdgOpenConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {}", path.display(), e))?;
        let mut config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {}", path.display(), e))?;
        let base = path.parent().unwrap_or(std::path::Path::new("."));
        config.resolve_paths(base);
        Ok(config)
    }

    fn resolve_paths(&mut self, base: &std::path::Path) {
        for vol in &mut self.container.volumes {
            *vol = resolve_volume_host(base, vol);
        }
        let c = &self.modules.secrets.config;
        if !c.is_empty() {
            self.modules.secrets.config = resolve_str(base, c);
        }
    }
}

/// Resolves the host side of a `host:container[:opts]` volume string.
/// Named podman volumes (no `/`, no leading `.`) and specifiers (starting with `%`)
/// are left unchanged; everything else is treated as a host path.
fn resolve_volume_host(base: &std::path::Path, vol: &str) -> String {
    let mut parts = vol.splitn(3, ':');
    let host = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();

    let resolved = if host.starts_with('%') || std::path::Path::new(host).is_absolute() {
        host.to_string()
    } else if host.contains('/') || host.starts_with('.') {
        base.join(host).to_string_lossy().into_owned()
    } else {
        // Plain name with no path separators — treat as a named podman volume.
        host.to_string()
    };

    if rest.is_empty() {
        resolved
    } else {
        format!("{}:{}", resolved, rest.join(":"))
    }
}

/// Resolves a string path against `base` unless it is absolute or a specifier.
fn resolve_str(base: &std::path::Path, s: &str) -> String {
    if s.starts_with('%') || std::path::Path::new(s).is_absolute() {
        s.to_string()
    } else {
        base.join(s).to_string_lossy().into_owned()
    }
}
