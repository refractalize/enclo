mod aws;
mod codex;
mod config;
mod get;
mod proxy;
mod run;
mod services;
mod util;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use enclo_backend_protocol::DEFAULT_SECRETS_SOCKET;

fn default_socket() -> PathBuf {
    std::env::var_os("ENCLO_SECRETS_SOCKET")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SECRETS_SOCKET))
}

#[derive(Parser)]
#[command(name = "enclo", about = "enclo container client")]
struct Cli {
    /// Select active services installed for this user (defaults to the only active connection)
    #[arg(long, global = true)]
    connection: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print AWS credential_process JSON using the host credentials provider
    AwsCredentials {
        /// AWS credentials profile to retrieve
        profile: String,
        /// Path to the enclo-secrets Unix socket
        #[arg(long, short = 's')]
        socket: Option<PathBuf>,
    },
    #[command(hide = true)]
    ServiceInstall { name: String },
    #[command(hide = true)]
    ServiceRemove { name: String, generation: PathBuf },
    /// Prepare a provider to authenticate through the host proxy
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Fetch one or more secrets and print them as a JSON object to stdout
    Get {
        /// Path to the enclo-secrets Unix socket
        #[arg(long, short = 's')]
        socket: Option<PathBuf>,

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
        #[arg(long, short = 's')]
        socket: Option<PathBuf>,

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

    /// Run a command with HTTP_PROXY/HTTPS_PROXY set from $ENCLO_PROXY
    Proxy {
        /// Print proxy env vars as shell export statements for use with eval
        #[arg(long)]
        env: bool,

        /// Command and arguments to run
        #[arg(required = true, last = true, conflicts_with = "env")]
        cmd: Vec<String>,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Install placeholder credentials for the native Codex provider
    Setup {
        #[arg(value_parser = ["codex"])]
        provider: String,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ServiceInstall { name } => {
            return services::install(&name, serde_json::from_reader(std::io::stdin())?)
        }
        Command::ServiceRemove { name, generation } => return services::remove(&name, &generation),
        _ => services::load(cli.connection.as_deref())?,
    }
    match cli.command {
        Command::ServiceInstall { .. } | Command::ServiceRemove { .. } => unreachable!(),
        Command::AwsCredentials { profile, socket } => {
            aws::run(&socket.unwrap_or_else(default_socket), &profile).await
        }
        Command::Auth {
            command:
                AuthCommand::Setup {
                    provider: _,
                    socket,
                },
        } => codex::setup(&socket.unwrap_or_else(default_socket)).await,
        Command::Get {
            socket,
            config,
            group,
            names,
            env,
        } => {
            if names.is_empty() && group.is_empty() {
                anyhow::bail!("at least one secret name or --group is required");
            }
            let cfg = config::ClientConfig::load_or_default(config.as_deref())?;
            let resolved = cfg.resolve(&group, &names)?;
            get::run(socket.unwrap_or_else(default_socket), resolved, env).await
        }
        Command::Run {
            socket,
            config,
            group,
            names,
            cmd,
        } => {
            if names.is_empty() && group.is_empty() {
                anyhow::bail!("at least one secret name or --group is required");
            }
            let cfg = config::ClientConfig::load_or_default(config.as_deref())?;
            let resolved = cfg.resolve(&group, &names)?;
            run::run(socket.unwrap_or_else(default_socket), resolved, cmd).await
        }
        Command::Proxy { env, cmd } => proxy::run(env, &cmd),
    }
}
