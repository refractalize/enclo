use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
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
#[allow(dead_code)]
pub struct Context {
    pub user: String,
    pub uid: u32,
    pub home: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_dir: PathBuf,
    /// $XDG_RUNTIME_DIR/insectjar/<container_name>/ — private to this container instance
    pub insectjar_dir: PathBuf,
    pub platform: Platform,
    pub container_name: String,
}

impl Context {
    /// Returns $XDG_RUNTIME_DIR/insectjar/<container_name>/<module_name>/
    pub fn module_dir(&self, module: &str) -> PathBuf {
        self.insectjar_dir.join(module)
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

        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/root"));

        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{}", uid)));

        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"));

        let insectjar_dir = runtime_dir.join("insectjar").join(&container_name);

        Ok(Self {
            user,
            uid,
            home,
            runtime_dir,
            config_dir,
            insectjar_dir,
            platform: Platform::current(),
            container_name,
        })
    }
}
