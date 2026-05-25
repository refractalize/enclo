mod get;
mod proxy;
mod run;

use std::path::PathBuf;
use clap::{Parser, Subcommand};

/// Default socket path inside the container (mounted by the secrets module).
const DEFAULT_SOCKET: &str = "/run/insectjar-secrets.sock";

#[derive(Parser)]
#[command(name = "insectjar", about = "insectjar container client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch one or more secrets and print them as a JSON object to stdout
    Get {
        /// Path to the insectjar-secrets Unix socket
        #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
        socket: PathBuf,

        /// Print secrets as shell export statements for use with eval
        #[arg(long)]
        env: bool,

        /// Secret names to retrieve
        #[arg(required = true)]
        names: Vec<String>,
    },

    /// Fetch secrets and run a command with them set as environment variables
    Run {
        /// Path to the insectjar-secrets Unix socket
        #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
        socket: PathBuf,

        /// Secret names to fetch and export as environment variables
        #[arg(required = true)]
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
        Command::Get { socket, names, env } => get::run(socket, names, env).await,
        Command::Run { socket, names, cmd } => run::run(socket, names, cmd).await,
        Command::Proxy { env, cmd } => proxy::run(env, &cmd),
    }
}
