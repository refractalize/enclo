mod client;
mod config;
mod context;
mod daemon;
mod module;
mod modules;
mod orchestrator;
mod podman;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use config::Config;
use context::Context;
use modules::build_registry;
use orchestrator::Orchestrator;

#[derive(Parser)]
#[command(name = "safebox", about = "Modular rootless container launcher")]
struct Cli {
    /// Explicit config file path (overrides NAME)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon and listen for commands on D-Bus
    Serve,

    /// Start a container directly (blocking, no daemon required)
    Run {
        /// Container name; resolves ~/.config/safebox/<NAME>.toml (default: dev)
        name: Option<String>,
    },

    /// Ask the daemon to start a container
    Start {
        /// Container name (default: dev)
        name: Option<String>,
    },

    /// Ask the daemon to stop a container
    Stop {
        /// Container name (default: dev)
        name: Option<String>,
    },

    /// Ask the daemon to restart a container (stop then start)
    Restart {
        /// Container name (default: dev)
        name: Option<String>,
    },

    /// Enter a running container (starts it via the daemon if necessary)
    Enter {
        /// Container name (default: dev)
        name: Option<String>,
        /// Command to run inside the container (default: exec_shell from config)
        #[arg(last = true)]
        cmd: Vec<String>,
    },

    /// Exec into an already-running container without contacting the daemon
    Exec {
        /// Container name (default: dev)
        name: Option<String>,
        /// Command to run inside the container (default: exec_shell from config)
        #[arg(last = true)]
        cmd: Vec<String>,
    },

    /// Show container status (queries daemon)
    Status {
        /// Container name; omit to list all
        name: Option<String>,
    },

    /// Create missing directories in the home named volume with correct ownership
    SetupVolume {
        /// Container name; resolves ~/.config/safebox/<NAME>.toml (default: dev)
        name: Option<String>,
        /// Print what would be created without making any changes
        #[arg(long)]
        dry_run: bool,
    },
}

fn resolve_config(explicit: Option<PathBuf>, name: Option<&str>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let name = name.unwrap_or("dev");
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(format!(".config/safebox/{}.toml", name))
}

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/safebox")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("safebox=info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve => {
            daemon::run_daemon(config_dir()).await
        }

        Command::Run { ref name } => {
            let path = resolve_config(cli.config, name.as_deref());
            let config = Config::load(&path)?;
            let ctx = Context::new(config.container.name.clone())?;
            let orch = Orchestrator::new(build_registry(&config.modules));
            orch.start(&config, &ctx, None).await
        }

        Command::Start { name } => {
            let name = name.unwrap_or_else(|| "dev".into());
            let path = resolve_config(cli.config, Some(&name));
            client::start(&name, &path.to_string_lossy()).await
        }

        Command::Stop { name } => {
            let name = name.unwrap_or_else(|| "dev".into());
            client::stop(&name).await
        }

        Command::Restart { name } => {
            let name = name.unwrap_or_else(|| "dev".into());
            let path = resolve_config(cli.config, Some(&name));
            client::restart(&name, &path.to_string_lossy()).await
        }

        Command::Status { name } => {
            client::status(name.as_deref()).await
        }

        Command::Enter { ref name, ref cmd } => {
            let name = name.as_deref().unwrap_or("dev");
            let path = resolve_config(cli.config, Some(name));
            let config = Config::load(&path)?;
            let ctx = Context::new(config.container.name.clone())?;

            // Ensure container is running via daemon
            client::ensure_running(name, &path.to_string_lossy()).await?;

            let exec_env = ctx.safebox_dir.join("exec-env");
            if !exec_env.exists() {
                anyhow::bail!(
                    "exec-env not found at {}; container may not be fully started",
                    exec_env.display()
                );
            }

            let shell_arg = cmd.first().map(|s| s.as_str());
            let shell = shell_arg.unwrap_or(&config.container.exec_shell);
            let workdir = resolve_exec_workdir(&config.container.exec_workdir, &ctx);
            podman::exec(&ctx.container_name, shell, &exec_env, &workdir).await
        }

        Command::Exec { ref name, ref cmd } => {
            let name = name.as_deref().unwrap_or("dev");
            let path = resolve_config(cli.config, Some(name));
            let config = Config::load(&path)?;
            let ctx = Context::new(config.container.name.clone())?;

            let exec_env = ctx.safebox_dir.join("exec-env");
            if !exec_env.exists() {
                anyhow::bail!(
                    "no exec-env file found at {}; is the container running?",
                    exec_env.display()
                );
            }

            let shell_arg = cmd.first().map(|s| s.as_str());
            let shell = shell_arg.unwrap_or(&config.container.exec_shell);
            let workdir = resolve_exec_workdir(&config.container.exec_workdir, &ctx);
            podman::exec(&ctx.container_name, shell, &exec_env, &workdir).await
        }

        Command::SetupVolume { ref name, dry_run } => {
            let path = resolve_config(cli.config, name.as_deref());
            let config = Config::load(&path)?;
            let ctx = Context::new(config.container.name.clone())?;
            let orch = Orchestrator::new(build_registry(&config.modules));
            orch.setup_volume(&config, &ctx, dry_run).await
        }
    }
}

fn resolve_exec_workdir(config_workdir: &Option<String>, ctx: &Context) -> String {
    if let Some(w) = config_workdir {
        return ctx.expand(w);
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ctx.home.to_string_lossy().into_owned())
}
