//! Non-secret local login placeholder for native Codex behind Enclo's proxy.
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

fn check_owned(path: &Path, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        !meta.file_type().is_symlink()
            && if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            },
        "{} must be a regular {}",
        path.display(),
        if directory { "directory" } else { "file" }
    );
    anyhow::ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "{} is not owned by the current user",
        path.display()
    );
    Ok(())
}

fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("missing parent directory")?)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub async fn setup(socket: &Path) -> Result<()> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".codex"),
    };
    fs::create_dir_all(&home)?;
    check_owned(&home, true)?;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
    let auth = home.join("auth.json");
    if auth.symlink_metadata().is_ok() {
        check_owned(&auth, false)?;
        let existing: Value = serde_json::from_slice(&fs::read(&auth)?)
            .context("existing Codex auth.json is invalid")?;
        anyhow::ensure!(existing.get("enclo_proxy_placeholder")==Some(&Value::Bool(true)),
            "a separate Codex login already exists; refusing to replace it with a proxy placeholder");
    }
    let responses = enclo_backend_protocol::unix_client::request(
        socket,
        &["CODEX_ACCOUNT_ID".into()],
    ).await.context("cannot read Codex account identity; configure CODEX_ACCOUNT_ID socket access and run enclo auth login codex on the host")?;
    let account = match responses.into_iter().next().map(|r|r.result) {
        Some(enclo_backend_protocol::ResponseResult::Ok { value }) if !value.is_empty() => value,
        _ => anyhow::bail!("Codex account identity unavailable; run enclo auth login codex on the host and allow CODEX_ACCOUNT_ID"),
    };
    let config = home.join("config.toml");
    let mut doc = if config.symlink_metadata().is_ok() {
        check_owned(&config, false)?;
        fs::read_to_string(&config)?.parse::<toml_edit::DocumentMut>()?
    } else {
        toml_edit::DocumentMut::new()
    };
    doc["cli_auth_credentials_store"] = toml_edit::value("file");
    doc["model_provider"] = toml_edit::value("openai");
    // These credentials are deliberately not usable outside the host proxy.
    // Future local timestamps prevent Codex from proactively refreshing them.
    let claims = json!({"exp":4102444800u64,"email":"enclo-proxy@example.invalid",
        "https://api.openai.com/auth":{"chatgpt_account_id":account}});
    let token = format!(
        "{}.{}.enclo-placeholder",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let cache = json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"enclo_proxy_placeholder":true,
        "tokens":{"id_token":token,"access_token":token,"refresh_token":"","account_id":account},
        "last_refresh":"2100-01-01T00:00:00Z"});
    replace(&config, doc.to_string().as_bytes())?;
    replace(&auth, cache.to_string().as_bytes())?;
    eprintln!(
        "Installed non-secret Codex proxy credentials in {}. Run: enclo proxy -- codex",
        home.display()
    );
    Ok(())
}
