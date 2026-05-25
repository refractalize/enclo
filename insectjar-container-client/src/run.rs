use std::{os::unix::process::CommandExt, path::PathBuf};
use crate::get;

pub async fn run(socket_path: PathBuf, names: Vec<String>, cmd: Vec<String>) -> anyhow::Result<()> {
    let secrets = get::fetch(&socket_path, &names).await?;

    let err = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .envs(&secrets)
        .exec(); // replaces the current process on success; only returns on error

    Err(anyhow::anyhow!("failed to exec '{}': {err}", cmd[0]))
}
