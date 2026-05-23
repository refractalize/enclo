mod access;
mod config;
mod get;
mod serve;

use std::path::PathBuf;
use clap::{Parser, Subcommand};

const DEFAULT_SOCKET: &str = "/run/insectjar-secrets.sock";

#[derive(Parser)]
#[command(name = "insectjar-secrets", about = "Secret service for insectjar containers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the secret service daemon and listen for requests on a Unix socket
    Serve {
        /// Path to the Unix socket to listen on
        #[arg(long, short = 's')]
        socket: PathBuf,

        /// Path to config file [default: $XDG_CONFIG_HOME/insectjar-secrets/config.toml]
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
    },

    /// Fetch one or more secrets and print them as a JSON object to stdout
    Get {
        /// Path to the Unix socket to connect to
        #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
        socket: PathBuf,

        /// Secret names to retrieve
        #[arg(required = true)]
        names: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { socket, config } => {
            let config_path = config.unwrap_or_else(default_config_path);
            let config = config::Config::from_file(&config_path)?;
            serve::run(socket, config_path, config).await
        }
        Command::Get { socket, names } => {
            get::run(socket, names).await
        }
    }
}

fn default_config_path() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("insectjar-secrets")
        .join("config.toml")
}
