mod auth;
mod client;
mod config;
mod context;
mod daemon;
mod home;
mod ipc;
mod module;
mod modules;
mod network;
mod orchestrator;
mod platform;
mod podman;
mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use config::Config;
use context::Context;
use modules::build_registry;
use orchestrator::Orchestrator;

#[derive(Parser)]
#[command(name = "enclo", about = "Modular rootless container launcher")]
struct Cli {
    /// Enclo configuration root (overrides ENCLO_CONFIG_DIR)
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// Persistent state root (overrides ENCLO_STATE_DIR)
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(flatten)]
    target: TargetArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Default)]
struct TargetArgs {
    /// Select the workspace found in this directory or its parents (no default fallback)
    #[arg(short = 'd', long, global = true, conflicts_with_all = ["container", "config"])]
    directory: Option<PathBuf>,
    /// Select a standalone central profile, bypassing workspace discovery
    #[arg(short = 'C', long, global = true, conflicts_with_all = ["directory", "config"])]
    container: Option<String>,
    /// Select an explicit config file, bypassing workspace discovery
    #[arg(short = 'c', long, global = true, conflicts_with_all = ["directory", "container"])]
    config: Option<PathBuf>,
}
impl TargetArgs {
    fn validate(&self) -> Result<()> {
        // Clap checks conflicts at each command level; also validate global values
        // after propagation so selectors on opposite sides of a subcommand conflict.
        let count = usize::from(self.directory.is_some())
            + usize::from(self.container.is_some())
            + usize::from(self.config.is_some());
        anyhow::ensure!(
            count <= 1,
            "--directory, --container and --config are mutually exclusive"
        );
        Ok(())
    }
    fn is_explicit(&self) -> bool {
        self.directory.is_some() || self.container.is_some() || self.config.is_some()
    }
}

