use std::path::{Path, PathBuf};

/// Path to the daemon's Unix socket (used on macOS instead of D-Bus).
#[cfg(target_os = "macos")]
pub fn daemon_socket() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"));
    let uid = unsafe { libc::getuid() };
    PlatformSupport::current().runtime_dir(&home, uid).join("enclo").join("daemon.sock")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlatformSupport {
    Linux,
    MacOs,
}

impl PlatformSupport {
    pub fn current() -> Self {
        match Platform::current() {
            Platform::Linux => Self::Linux,
            Platform::MacOs => Self::MacOs,
        }
    }

    pub fn platform(&self) -> Platform {
        match self {
            Self::Linux => Platform::Linux,
            Self::MacOs => Platform::MacOs,
        }
    }

    pub fn runtime_dir(&self, home: &Path, uid: u32) -> PathBuf {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir);
        }

        match self {
            Self::Linux => PathBuf::from(format!("/run/user/{uid}")),
            // macOS does not have a stable XDG runtime dir by default. Keep enclo
            // state under the user's caches area instead of guessing a Linux path.
            Self::MacOs => home.join("Library/Caches/enclo/run"),
        }
    }

    pub fn config_home(&self, home: &Path) -> PathBuf {
        if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(dir);
        }

        match self {
            // Keep ~/.config on both platforms for now so existing config lookup
            // behavior stays stable while the port is incomplete.
            Self::Linux | Self::MacOs => home.join(".config"),
        }
    }

    pub fn config_dir(&self, home: &Path) -> PathBuf {
        self.config_home(home).join("enclo")
    }

    pub fn session_bus_socket_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join("bus")
    }

    pub fn default_podman_socket_path(&self, runtime_dir: &Path) -> PathBuf {
        match self {
            Self::Linux => runtime_dir.join("podman/podman.sock"),
            Self::MacOs => runtime_dir.join("podman/podman.sock"),
        }
    }

    pub fn default_gpg_agent_ssh_socket_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join("gnupg/S.gpg-agent.ssh")
    }

    pub fn default_gpg_agent_extra_socket_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join("gnupg/S.gpg-agent.extra")
    }

    pub fn supports_module(&self, module_name: &str) -> bool {
        match self {
            Self::Linux => true,
            Self::MacOs => !matches!(
                module_name,
                "dbus_proxy"
                    | "docker_socket"
                    | "drm"
                    | "gpg_agent"
                    | "nvidia"
                    | "podman_socket"
                    | "ssh_agent"
                    | "wayland"
                    | "xdg_open"
            ),
        }
    }
}
