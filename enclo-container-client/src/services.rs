//! Per-user installation and discovery for ordinary SSH sessions.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub generation: PathBuf,
    pub env: BTreeMap<String, String>,
}
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME")
            .filter(|s| !s.is_empty())
            .context("HOME is not set")?,
    ))
}
fn root() -> Result<PathBuf> {
    Ok(home()?.join(".local/state/enclo"))
}
fn directory(path: &Path, private: bool) -> Result<()> {
    match fs::DirBuilder::new()
        .mode(if private { 0o700 } else { 0o755 })
        .create(path)
    {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & if private { 0o077 } else { 0o022 } == 0,
        "unsafe installation directory: {}",
        path.display()
    );
    Ok(())
}
fn read(path: &Path) -> Result<Settings> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "service settings must be a private file owned by you"
    );
    let settings: Settings = serde_json::from_slice(&fs::read(path)?)?;
    validate(&settings)?;
    Ok(settings)
}
fn validate(settings: &Settings) -> Result<()> {
    let generation = settings
        .generation
        .to_str()
        .context("invalid service generation")?;
    ensure!(
        generation.starts_with("/tmp/enclo-svc.")
            && generation[5..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid service generation directory"
    );
    for (key, value) in &settings.env {
        ensure!(
            [
                "ENCLO_SECRETS_SOCKET",
                "ENCLO_PROXY",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "http_proxy",
                "https_proxy",
                "SSL_CERT_FILE",
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                "GIT_SSL_CAINFO",
                "AWS_CA_BUNDLE",
                "NODE_EXTRA_CA_CERTS"
            ]
            .contains(&key.as_str())
                && !value.contains('\0'),
            "invalid service environment setting"
        );
    }
    Ok(())
}
fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
        "cannot lock client installation"
    );
    Ok(file)
}
fn atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
fn live(settings: &Settings) -> bool {
    if let Some(socket) = settings.env.get("ENCLO_SECRETS_SOCKET") {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return true;
        }
    }
    // Proxy-only generations have no per-generation listener to probe; preserve
    // ownership while their directory exists rather than stealing an active name.
    settings.env.contains_key("ENCLO_PROXY")
        && !settings.generation.join("secrets.sock").exists()
        && settings.generation.is_dir()
}
pub fn install(name: &str, settings: Settings) -> Result<()> {
    ensure!(name_ok(name), "invalid connection name");
    validate(&settings)?;
    let home = home()?;
    directory(&home.join(".local"), false)?;
    directory(&home.join(".local/bin"), false)?;
    directory(&home.join(".local/state"), false)?;
    let root = root()?;
    directory(&root, true)?;
    let _lock = lock(&root.join("install.lock"))?;
    directory(&root.join("connections"), true)?;
    let dir = root.join("connections").join(name);
    directory(&dir, true)?;
    let path = dir.join("active.json");
    if path.exists() {
        let old = read(&path)?;
        ensure!(
            old.generation == settings.generation || !live(&old),
            "connection {name} already has published services; stop its existing owner first"
        );
    }
    let binary = home.join(".local/bin/enclo");
    let marker = root.join("installed-client");
    if let Ok(meta) = fs::symlink_metadata(&binary) {
        ensure!(
            meta.is_file()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o022 == 0
                && fs::read_to_string(&marker).is_ok_and(|s| s == "enclo-container-client\n"),
            "refusing to overwrite an unmanaged ~/.local/bin/enclo"
        );
    }
    atomic(&binary, &fs::read(std::env::current_exe()?)?, 0o700)?;
    atomic(&marker, b"enclo-container-client\n", 0o600)?;
    atomic(&path, &serde_json::to_vec(&settings)?, 0o600)?;
    Ok(())
}
pub fn remove(name: &str, generation: &Path) -> Result<()> {
    ensure!(name_ok(name), "invalid connection name");
    let root = root()?;
    if !root.exists() {
        return Ok(());
    }
    directory(&root, true)?;
    let _lock = lock(&root.join("install.lock"))?;
    let path = root.join("connections").join(name).join("active.json");
    if path.exists() && read(&path)?.generation == generation {
        fs::remove_file(path)?;
    }
    Ok(())
}
pub fn load(selected: Option<&str>) -> Result<()> {
    let selected = selected
        .map(str::to_owned)
        .or_else(|| std::env::var("ENCLO_CONNECTION").ok());
    // Explicit legacy environments remain supported without an installation.
    if selected.is_none()
        && (std::env::var_os("ENCLO_SECRETS_SOCKET").is_some()
            || std::env::var_os("ENCLO_PROXY").is_some())
    {
        return Ok(());
    }
    let root = root()?;
    let connections = root.join("connections");
    if !connections.exists() && selected.is_none() {
        return Ok(());
    }
    directory(&root, true)?;
    directory(&connections, true)?;
    let name = if let Some(name) = selected {
        ensure!(name_ok(&name), "invalid connection name");
        name
    } else {
        let mut names = Vec::new();
        for entry in fs::read_dir(&connections)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name_ok(&name) && entry.path().join("active.json").exists() {
                directory(&entry.path(), true)?;
                if read(&entry.path().join("active.json"))?.generation.is_dir() {
                    names.push(name);
                }
            }
        }
        ensure!(
            !names.is_empty(),
            "no active enclo services; start services on the host"
        );
        ensure!(
            names.len() == 1,
            "multiple active connections; select one with --connection NAME or ENCLO_CONNECTION"
        );
        names.remove(0)
    };
    let dir = connections.join(&name);
    directory(&dir, true)?;
    let settings = read(&dir.join("active.json")).with_context(|| {
        format!("services for {name} are unavailable; start services on the host")
    })?;
    ensure!(
        settings.generation.is_dir(),
        "services for {name} are unavailable; restart services on the host"
    );
    for key in [
        "ENCLO_SECRETS_SOCKET",
        "ENCLO_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "SSL_CERT_FILE",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "GIT_SSL_CAINFO",
        "AWS_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
    ] {
        std::env::remove_var(key);
    }
    for (key, value) in settings.env {
        std::env::set_var(key, value);
    }
    Ok(())
}
