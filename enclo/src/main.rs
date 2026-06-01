mod client;
mod config;
mod context;
mod daemon;
mod ipc;
mod module;
mod modules;
mod orchestrator;
mod podman;
mod platform;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use config::Config;
use context::Context;
use modules::build_registry;
use orchestrator::Orchestrator;
use platform::PlatformSupport;

#[derive(Parser)]
#[command(name = "enclo", about = "Modular rootless container launcher")]
struct Cli {
    /// Explicit config file path (overrides NAME)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (D-Bus on Linux, Unix socket on macOS)
    Serve,

    /// Start a container directly (blocking, no daemon required)
    Run {
        /// Container name; resolves ~/.config/enclo/containers/<NAME>.toml (default: dev)
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
        /// Stop even if there are active exec sessions
        #[arg(long)]
        force: bool,
    },

    /// Ask the daemon to restart a container (stop then start)
    Restart {
        /// Container name (default: dev)
        name: Option<String>,
        /// Restart even if there are active exec sessions
        #[arg(long)]
        force: bool,
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

    /// Ask the daemon to reload config for a running container's modules (sends SIGHUP to supporting services)
    Reload {
        /// Container name (default: dev)
        name: Option<String>,
    },

    /// Show container status (queries daemon)
    Status {
        /// Container name; omit to list all
        name: Option<String>,
    },

    /// Create missing directories in the home named volume with correct ownership
    SetupVolume {
        /// Container name; resolves ~/.config/enclo/containers/<NAME>.toml (default: dev)
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
    config_dir().join("containers").join(format!("{name}.toml"))
}

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"));
    PlatformSupport::current().config_dir(&home)
}

struct LoadedTarget {
    config: Config,
    ctx: Context,
    path: PathBuf,
    name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("enclo=info"))
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve => daemon::run_daemon(config_dir()).await,

        Command::Run { ref name } => {
            let loaded = load_target(cli.config.clone(), name.as_deref())?;
            Orchestrator::new(build_registry(&loaded.config.modules))
                .start(&loaded.config, &loaded.ctx, None, None)
                .await
        }

        Command::Start { name } => {
            let name = resolve_name(name);
            let path = resolve_config(cli.config.clone(), Some(&name));
            client::start(&name, &path.to_string_lossy()).await
        }

        Command::Stop { name, force } => {
            let name = resolve_name(name);
            client::stop(&name, force).await
        }

        Command::Restart { name, force } => {
            let name = resolve_name(name);
            let path = resolve_config(cli.config.clone(), Some(&name));
            client::restart(&name, &path.to_string_lossy(), force).await
        }

        Command::Reload { name } => {
            let name = resolve_name(name);
            client::reload(&name).await
        }

        Command::Status { name } => client::status(name.as_deref()).await,

        Command::Enter { ref name, ref cmd } => {
            let loaded = load_target(cli.config.clone(), name.as_deref())?;
            exec_into_target(&loaded, cmd, true).await
        }

        Command::Exec { ref name, ref cmd } => {
            let loaded = load_target(cli.config.clone(), name.as_deref())?;
            exec_into_target(&loaded, cmd, false).await
        }

        Command::SetupVolume { ref name, dry_run } => {
            let loaded = load_target(cli.config.clone(), name.as_deref())?;
            Orchestrator::new(build_registry(&loaded.config.modules))
                .setup_volume(&loaded.config, &loaded.ctx, dry_run)
                .await
        }
    }
}

fn resolve_name(name: Option<String>) -> String {
    name.unwrap_or_else(|| "dev".into())
}

fn load_target(explicit_config: Option<PathBuf>, name: Option<&str>) -> Result<LoadedTarget> {
    let path = resolve_config(explicit_config, name);
    let config = Config::load(&path)?;
    let ctx = Context::new(config.container.name.clone())?;
    Ok(LoadedTarget {
        config,
        ctx,
        path,
        name: name.unwrap_or("dev").to_string(),
    })
}

async fn exec_into_target(loaded: &LoadedTarget, cmd: &[String], ensure_running: bool) -> Result<()> {
    if ensure_running {
        client::ensure_running(&loaded.name, &loaded.path.to_string_lossy()).await?;
    }

    let exec_env = loaded.ctx.enclo_dir.join("exec-env");
    if !exec_env.exists() {
        let error = if ensure_running {
            format!(
                "exec-env not found at {}; container may not be fully started",
                exec_env.display()
            )
        } else {
            format!(
                "no exec-env file found at {}; is the container running?",
                exec_env.display()
            )
        };
        anyhow::bail!(error);
    }

    let shell = cmd
        .first()
        .map(|value| value.as_str())
        .unwrap_or(&loaded.config.container.exec_shell);
    let workdir = resolve_exec_workdir(&loaded.config.container.exec_workdir, &loaded.ctx);

    podman::exec(&loaded.ctx.container_name, shell, &exec_env, &workdir).await
}

fn resolve_exec_workdir(config_workdir: &Option<String>, ctx: &Context) -> String {
    if let Some(w) = config_workdir {
        return ctx.expand(w);
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ctx.home.to_string_lossy().into_owned())
}
