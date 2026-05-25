mod access;
mod config;
mod serve;

use std::path::PathBuf;
use clap::Parser;

#[derive(Parser)]
#[command(name = "insectjar-secrets", about = "Secret service for insectjar containers")]
struct Cli {
    /// Path to the Unix socket to listen on [default: $XDG_RUNTIME_DIR/insectjar-secrets.sock]
    #[arg(long, short = 's')]
    socket: Option<PathBuf>,

    /// Path to config file [default: $XDG_CONFIG_HOME/insectjar-secrets/config.toml]
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("insectjar_secret_store=info".parse()?)
                .add_directive("insectjar_secrets=info".parse()?)
        )
        .init();

    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(default_socket_path);
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let config = config::Config::from_file(&config_path)?;
    serve::run(socket, config_path, config).await
}

fn default_socket_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("insectjar-secrets.sock")
}

fn default_config_path() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("insectjar-secrets")
        .join("config.toml")
}