#[derive(Subcommand)]
enum Command {
    /// Local Podman machine lifecycle (run/reload retain the legacy backend)
    Container {
        #[command(subcommand)]
        action: ContainerAction,
        #[arg(long, global = true)]
        runtime_dir: Option<PathBuf>,
    },
    /// Control a named SSH services supervisor without the installed daemon
    Services {
        #[arg(long, global = true)]
        profile: Option<PathBuf>,
        /// Runtime parent directory (overrides ENCLO_RUNTIME_DIR)
        #[arg(long, global = true)]
        runtime_dir: Option<PathBuf>,
        #[arg(long, hide = true, global = true)]
        startup_fd: Option<i32>,
        #[command(subcommand)]
        action: ServicesAction,
    },
    /// List or explicitly terminate SSH sessions, even when services are stopped
    Sessions {
        #[arg(long, global = true)]
        profile: Option<PathBuf>,
        /// Runtime parent directory (overrides ENCLO_RUNTIME_DIR)
        #[arg(long, global = true)]
        runtime_dir: Option<PathBuf>,
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Open an independent shell/command using an established connection profile
    Session {
        /// Named profile from the central connections directory
        name: Option<String>,
        /// Same connection TOML file used by the running services supervisor
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Runtime parent directory (overrides ENCLO_RUNTIME_DIR; same as services run)
        #[arg(long)]
        runtime_dir: Option<PathBuf>,
        /// Remote command and literal arguments; omit for an interactive shell
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Manage host-side credentials dedicated to Enclo containers
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Forget approval for the nearest workspace configuration
    Untrust,
    /// Manage persistent container homes
    Home {
        #[command(subcommand)]
        command: HomeCommand,
    },

    /// Start the daemon (D-Bus on Linux, Unix socket on macOS)
    Serve,

    /// Start a container directly (blocking, no daemon required)
    Run,

    /// Ask the daemon to start a container
    Start,

    /// Ask the daemon to stop a container
    Stop {
        /// Stop even if there are active exec sessions
        #[arg(long)]
        force: bool,
    },

    /// Ask the daemon to restart a container (stop then start)
    Restart {
        /// Restart even if there are active exec sessions
        #[arg(long)]
        force: bool,
    },

    /// Enter a running container (starts it via the daemon if necessary)
    Enter {
        /// Working directory inside the container (overrides config and host CWD)
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// User to run as inside the container (e.g. root or 1000)
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Set additional environment variables (KEY=VALUE); may be repeated
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Command to run inside the container (default: exec_shell from config)
        #[arg(last = true)]
        cmd: Vec<String>,
    },

    /// Exec into an already-running container without contacting the daemon
    Exec {
        /// Working directory inside the container (overrides config and host CWD)
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// User to run as inside the container (e.g. root or 1000)
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Set additional environment variables (KEY=VALUE); may be repeated
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Command to run inside the container (default: exec_shell from config)
        #[arg(last = true)]
        cmd: Vec<String>,
    },

    /// Ask the daemon to reload config for a running container's modules (sends SIGHUP to supporting services)
    Reload,

    /// Show container status (queries daemon)
    Status,

    /// Create missing directories in the home named volume with correct ownership
    SetupVolume {
        /// Print what would be created without making any changes
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in to a provider using a dedicated Enclo credential profile
    Login {
        #[arg(value_enum)]
        provider: AuthProvider,
    },
}

#[derive(Args)]
struct ConnectionName {
    name: Option<String>,
}

#[derive(Subcommand)]
enum ContainerAction {
    /// Initialize a container instance; --rerun explicitly reapplies the script
    Init { name: String, #[arg(long)] rerun: bool },
    /// Export SSH settings, or add/update a managed entry in ~/.ssh/config
    Export { name: String, #[arg(long)] host: Option<String>, #[arg(long)] add: bool },
    /// Initialize a managed home; --rerun explicitly reapplies the configured script
    HomeInit { name: String, #[arg(long)] rerun: bool },
    Run(ConnectionName),
    Start(ConnectionName),
    Status(ConnectionName),
    List,
    Stop { name: Option<String>, #[arg(long)] force: bool },
    Restart { name: Option<String>, #[arg(long)] force: bool },
    Reload(ConnectionName),
    /// Discard the writable layer; keep named volumes and SSH identity
    Remove { name: String, #[arg(long, help = "Allow removing a running container, disconnecting its sessions")] force: bool },
    /// Replace with current config/image; discard writable layer, keep named volumes
    Recreate { name: String, #[arg(long, help = "Allow replacing a running container, disconnecting its sessions")] force: bool },
}

// The foreground legacy runner is retained for migration. The other grouped
// commands use the local SSH machine backend without the installed daemon.

fn normalize_container(mut cli: Cli) -> Result<Cli> {
    let legacy = match &cli.command {
        Command::Container { action: ContainerAction::Run(selected), .. } => Some((false, selected.name.clone())),
        Command::Container { action: ContainerAction::Reload(selected), .. } => Some((true, selected.name.clone())),
        _ => None,
    };
    if let Some((reload, name)) = legacy {
        if let Command::Container { runtime_dir, .. } = &cli.command {
            anyhow::ensure!(runtime_dir.is_none(), "legacy container run/reload do not support --runtime-dir; use XDG_RUNTIME_DIR for foreground legacy testing");
        }
        if let Some(name) = name {
            anyhow::ensure!(!cli.target.is_explicit(), "choose a container name or a target selector, not both");
            enclo::profiles::validate_name(&name)?;
            cli.target.container = Some(name);
        }
        cli.command = if reload { Command::Reload } else { Command::Run };
    }
    Ok(cli)
}

#[derive(Subcommand)]
enum ServicesAction {
    /// Start a detached supervisor and wait for readiness
    Start { name: Option<String>, #[arg(long, default_value = "600")] timeout_secs: std::num::NonZeroU64 },
    /// List all running connection supervisors
    List,
    /// Run the services supervisor in the foreground
    Run(ConnectionName),
    Status(ConnectionName),
    Stop(ConnectionName),
    /// Validate and apply the profile, reconnecting the services transport
    Reload(ConnectionName),
}

#[derive(Subcommand)]
enum SessionAction {
    List(ConnectionName),
    Stop {
        #[arg(long)]
        connection: Option<String>,
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        /// Stop a snapshot of all sessions registered for this profile
        #[arg(long)]
        all: bool,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum AuthProvider {
    Codex,
}

#[derive(Subcommand)]
enum HomeCommand {
    /// Run pending home initialization in an already-running container
    Init {
        /// Explicitly rerun both initialization scripts, even if completed
        #[arg(long)]
        rerun: bool,
    },
}

static CONFIG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn config_dir() -> PathBuf {
    CONFIG_DIR.get().cloned().unwrap_or_else(|| enclo::profiles::config_dir(None))
}

struct LoadedTarget {
    config: Config,
    ctx: Context,
    path: PathBuf,
    name: String,
    directory: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("enclo=info")),
        )
        .init();

    let cli = normalize_container(Cli::parse())?;
    cli.target.validate()?;
    let mut config_root = enclo::profiles::config_dir(cli.config_dir.as_deref());
    if config_root.is_relative() {
        config_root = std::env::current_dir()?.join(config_root);
    }
    CONFIG_DIR.set(config_root).expect("configuration root initialized once");

    match cli.command {
        Command::Container { action, runtime_dir } => {
            anyhow::ensure!(!cli.target.is_explicit(), "container commands take a name; use top-level run for legacy configs");
            if let ContainerAction::Export { name, host, add } = &action {
                let machine = enclo::local_container::LocalContainer::new(name, &config_dir(), runtime_dir.as_deref(), cli.state_dir.as_deref())?;
                let text = machine.export_ssh(host.as_deref(), *add).await?;
                if !add { print!("{text}"); }
                return Ok(());
            }
            let (operation, name, force) = match action {
                ContainerAction::Export { .. } => unreachable!(),
                ContainerAction::HomeInit { name, rerun } => ("home-init", Some(name), rerun),
                ContainerAction::Init { name, rerun } => ("init", Some(name), rerun),
                ContainerAction::Start(selected) => ("start", selected.name, false),
                ContainerAction::Status(selected) => ("status", selected.name, false),
                ContainerAction::Stop { name, force } => ("stop", name, force),
                ContainerAction::Restart { name, force } => ("restart", name, force),
                ContainerAction::Remove { name, force } => ("remove", Some(name), force),
                ContainerAction::Recreate { name, force } => ("recreate", Some(name), force),
                ContainerAction::List => {
                    println!("{}", serde_json::to_string_pretty(&enclo::local_container::LocalContainer::list(&config_dir()).await?)?);
                    return Ok(());
                }
                ContainerAction::Run(_) | ContainerAction::Reload(_) => anyhow::bail!("use container start for independent SSH machines; top-level run/reload retain the legacy backend"),
            };
            let name = name.ok_or_else(|| anyhow::anyhow!("a container name is required"))?;
            let machine = enclo::local_container::LocalContainer::new(&name, &config_dir(), runtime_dir.as_deref(), cli.state_dir.as_deref())?;
            if operation == "init" {
                println!("{}", serde_json::to_string_pretty(&machine.container_init(force).await?)?);
                return Ok(());
            }
            if operation == "home-init" {
                println!("{}", serde_json::to_string_pretty(&machine.home_init(force).await?)?);
                return Ok(());
            }
            if operation == "remove" {
                machine.remove(force).await?;
                println!("null");
                return Ok(());
            }
            if operation == "recreate" {
                let endpoint = machine.recreate(force).await?;
                println!("{}", serde_json::to_string_pretty(&endpoint)?);
                return Ok(());
            }
            if operation == "stop" || operation == "restart" {
                // Stopping the machine necessarily kills its SSH sessions.
                if !force {
                    if let Some(endpoint) = machine.status().await? {
                        let target = format!("enclo-{}", endpoint.container_id);
                        let sessions = enclo::session_registry::list_all(runtime_dir.as_deref()).await?;
                        anyhow::ensure!(!sessions.iter().any(|s| s.session.target == target), "container has active sessions; close them or use --force");
                    }
                }
                machine.stop().await?;
            }
            let status = if operation == "start" || operation == "restart" { Some(machine.start().await?) } else { machine.status().await? };
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        },
        Command::Services {
            profile,
            runtime_dir,
            startup_fd,
            action,
        } => {
            anyhow::ensure!(
                !cli.target.is_explicit(),
                "services uses --profile, not container selectors"
            );
            anyhow::ensure!(startup_fd.is_none() || matches!(action, ServicesAction::Run(_)), "internal startup FD applies only to services run");
            if let ServicesAction::Start { name, timeout_secs } = &action {
                let path = enclo::profiles::connection(&config_dir(), name.as_deref(), profile.as_deref())?;
                let status = enclo::connection::supervisor::start(&path, runtime_dir.as_deref(), &config_dir(), cli.state_dir.as_deref(), std::time::Duration::from_secs(timeout_secs.get())).await?;
                println!("{}", serde_json::to_string_pretty(&status)?);
                return Ok(());
            }
            if matches!(action, ServicesAction::List) {
                anyhow::ensure!(profile.is_none(), "services list does not accept --profile");
                let mut statuses = Vec::new();
                for name in enclo::connection_runtime::discover(runtime_dir.as_deref())? {
                    let path = PathBuf::from(format!("{name}.toml"));
                    let mut status = enclo::connection::supervisor::request(&path, runtime_dir.as_deref(), "status").await?;
                    if status["supervisor_running"] == true {
                        status["connection"] = serde_json::Value::String(name);
                        statuses.push(status);
                    }
                }
                println!("{}", serde_json::to_string_pretty(&statuses)?);
                return Ok(());
            }
            let (operation, selected) = match action {
                ServicesAction::List | ServicesAction::Start { .. } => unreachable!(),
                ServicesAction::Run(selected) => ("run", selected),
                ServicesAction::Status(selected) => ("status", selected),
                ServicesAction::Stop(selected) => ("stop", selected),
                ServicesAction::Reload(selected) => ("reload", selected),
            };
            let profile = enclo::profiles::connection(&config_dir(), selected.name.as_deref(), profile.as_deref())?;
            if operation == "run" {
                enclo::connection::supervisor::run_with_startup(&profile, runtime_dir.as_deref(), &config_dir(), cli.state_dir.as_deref(), startup_fd).await
            } else {
                let status = enclo::connection::supervisor::request(
                    &profile, runtime_dir.as_deref(), operation,
                ).await?;
                println!("{}", serde_json::to_string_pretty(&status)?);
                Ok(())
            }
        }
        Command::Sessions {
            profile,
            runtime_dir,
            action,
        } => {
            anyhow::ensure!(
                !cli.target.is_explicit(),
                "sessions uses --profile, not container selectors"
            );
            let selected = match &action {
                SessionAction::List(selected) => selected.name.as_deref(),
                SessionAction::Stop { connection, .. } => connection.as_deref(),
            };
            if selected.is_none() && profile.is_none() {
                match &action {
                    SessionAction::List(_) => println!("{}", serde_json::to_string_pretty(&enclo::session_registry::list_all(runtime_dir.as_deref()).await?)?),
                    SessionAction::Stop { id: Some(id), .. } => enclo::session_registry::stop_by_id(id, runtime_dir.as_deref()).await?,
                    _ => anyhow::bail!("bulk stop requires --connection NAME or --profile FILE"),
                }
                return Ok(());
            }
            let profile = enclo::profiles::connection(&config_dir(), selected, profile.as_deref())?;
            let registry = enclo::session_registry::Registry::resolve(&profile, runtime_dir.as_deref())?;
            match action {
                SessionAction::List(_) => {
                    println!("{}", serde_json::to_string_pretty(&registry.list().await?)?);
                }
                SessionAction::Stop { id: Some(id), .. } => registry.stop(&id).await?,
                SessionAction::Stop { all: true, .. } => {
                    let mut errors = Vec::new();
                    for session in registry.list().await? {
                        if let Err(error) = registry.stop(&session.id).await {
                            errors.push(format!("{}: {error:#}", session.id));
                        }
                    }
                    anyhow::ensure!(errors.is_empty(), "{}", errors.join("\n"));
                }
                _ => anyhow::bail!("specify a session ID or --all"),
            }
            Ok(())
        }
        Command::Session {
            name,
            profile,
            runtime_dir,
            cmd,
        } => {
            anyhow::ensure!(
                !cli.target.is_explicit(),
                "session uses --profile, not container selectors"
            );
            let profile = enclo::profiles::connection(&config_dir(), name.as_deref(), profile.as_deref())?;
            let code = enclo::connection::shell(&profile, runtime_dir.as_deref(), &config_dir(), cli.state_dir.as_deref(), cmd).await?;
            std::process::exit(code);
        }
        Command::Auth {
            command:
                AuthCommand::Login {
                    provider: AuthProvider::Codex,
                },
        } => {
            anyhow::ensure!(
                !cli.target.is_explicit(),
                "auth commands do not accept container, config or directory selectors"
            );
            auth::login_codex().await
        }
        Command::Untrust => {
            anyhow::ensure!(
                cli.target.container.is_none() && cli.target.config.is_none(),
                "untrust only accepts --directory, not --container or --config"
            );
            workspace::untrust(&cli.target.directory.unwrap_or(std::env::current_dir()?))
        }
        Command::Home {
            command: HomeCommand::Init { rerun },
        } => {
            let loaded = load_target(&cli.target)?;
            anyhow::ensure!(
                loaded.ctx.enclo_dir.join("exec-env").exists(),
                "container is not running; use enclo enter first"
            );
            if let Some(workspace) = &loaded.config.workspace {
                let running =
                    std::fs::read_to_string(loaded.ctx.enclo_dir.join("workspace-fingerprint"))?;
                anyhow::ensure!(
                    running == workspace.fingerprint,
                    "configuration changed; run enclo restart first"
                );
            }
            anyhow::ensure!(
                loaded.config.container.home.enabled,
                "managed home is not enabled for this container"
            );
            anyhow::ensure!(
                loaded.ctx.enclo_dir.join("home-ready").exists(),
                "container home is not ready; finish starting or restart the container first"
            );
            home::initialize(&loaded.config, &loaded.ctx, rerun).await
        }
        Command::Serve => {
            anyhow::ensure!(
                !cli.target.is_explicit(),
                "serve does not accept container, directory or config selectors"
            );
            daemon::run_daemon(config_dir()).await
        }

        Command::Run => {
            let loaded = load_target(&cli.target)?;
            Orchestrator::new(build_registry(&loaded.config))
                .start(&loaded.config, &loaded.ctx, None, None)
                .await
        }

        Command::Start => {
            let loaded = load_target(&cli.target)?;
            let name = loaded.name;
            let path = loaded.path;
            client::start(&name, &path.to_string_lossy()).await
        }

        Command::Stop { force } => {
            let name = resolve_target_name(&cli.target)?;
            client::stop(&name, force).await
        }

        Command::Restart { force } => {
            let loaded = load_target(&cli.target)?;
            let name = loaded.name;
            let path = loaded.path;
            client::restart(&name, &path.to_string_lossy(), force).await
        }

        Command::Reload => {
            let name = resolve_target_name(&cli.target)?;
            client::reload(&name).await
        }

        Command::Status => {
            let name = if cli.target.is_explicit() {
                Some(resolve_target_name(&cli.target)?)
            } else {
                None
            };
            client::status(name.as_deref()).await
        }

        Command::Enter {
            ref workdir,
            ref user,
            ref env,
            ref cmd,
        } => {
            let loaded = load_target(&cli.target)?;
            exec_into_target(&loaded, cmd, workdir.as_deref(), user.as_deref(), env, true).await
        }

        Command::Exec {
            ref workdir,
            ref user,
            ref env,
            ref cmd,
        } => {
            let loaded = load_target(&cli.target)?;
            exec_into_target(
                &loaded,
                cmd,
                workdir.as_deref(),
                user.as_deref(),
                env,
                false,
            )
            .await
        }

        Command::SetupVolume { dry_run } => {
            let loaded = load_target(&cli.target)?;
            Orchestrator::new(build_registry(&loaded.config))
                .setup_volume(&loaded.config, &loaded.ctx, dry_run)
                .await
        }
    }
}

struct Selection {
    path: PathBuf,
    name: String,
    directory: Option<PathBuf>,
    implicit: bool,
}

fn select_target(target: &TargetArgs) -> Result<Selection> {
    if let Some(name) = &target.container {
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "--container must be a central profile name (letters, numbers, - and _)"
        );
        return Ok(Selection {
            path: config_dir().join("containers").join(format!("{name}.toml")),
            name: name.clone(),
            directory: None,
            implicit: false,
        });
    }
    if let Some(path) = &target.config {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("default")
            .to_string();
        return Ok(Selection {
            path: path.clone(),
            name,
            directory: None,
            implicit: false,
        });
    }
    let directory = target
        .directory
        .clone()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    anyhow::ensure!(directory.is_dir(), "--directory must refer to a directory");
    if let Some(path) = workspace::discover(&directory)? {
        return Ok(Selection {
            path,
            name: String::new(),
            directory: Some(directory),
            implicit: !target.is_explicit(),
        });
    }
    anyhow::ensure!(
        target.directory.is_none(),
        "no .enclo.toml found in {} or its parents",
        directory.display()
    );
    eprintln!("No workspace configuration found; using central container: default");
    Ok(Selection {
        path: config_dir().join("containers/default.toml"),
        name: "default".into(),
        directory: None,
        implicit: true,
    })
}

fn resolve_target_name(target: &TargetArgs) -> Result<String> {
    let selected = select_target(target)?;
    if selected
        .path
        .file_name()
        .is_some_and(|n| n == ".enclo.toml")
    {
        return Ok(load_selection(selected)?.name);
    }
    // Stop/status/reload can address a central instance even if its file was removed.
    Ok(selected.name)
}

fn load_target(target: &TargetArgs) -> Result<LoadedTarget> {
    load_selection(select_target(target)?)
}

fn load_selection(selected: Selection) -> Result<LoadedTarget> {
    let mut config = if selected
        .path
        .file_name()
        .is_some_and(|n| n == ".enclo.toml")
    {
        workspace::load(&selected.path, true)?
    } else {
        Config::load(&selected.path)?
    };
    let path = if config.workspace.is_some() {
        selected
            .path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .canonicalize()?
            .join(".enclo.toml")
    } else {
        std::fs::canonicalize(selected.path)?
    };
    let target_name = if let Some(workspace) = &config.workspace {
        if selected.implicit {
            eprintln!(
                "Using workspace {} (profile: {})",
                workspace.root.display(),
                workspace.profile
            );
        }
        config.container.name.clone()
    } else {
        selected.name
    };
    let ctx = Context::new(config.container.name.clone())?;
    home::configure(&mut config, &ctx)?;
    Ok(LoadedTarget {
        config,
        ctx,
        path,
        name: target_name,
        directory: selected.directory,
    })
}

async fn exec_into_target(
    loaded: &LoadedTarget,
    cmd: &[String],
    workdir: Option<&str>,
    user: Option<&str>,
    env: &[String],
    ensure_running: bool,
) -> Result<()> {
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

    let (shell, extra_cmd) = match cmd.split_first() {
        Some((first, rest)) => (first.as_str(), rest),
        None => (loaded.config.container.exec_shell.as_str(), [].as_slice()),
    };
    if let Some(workspace) = &loaded.config.workspace {
        let running = std::fs::read_to_string(loaded.ctx.enclo_dir.join("workspace-fingerprint"))?;
        anyhow::ensure!(
            running == workspace.fingerprint,
            "workspace configuration changed; run enclo restart before entering"
        );
    }
    let workspace_workdir = loaded.config.workspace.as_ref().map(|workspace| {
        let cwd = loaded
            .directory
            .clone()
            .or_else(|| std::env::current_dir().and_then(|p| p.canonicalize()).ok())
            .unwrap_or_else(|| workspace.root.clone());
        workspace
            .destination
            .join(
                cwd.strip_prefix(&workspace.root)
                    .unwrap_or(std::path::Path::new("")),
            )
            .to_string_lossy()
            .into_owned()
    });
    let workdir = workdir
        .map(|w| w.to_string())
        .or(workspace_workdir)
        .unwrap_or_else(|| {
            resolve_exec_workdir(&loaded.config.container.exec_workdir, &loaded.ctx)
        });

    let registry = build_registry(&loaded.config);
    let mut all_env: Vec<String> = Vec::new();
    for module in &registry {
        if !module.enabled() {
            continue;
        }
        match module.pre_exec(&loaded.ctx).await {
            Ok(output) => {
                for (k, v) in output.env_vars {
                    all_env.push(format!("{k}={v}"));
                }
            }
            Err(e) => tracing::warn!("module {} pre_exec error: {:#}", module.name(), e),
        }
    }
    all_env.extend_from_slice(env);

    let workspace_user = format!("{}:{}", loaded.ctx.uid, loaded.ctx.gid);
    let user = user.or_else(|| loaded.config.modules.systemd.enabled.then_some(workspace_user.as_str()));
    podman::exec(
        &loaded.ctx.container_name,
        shell,
        extra_cmd,
        &exec_env,
        &workdir,
        user,
        &all_env,
        &loaded.config.container.exec_args,
    )
    .await
}

fn resolve_exec_workdir(config_workdir: &Option<String>, ctx: &Context) -> String {
    if let Some(w) = config_workdir {
        return ctx.expand(w);
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ctx.home.to_string_lossy().into_owned())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn named_container_commands_use_independent_backend() {
        let cli = Cli::try_parse_from(["enclo", "container", "start", "dev", "--runtime-dir", "/tmp/enclo-test"]).unwrap();
        assert!(matches!(cli.command, Command::Container { action: ContainerAction::Start(ConnectionName { name: Some(name) }), runtime_dir: Some(root) } if name == "dev" && root == std::path::Path::new("/tmp/enclo-test")));
        assert!(matches!(Cli::try_parse_from(["enclo", "start", "-C", "dev"]).unwrap().command, Command::Start));
    }

    #[test]
    fn named_connections_and_global_discovery_parse() {
        assert!(matches!(Cli::try_parse_from(["enclo", "services", "run", "work"]).unwrap().command,
            Command::Services { action: ServicesAction::Run(ConnectionName { name: Some(name) }), .. } if name == "work"));
        assert!(matches!(Cli::try_parse_from(["enclo", "session", "work", "--", "echo", "arg"]).unwrap().command,
            Command::Session { name: Some(name), cmd, .. } if name == "work" && cmd == ["echo", "arg"]));
        for args in [vec!["enclo", "services", "list"], vec!["enclo", "sessions", "list"], vec!["enclo", "sessions", "list", "work"], vec!["enclo", "sessions", "stop", "--connection", "work", "--all"]] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn services_run_and_session_preserve_remote_arguments() {
        for args in [
            vec!["enclo", "services", "run", "--profile", "dev.toml"],
            vec!["enclo", "services", "--profile", "dev.toml", "run"],
        ] {
            assert!(matches!(Cli::try_parse_from(args).unwrap().command,
                Command::Services { action: ServicesAction::Run(_), .. }));
        }
        let parsed = Cli::try_parse_from([
            "enclo", "session", "--profile", "dev.toml", "--",
            "command", "arg1", "two words", "--profile", "literal", "$(false)",
        ]).unwrap();
        match parsed.command {
            Command::Session { profile, cmd, .. } => {
                assert_eq!(profile, Some(PathBuf::from("dev.toml")));
                assert_eq!(cmd, ["command", "arg1", "two words", "--profile", "literal", "$(false)"]);
            }
            _ => panic!("expected session"),
        }
        assert!(matches!(Cli::try_parse_from(["enclo", "session", "--profile", "dev.toml"]).unwrap().command,
            Command::Session { cmd, .. } if cmd.is_empty()));
        for command in ["connect", "ssh"] {
            assert!(Cli::try_parse_from(["enclo", command, "--profile", "dev.toml"]).is_err());
        }
        assert!(Cli::try_parse_from(["enclo", "services", "start", "--profile", "dev.toml"]).is_ok());
        assert!(Cli::try_parse_from(["enclo", "services", "start", "work", "--timeout-secs", "0"]).is_err());
    }

    #[test]
    fn selectors_work_before_after_and_with_nested_commands() {
        for args in [
            vec!["enclo", "-C", "default", "enter"],
            vec!["enclo", "enter", "--container", "default"],
            vec!["enclo", "home", "-C", "default", "init", "--rerun"],
            vec!["enclo", "home", "init", "--rerun", "-C", "default"],
        ] {
            assert_eq!(
                Cli::try_parse_from(args)
                    .unwrap()
                    .target
                    .container
                    .as_deref(),
                Some("default")
            );
        }
        assert_eq!(
            Cli::try_parse_from(["enclo", "enter", "-c", "test.toml"])
                .unwrap()
                .target
                .config,
            Some("test.toml".into())
        );
        assert_eq!(
            Cli::try_parse_from(["enclo", "enter", "-d", "."])
                .unwrap()
                .target
                .directory,
            Some(".".into())
        );
    }

    #[test]
    fn selectors_conflict_even_across_nested_command_levels() {
        for args in [
            vec!["enclo", "enter", "-d", ".", "-C", "default"],
            vec!["enclo", "-C", "default", "home", "init", "-c", "file.toml"],
            vec!["enclo", "-d", ".", "enter", "-c", "file.toml"],
        ] {
            let parsed = Cli::try_parse_from(args);
            assert!(parsed.is_err() || parsed.unwrap().target.validate().is_err());
        }
    }

    #[test]
    fn no_positional_target_names_and_exec_arguments_are_preserved() {
        for command in [
            "run",
            "start",
            "stop",
            "restart",
            "enter",
            "exec",
            "reload",
            "status",
            "setup-volume",
        ] {
            assert!(Cli::try_parse_from(["enclo", command, "default"]).is_err());
        }
        assert!(Cli::try_parse_from(["enclo", "home", "init", "default"]).is_err());
        let cli = Cli::try_parse_from(["enclo", "exec", "-d", ".", "--", "echo", "-C", "default"])
            .unwrap();
        match cli.command {
            Command::Exec { cmd, .. } => assert_eq!(cmd, ["echo", "-C", "default"]),
            _ => panic!("expected exec"),
        }
        assert!(cli.target.container.is_none());
    }
}
