//! Host credential service and per-connection SSH preparation. No Podman APIs.
use super::*;
use rand_core::RngCore;
use std::{net::TcpListener, os::unix::fs::DirBuilderExt};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub secrets: Option<SecretsConfig>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Existing, trusted host secrets policy. Relative to the connection profile.
    pub config: PathBuf,
    /// Optional executable built for the target machine, uploaded as `enclo`.
    pub client_binary: Option<PathBuf>,
    #[serde(default = "default_port")]
    pub remote_proxy_port: u16,
    #[serde(default)]
    pub proxy_mode: ProxyMode,
}
fn default_port() -> u16 {
    18080
}
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    #[default]
    Explicit,
    Implicit,
}

#[derive(Clone)]
pub struct Prepared {
    config: SecretsConfig,
    source_dir: PathBuf,
    snapshot: String,
    binary: PathBuf,
    socket_enabled: bool,
    proxy_enabled: bool,
    ca: Option<Vec<u8>>,
}
impl Prepared {
    pub async fn load(profile: &ConnectionProfile, runtime: &Path) -> Result<Option<Self>> {
        let Some(config) = &profile.services.secrets else {
            return Ok(None);
        };
        let mut parsed: toml::Value = toml::from_str(&fs::read_to_string(&config.config)?)?;
        let source_dir = config.config.parent().unwrap().to_owned();
        let proxy_enabled = parsed
            .get("proxy")
            .and_then(|p| p.get("enabled"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let socket_enabled = parsed
            .get("secrets")
            .and_then(|s| s.get("socket"))
            .and_then(|s| s.get("enabled"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        let mut ca = None;
        if proxy_enabled {
            if let Some(tls) = parsed.get_mut("proxy").and_then(|p| p.get_mut("tls")) {
                for field in ["ca_cert", "ca_key"] {
                    let value = tls
                        .get(field)
                        .and_then(toml::Value::as_str)
                        .context("proxy TLS paths are required")?;
                    let mut path = PathBuf::from(value);
                    if path.is_relative() {
                        path = source_dir.join(path);
                    }
                    path = path.canonicalize()?;
                    if field == "ca_cert" && proxy_enabled {
                        ca = Some(fs::read(&path)?);
                    }
                    tls[field] = toml::Value::String(
                        path.to_str().context("TLS path must be UTF-8")?.into(),
                    );
                }
            }
        }
        let prepared = Self {
            config: config.clone(),
            source_dir,
            snapshot: toml::to_string(&parsed)?,
            binary: binary("enclo-secrets")?,
            socket_enabled,
            proxy_enabled,
            ca,
        };
        let check = runtime.join(format!("check-{:x}.toml", rand_core::OsRng.next_u64()));
        private_write(&check, prepared.snapshot.as_bytes())?;
        let result = checked(
            Command::new(&prepared.binary)
                .args(["check", "--config"])
                .arg(&check)
                .args(["--listen", "127.0.0.1:1"]),
        )
        .await;
        let _ = fs::remove_file(check);
        result.context("validate secrets policy before changing services")?;
        Ok(Some(prepared))
    }
}

#[derive(Serialize)]
pub struct Status {
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_enabled: bool,
    pub proxy_enabled: bool,
    pub host_proxy: Option<String>,
    pub remote_directory: Option<String>,
    pub remote_proxy_port: Option<u16>,
    pub policy_revision: String,
}

pub struct HostServices {
    prepared: Prepared,
    dir: PathBuf,
    child: Option<Child>,
    group: Option<i32>,
    remote: Option<String>,
    proxy_addr: Option<String>,
    password: Option<String>,
}
impl Drop for HostServices {
    fn drop(&mut self) {
        // Also terminate credential backends if startup is cancelled.
        if let Some(group) = self.group.take() {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}
impl HostServices {
    pub fn new(prepared: Prepared, root: &Path) -> Result<Self> {
        let dir = root
            .canonicalize()?
            .join(format!("hs-{:08x}", rand_core::OsRng.next_u32()));
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self {
            prepared,
            dir,
            child: None,
            group: None,
            remote: None,
            proxy_addr: None,
            password: None,
        })
    }
    pub fn status(&self) -> Status {
        Status {
            running: self.child.as_ref().and_then(Child::id).is_some(),
            pid: self.child.as_ref().and_then(Child::id),
            socket_enabled: self.prepared.socket_enabled,
            proxy_enabled: self.prepared.proxy_enabled,
            host_proxy: self.proxy_addr.clone(),
            remote_directory: self.remote.clone(),
            remote_proxy_port: self
                .prepared
                .proxy_enabled
                .then_some(self.prepared.config.remote_proxy_port),
            policy_revision: format!("{:x}", Sha256::digest(self.prepared.snapshot.as_bytes())),
        }
    }
    pub fn exited(&mut self) -> Result<bool> {
        Ok(match &mut self.child {
            Some(child) => child.try_wait()?.is_some(),
            None => true,
        })
    }
    pub async fn start(&mut self) -> Result<()> {
        let policy = self.dir.join("config.toml");
        private_write(&policy, self.prepared.snapshot.as_bytes())?;
        let mut cmd = Command::new(&self.prepared.binary);
        cmd.args(["serve", "--config"])
            .arg(policy)
            .arg("--socket")
            .arg(self.dir.join("sock"))
            .arg("--ready-file")
            .arg(self.dir.join("ready"))
            .current_dir(&self.prepared.source_dir)
            .process_group(0)
            .kill_on_drop(true)
            .stdin(Stdio::null());
        let listener = if self.prepared.proxy_enabled {
            let password = format!(
                "{:016x}{:016x}",
                rand_core::OsRng.next_u64(),
                rand_core::OsRng.next_u64()
            );
            let output = checked(
                Command::new(&self.prepared.binary)
                    .args(["generate-basic-auth", "enclo"])
                    .env("ENCLO_PROXY_PASSWORD", &password),
            )
            .await?;
            let auth: toml::Value = toml::from_str(std::str::from_utf8(&output)?)?;
            let hash = auth["proxy"]["auth"]["password_hash"]
                .as_str()
                .context("missing generated proxy password hash")?;
            cmd.args(["--username", "enclo", "--password-hash", hash]);
            self.password = Some(password);
            let listener = TcpListener::bind("127.0.0.1:0")?;
            self.proxy_addr = Some(listener.local_addr()?.to_string());
            let fd = listener.as_raw_fd();
            cmd.arg("--listen-fd").arg(fd.to_string());
            unsafe {
                cmd.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Some(listener)
        } else {
            None
        };
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.dir.parent().unwrap().join("host-services.log"))?;
        cmd.stdout(log.try_clone()?).stderr(log);
        let child = cmd.spawn()?;
        self.group = child.id().map(|id| id as i32);
        self.child = Some(child);
        drop(listener);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            ensure!(
                !self.exited()?,
                "host secrets service exited; see host-services.log"
            );
            if self.dir.join("ready").exists()
                && (!self.prepared.socket_enabled
                    || tokio::net::UnixStream::connect(self.dir.join("sock"))
                        .await
                        .is_ok())
            {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "host secrets readiness timed out; see host-services.log"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
    pub async fn prepare_remote(&mut self, profile: &ConnectionProfile) -> Result<Vec<Forward>> {
        let bytes = remote(
            profile,
            "umask 077; mktemp -d /tmp/enclo-svc.XXXXXXXXXX",
            &[],
        )
        .await?;
        let dir = String::from_utf8(bytes)?.trim().to_owned();
        ensure!(
            dir.starts_with("/tmp/enclo-svc.")
                && dir.len() <= 64
                && dir[5..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "invalid remote services directory"
        );
        self.remote = Some(dir.clone());
        let mut forwards = Vec::new();
        let mut env =
            String::from("# Generated by enclo. Private to this connection generation.\n");
        if self.prepared.socket_enabled {
            forwards.push(Forward {
                direction: Direction::Remote,
                spec: format!("{dir}/secrets.sock:{}", self.dir.join("sock").display()),
            });
        }
        env.push_str(&format!(
            "export ENCLO_SECRETS_SOCKET={}\n",
            quote(&format!("{dir}/secrets.sock"))
        ));
        if let Some(address) = &self.proxy_addr {
            let port = self.prepared.config.remote_proxy_port;
            forwards.push(Forward {
                direction: Direction::Remote,
                spec: format!("127.0.0.1:{port}:{address}"),
            });
            let proxy = format!(
                "http://enclo:{}@127.0.0.1:{port}",
                self.password.as_ref().unwrap()
            );
            env.push_str(&format!("export ENCLO_PROXY={}\n", quote(&proxy)));
            if matches!(self.prepared.config.proxy_mode, ProxyMode::Implicit) {
                for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                    env.push_str(&format!("export {name}={}\n", quote(&proxy)));
                }
            }
            let ca = self
                .prepared
                .ca
                .as_ref()
                .context("proxy CA certificate is required")?;
            remote(
                profile,
                &format!("umask 077; cat > {}", quote(&format!("{dir}/ca.pem"))),
                ca,
            )
            .await?;
            // Retain normal public trust; scope the extra CA to enclo sessions.
            remote(profile, &format!("umask 077; : > '{dir}/ca-bundle.pem'; for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do if [ -r \"$f\" ]; then cat \"$f\" > '{dir}/ca-bundle.pem'; break; fi; done; printf '\\n' >> '{dir}/ca-bundle.pem'; cat '{dir}/ca.pem' >> '{dir}/ca-bundle.pem'"), &[]).await?;
            for name in [
                "SSL_CERT_FILE",
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                "GIT_SSL_CAINFO",
                "AWS_CA_BUNDLE",
            ] {
                env.push_str(&format!(
                    "export {name}={}\n",
                    quote(&format!("{dir}/ca-bundle.pem"))
                ));
            }
            env.push_str(&format!(
                "export NODE_EXTRA_CA_CERTS={}\n",
                quote(&format!("{dir}/ca.pem"))
            ));
        }
        if let Some(binary) = &self.prepared.config.client_binary {
            let bytes = fs::read(binary).context("read target-compatible container client")?;
            remote(profile, &format!("umask 077; mkdir '{dir}/bin'; cat > '{dir}/bin/enclo'; chmod 700 '{dir}/bin/enclo'"), &bytes).await?;
            remote(
                profile,
                &format!("'{dir}/bin/enclo' --help >/dev/null"),
                &[],
            )
            .await
            .context("client_binary must be executable on the target architecture/libc")?;
            env.push_str(&format!("export PATH='{dir}/bin':\"$PATH\"\n"));
        }
        remote(
            profile,
            &format!("umask 077; cat > '{dir}/env.sh'"),
            env.as_bytes(),
        )
        .await?;
        Ok(forwards)
    }
    pub fn environment_file(&self) -> Option<String> {
        self.remote.as_ref().map(|dir| format!("{dir}/env.sh"))
    }
    pub async fn stop(&mut self, profile: &ConnectionProfile) -> Result<()> {
        if let Some(group) = self.group.take() {
            unsafe {
                libc::kill(-group, libc::SIGTERM);
            }
            if let Some(child) = &mut self.child {
                let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            }
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            if let Some(child) = &mut self.child {
                let _ = child.wait().await;
            }
        }
        self.child = None;
        self.password = None;
        if let Some(dir) = self.remote.take() {
            // Only remove the private mktemp directory created by this instance.
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                remote(profile, &format!("rm -rf -- {}", quote(&dir)), &[]),
            )
            .await;
        }
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

fn binary(name: &str) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?.parent().unwrap().join(name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    which::which(name).with_context(|| {
        format!("{name} not found beside enclo or on PATH; build the workspace first")
    })
}
pub(super) fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}
async fn checked(cmd: &mut Command) -> Result<Vec<u8>> {
    cmd.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await??;
    ensure!(
        output.status.success(),
        "service command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}
async fn remote(profile: &ConnectionProfile, script: &str, input: &[u8]) -> Result<Vec<u8>> {
    let mut cmd = profile.independent();
    cmd.arg("-T")
        .arg(&profile.target.ssh)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        child.stdin.take().unwrap().write_all(input).await?;
        let output = child.wait_with_output().await?;
        ensure!(
            output.status.success(),
            "remote service preparation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output.stdout)
    })
    .await?;
    result
}
