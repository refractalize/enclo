//! Host-side login profiles dedicated to Enclo, separate from ordinary CLI logins.
use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result};

fn check_owned(path: &Path, directory: bool) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink()
            && if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            },
        "{} must be a regular {} rather than a symlink",
        path.display(),
        if directory { "directory" } else { "file" }
    );
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "{} is not owned by the current user",
        path.display()
    );
    Ok(())
}

pub async fn login_codex() -> Result<()> {
    let binary = which::which("codex").context("Codex CLI is not installed or is not on PATH; install it before running enclo auth login codex")?;
    let home = enclo_secret_store_codex::profile_home()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .with_context(|| format!("creating Codex credential directory {}", home.display()))?;
    check_owned(&home, true)?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    let _lock = enclo_secret_store_codex::lock_profile(&home).await?;
    let auth = home.join("auth.json");
    if auth.symlink_metadata().is_ok() {
        check_owned(&auth, false)?;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))?;
    }

    eprintln!(
        "Signing in to Codex for Enclo containers (profile: {})",
        home.display()
    );
    let mut command = tokio::process::Command::new(binary);
    command
        .args([
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "--device-auth",
        ])
        .env("CODEX_HOME", &home)
        // Ignore repository-local Codex settings while establishing this login.
        .current_dir(&home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // SAFETY: umask is async-signal-safe and affects only the forked child.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let status = command
        .status()
        .await
        .context("failed to launch Codex device login")?;
    // Protect a credential file even if login failed after creating one.
    if auth.symlink_metadata().is_ok() {
        check_owned(&auth, false)?;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))?;
    }
    anyhow::ensure!(status.success(), "Codex device login failed ({status})");
    check_owned(&auth, false).context(
        "Codex reported success without a usable auth.json; check credential-store requirements",
    )?;
    eprintln!("Codex login saved for Enclo containers.");
    Ok(())
}
