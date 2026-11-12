//! A local session registry independent of service supervisors and host daemons.
//! Each session owner serves its own control socket. Directory discovery plus a
//! live status handshake replaces PID files and a mandatory background manager.
use anyhow::{ensure, Context, Result};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub target: String,
    pub owner_pid: u32,
    pub started_at: u64,
}

pub struct Registry {
    dir: PathBuf,
}
impl Registry {
    /// Stable by connection name across edits and restarts. Listing sessions
    /// does not parse the profile or require its services to be available.
    pub fn resolve(profile: &Path, root: Option<&Path>) -> Result<Self> {
        let dir = crate::connection_runtime::directory(profile, root)?.join("sessions");
        Ok(Self { dir })
    }
    fn validate_dir(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.dir)?;
        ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "session registry must be owned by you with mode 0700"
        );
        Ok(())
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_id(id), "invalid session ID");
        Ok(self.dir.join(id))
    }
    pub fn register(&self, target: String) -> Result<Registration> {
        crate::connection_runtime::prepare(self.dir.parent().unwrap())?;
        match fs::DirBuilder::new().mode(0o700).create(&self.dir) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        self.validate_dir()?;
        let mut random = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut random);
        let id = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = self.path(&id)?;
        let listener = UnixListener::bind(&path)?;
        Ok(Registration {
            listener,
            path,
            info: SessionInfo {
                id,
                target,
                owner_pid: std::process::id(),
                started_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            },
        })
    }
    pub async fn list(&self) -> Result<Vec<SessionInfo>> {
        match self.validate_dir() {
            Ok(()) => (),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(Vec::new())
            }
            Err(e) => return Err(e),
        }
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_id(&id) {
                continue;
            }
            match self.request(&id, b'I').await {
                Ok(bytes) if bytes.is_empty() => (),
                Ok(bytes) => {
                    let info: SessionInfo = serde_json::from_slice(&bytes)?;
                    ensure!(info.id == id, "session identity mismatch");
                    sessions.push(info);
                }
                // A killed owner can leave a socket inode behind. Never infer
                // liveness from it or use its recorded PID to signal anything.
                Err(e)
                    if e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::UnexpectedEof
                        )
                    }) => {}
                Err(e) => return Err(e.context(format!("query session {id}"))),
            }
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }
    pub async fn stop(&self, id: &str) -> Result<()> {
        self.validate_dir()?;
        let reply = self
            .request(id, b'S')
            .await
            .context("session owner unavailable or termination unconfirmed")?;
        ensure!(
            reply == b"OK",
            "session termination failed: {}",
            String::from_utf8_lossy(&reply)
        );
        Ok(())
    }
    async fn request(&self, id: &str, request: u8) -> Result<Vec<u8>> {
        let path = self.path(id)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = UnixStream::connect(path).await?;
            stream.write_all(&[request]).await?;
            let mut response = Vec::new();
            (&mut stream).take(8193).read_to_end(&mut response).await?;
            ensure!(response.len() <= 8192, "session response too large");
            Ok(response)
        })
        .await
        .context("session owner did not respond")?
    }
}
#[derive(Serialize)]
pub struct ConnectionSession {
    pub connection: String,
    #[serde(flatten)]
    pub session: SessionInfo,
}

pub async fn list_all(root: Option<&Path>) -> Result<Vec<ConnectionSession>> {
    let mut result = Vec::new();
    for name in crate::connection_runtime::discover(root)? {
        let profile = PathBuf::from(format!("{name}.toml"));
        for session in Registry::resolve(&profile, root)?.list().await? {
            result.push(ConnectionSession {
                connection: name.clone(),
                session,
            });
        }
    }
    Ok(result)
}

pub async fn stop_by_id(id: &str, root: Option<&Path>) -> Result<()> {
    ensure!(valid_id(id), "invalid session ID");
    let found: Vec<_> = list_all(root)
        .await?
        .into_iter()
        .filter(|s| s.session.id == id)
        .collect();
    ensure!(
        found.len() == 1,
        "expected one live session for ID {id}, found {}",
        found.len()
    );
    let profile = PathBuf::from(format!("{}.toml", found[0].connection));
    Registry::resolve(&profile, root)?.stop(id).await
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
}

pub struct Registration {
    listener: UnixListener,
    path: PathBuf,
    pub info: SessionInfo,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
impl Registration {
    /// Status is handled here; termination is acknowledged by the session owner
    /// only after it has closed and reaped its SSH child.
    pub async fn next_request(&self) -> Result<Option<StopReply>> {
        let (mut stream, _) = self.listener.accept().await?;
        tokio::time::timeout(Duration::from_secs(1), async {
            match stream.read_u8().await? {
                b'I' => {
                    stream.write_all(&serde_json::to_vec(&self.info)?).await?;
                    Ok(None)
                }
                b'S' => Ok(Some(StopReply(stream))),
                _ => {
                    stream.write_all(b"unknown request").await?;
                    Ok(None)
                }
            }
        })
        .await
        .context("incomplete session request")?
    }
}
pub struct StopReply(UnixStream);
impl StopReply {
    pub async fn confirm(&mut self) -> Result<()> {
        self.0.write_all(b"OK").await?;
        self.0.shutdown().await?;
        Ok(())
    }
    pub async fn reject(&mut self, error: &str) -> Result<()> {
        self.0.write_all(error.as_bytes()).await?;
        self.0.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_ids_cannot_escape_registry() {
        for id in [
            "../control",
            "",
            "/tmp/session",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("0123456789abcdef0123456789abcdef"));
    }
}
