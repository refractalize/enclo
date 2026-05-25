mod config;
mod get;
mod proxy;
mod run;
mod util;

use std::path::PathBuf;
use clap::{Parser, Subcommand};

/// Default socket path inside the container (mounted by the secrets module).
const DEFAULT_SOCKET: &str = "/run/enclo-secrets.sock";

#[derive(Parser)]
#[command(name = "enclo", about = "enclo container client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch one or more secrets and print them as a JSON object to stdout
    Get {
        /// Path to the enclo-secrets Unix socket
        #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
        socket: PathBuf,

        /// Path to the client config file (default: $XDG_CONFIG_HOME/enclo/client.toml)
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,

        /// Secret group(s) to fetch; may be comma-separated or repeated
        #[arg(long, short = 'g')]
        group: Vec<String>,

        /// Print secrets as shell export statements for use with eval
        #[arg(long)]
        env: bool,

        /// Secret names to retrieve
        names: Vec<String>,
    },

    /// Fetch secrets and run a command with them set as environment variables
    Run {
        /// Path to the enclo-secrets Unix socket
        #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
        socket: PathBuf,

        /// Path to the client config file (default: $XDG_CONFIG_HOME/enclo/client.toml)
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,

        /// Secret group(s) to fetch; may be comma-separated or repeated
        #[arg(long, short = 'g')]
        group: Vec<String>,

        /// Secret names to fetch and export as environment variables
        names: Vec<String>,

        /// Command and arguments to run
        #[arg(required = true, last = true)]
        cmd: Vec<String>,
    },

    /// Run a command with HTTP_PROXY/HTTPS_PROXY set from $INSECTJAR_PROXY
    Proxy {
        /// Print proxy env vars as shell export statements for use with eval
        #[arg(long)]
        env: bool,

        /// Command and arguments to run
        #[arg(required = true, last = true, conflicts_with = "env")]
        cmd: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Get { socket, config, group, names, env } => {
            if names.is_empty() && group.is_empty() {
                anyhow::bail!("at least one secret name or --group is required");
            }
            let cfg = config::ClientConfig::load_or_default(config.as_deref())?;
            let resolved = cfg.resolve(&group, &names)?;
            get::run(socket, resolved, env).await
        }
        Command::Run { socket, config, group, names, cmd } => {
            if names.is_empty() && group.is_empty() {
                anyhow::bail!("at least one secret name or --group is required");
            }
            let cfg = config::ClientConfig::load_or_default(config.as_deref())?;
            let resolved = cfg.resolve(&group, &names)?;
            run::run(socket, resolved, cmd).await
        }
        Command::Proxy { env, cmd } => proxy::run(env, &cmd),
    }
}
