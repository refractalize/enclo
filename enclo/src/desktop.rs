//! Local descriptor-passing desktop services, owned by the SSH supervisor.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        io::{AsRawFd, OwnedFd},
    },
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopConfig {
    #[serde(default)]
    pub wayland: bool,
    #[serde(default)]
    pub dbus: DbusConfig,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DbusConfig {
    pub address: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub talk: Vec<String>,
    #[serde(default)]
    pub call: Vec<String>,
    #[serde(default)]
    pub broadcast: Vec<String>,
}
impl DesktopConfig {
    pub fn disabled(&self) -> bool {
        !self.wayland && !self.dbus.enabled
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.dbus.enabled
                || (self.dbus.talk.is_empty()
                    && self.dbus.call.is_empty()
                    && self.dbus.broadcast.is_empty()),
            "desktop.dbus rules require enabled=true"
        );
        if let Some(address) = &self.dbus.address {
            ensure!(
                self.dbus.enabled && address.starts_with("unix:") && !address.contains('\0'),
                "desktop.dbus.address requires enabled=true and a local Unix bus address"
            );
        }
        for rule in self
            .dbus
            .talk
            .iter()
            .chain(&self.dbus.call)
            .chain(&self.dbus.broadcast)
        {
            ensure!(
                !rule.is_empty() && !rule.chars().any(|c| c.is_control() || c.is_whitespace()),
                "invalid desktop D-Bus rule"
            );
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Plan {
    pub directory: PathBuf,
    pub app_id: String,
    pub config: DesktopConfig,
}
pub struct Desktop {
    plan: Plan,
    child: Option<Child>,
    close: Option<OwnedFd>,
    _lock: File,
}
impl Desktop {
    pub async fn start(plan: Plan) -> Result<Self> {
        crate::connection_runtime::validate(&plan.directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(plan.directory.join("services.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "desktop services for this machine already belong to another connection"
        );
        let mut service = Self {
            plan,
            child: None,
            close: None,
            _lock: lock,
        };
        let sockets = service.plan.directory.join("sockets");
        crate::connection_runtime::validate(&sockets)?;
        for name in ["bus", "wayland"] {
            match fs::remove_file(sockets.join(name)) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        if service.plan.config.wayland {
            let path = sockets.join("wayland");
            let app = service.plan.app_id.clone();
            // Keep the machine lock until the blocking protocol handshake finishes.
            let registration_lock = service._lock.try_clone()?;
            service.close = Some(
                tokio::task::spawn_blocking(move || {
                    let _lock = registration_lock;
                    crate::wayland_security::register_security_context(path, app)
                })
                .await??,
            );
        }
        if service.plan.config.dbus.enabled {
            let address = service
                .plan
                .config
                .dbus
                .address
                .clone()
                .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok())
                .context("desktop D-Bus needs DBUS_SESSION_BUS_ADDRESS or desktop.dbus.address")?;
            ensure!(
                address.starts_with("unix:") && !address.contains('\0'),
                "desktop D-Bus requires a local Unix bus address"
            );
            let path = sockets.join("bus");
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(service.plan.directory.join("dbus.log"))?;
            let mut cmd = Command::new("xdg-dbus-proxy");
            cmd.arg(address)
                .arg(&path)
                .arg("--filter")
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .kill_on_drop(true);
            for (prefix, rules) in [
                ("talk", &service.plan.config.dbus.talk),
                ("call", &service.plan.config.dbus.call),
                ("broadcast", &service.plan.config.dbus.broadcast),
            ] {
                for rule in rules {
                    cmd.arg(format!("--{prefix}={rule}"));
                }
            }
            #[cfg(target_os = "linux")]
            unsafe {
                let parent = libc::getpid();
                cmd.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        libc::_exit(1);
                    }
                    Ok(())
                });
            }
            service.child = Some(cmd.spawn().context("start xdg-dbus-proxy")?);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                ensure!(
                    !service.exited()?,
                    "desktop D-Bus proxy exited; inspect dbus.log"
                );
                if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
                    break;
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "desktop D-Bus proxy readiness timed out"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        Ok(service)
    }
    pub fn exited(&mut self) -> Result<bool> {
        Ok(match self.child.as_mut() {
            Some(child) => child.try_wait()?.is_some(),
            None => false,
        })
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.close = None;
        if let Some(child) = &mut self.child {
            if child.try_wait()?.is_none() {
                child.kill().await?;
            }
            child.wait().await?;
        }
        self.child = None;
        Ok(())
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        self.close = None;
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        for name in ["bus", "wayland"] {
            let _ = fs::remove_file(self.plan.directory.join("sockets").join(name));
        }
    }
}
