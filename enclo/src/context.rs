use std::path::PathBuf;
use crate::platform::{Platform, PlatformSupport};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Context {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_dir: PathBuf,
    /// $XDG_RUNTIME_DIR/enclo/<container_name>/ — private to this container instance
    pub enclo_dir: PathBuf,
    pub platform: Platform,
    pub support: PlatformSupport,
    pub container_name: String,
}

impl Context {
    /// Returns $XDG_RUNTIME_DIR/enclo/<container_name>/<module_name>/
    pub fn module_dir(&self, module: &str) -> PathBuf {
        self.enclo_dir.join(module)
    }

    /// Expands systemd-style specifiers in a string:
    ///   %h → home directory
    ///   %t → XDG_RUNTIME_DIR
    pub fn expand(&self, s: &str) -> String {
        s.replace("%h", &self.home.to_string_lossy())
         .replace("%t", &self.runtime_dir.to_string_lossy())
    }

    pub fn new(container_name: String) -> anyhow::Result<Self> {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".into());

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/root"));

        let support = PlatformSupport::current();
        let runtime_dir = support.runtime_dir(&home, uid);
        let config_dir = support.config_home(&home);
        let platform = support.platform();

        let enclo_dir = runtime_dir.join("enclo").join(&container_name);

        Ok(Self {
            user,
            uid,
            gid,
            home,
            runtime_dir,
            config_dir,
            enclo_dir,
            platform,
            support,
            container_name,
        })
    }
}
