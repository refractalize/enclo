//! Credentials from Enclo's dedicated Codex login. Refresh tokens stay on the host.
use anyhow::{Context, Result};
use base64::{
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
    Engine,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub fn profile_home() -> Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let data = match data {
        Some(data) => data,
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/share")
        }
    };
    anyhow::ensure!(
        data.is_absolute(),
        "Enclo credential storage must be an absolute path"
    );
    Ok(data.join("enclo/codex"))
}

/// Cross-process lock shared by login and all workspace providers.
pub async fn lock_profile(home: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        // Codex owns auth.lock; never hold its lock while invoking its CLI.
        .open(home.join("enclo-auth.lock"))
        .context("opening Codex profile lock")?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Codex credentials are busy; finish the active login and retry")??;
    Ok(file)
}

fn check_private(path: &Path, directory: bool) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .context("Codex login is missing; run enclo auth login codex")?;
    anyhow::ensure!(
        !meta.file_type().is_symlink()
            && if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            },
        "Codex credential path must not be a symlink or special file"
    );
    anyhow::ensure!(meta.uid() == unsafe {libc::geteuid()} && meta.mode() & 0o077 == 0,
        "Codex credential permissions must be private to the current user; run enclo auth login codex");
    Ok(())
}

#[derive(Deserialize)]
struct Auth {
    tokens: Option<Tokens>,
}
#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    account_id: String,
}

fn read_tokens(home: &Path) -> Result<Tokens> {
    let path = home.join("auth.json");
    check_private(&path, false)?;
    // Never include credential contents or a deserialized structure in errors.
    let auth: Auth =
        serde_json::from_slice(&std::fs::read(path)?).context("invalid Codex credential file")?;
    let tokens = auth
        .tokens
        .context("a ChatGPT login is required; run enclo auth login codex")?;
    anyhow::ensure!(
        !tokens.access_token.is_empty() && !tokens.account_id.is_empty(),
        "incomplete Codex login; run enclo auth login codex"
    );
    Ok(tokens)
}

fn expiry(token: &str) -> Result<u64> {
    let payload = token
        .split('.')
        .nth(1)
        .context("Codex access token has no expiry metadata")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .context("invalid Codex access token metadata")?;
    let claims: Value =
        serde_json::from_slice(&bytes).context("invalid Codex access token metadata")?;
    claims
        .get("exp")
        .and_then(Value::as_u64)
        .context("Codex access token has no expiry metadata")
}

/// Non-secret account identity for container-side native routing discovery.
pub async fn account_id() -> Result<String> {
    let home = profile_home()?;
    check_private(&home, true)?;
    let _lock = lock_profile(&home).await?;
    Ok(read_tokens(&home)?.account_id)
}

pub async fn credentials() -> Result<String> {
    let home = profile_home()?;
    check_private(&home, true)?;
    let _lock = lock_profile(&home).await?;
    let mut tokens = read_tokens(&home)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if expiry(&tokens.access_token)? <= now.saturating_add(120) {
        refresh(&home).await?;
        tokens = read_tokens(&home)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        anyhow::ensure!(
            expiry(&tokens.access_token)? > now.saturating_add(120),
            "Codex did not renew its access token; run enclo auth login codex"
        );
    }
    Ok(json!({"access_token": tokens.access_token, "account_id": tokens.account_id}).to_string())
}

async fn read_response(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    id: u64,
) -> Result<Value> {
    loop {
        let mut line = String::new();
        anyhow::ensure!(
            reader.read_line(&mut line).await? != 0,
            "Codex app-server exited during refresh"
        );
        let message: Value =
            serde_json::from_str(&line).context("invalid Codex app-server response")?;
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            anyhow::ensure!(
                message.get("error").is_none(),
                "Codex credential refresh failed; run enclo auth login codex"
            );
            return message
                .get("result")
                .cloned()
                .context("missing Codex app-server result");
        }
    }
}

async fn refresh(home: &Path) -> Result<()> {
    let mut child = tokio::process::Command::new("codex")
        .args([
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "app-server",
            "--stdio",
        ])
        .env("CODEX_HOME", home)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("cannot launch Codex to refresh the dedicated login")?;
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        let mut stdin = child.stdin.take().context("missing Codex input")?;
        let mut stdout = BufReader::new(child.stdout.take().context("missing Codex output")?);
        let init = json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"enclo_codex_auth","version":env!("CARGO_PKG_VERSION")}}});
        stdin.write_all(format!("{init}\n").as_bytes()).await?;
        read_response(&mut stdout, 1).await?;
        stdin.write_all(b"{\"method\":\"initialized\"}\n{\"id\":2,\"method\":\"account/read\",\"params\":{\"refreshToken\":true}}\n").await?;
        let result = read_response(&mut stdout, 2).await?;
        anyhow::ensure!(result.pointer("/account/type").and_then(Value::as_str) == Some("chatgpt"),
            "Codex ChatGPT login is unavailable; run enclo auth login codex");
        Ok::<(), anyhow::Error>(())
    }).await.context("Codex credential refresh timed out");
    let _ = child.kill().await;
    result??;
    Ok(())
}
