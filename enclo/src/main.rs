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
use platform::PlatformSupport;

#[derive(Parser)]
#[command(name = "enclo", about = "Modular rootless container launcher")]
struct Cli {
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
    directory: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("enclo=info")),
        )
        .init();

    let cli = Cli::parse();
    cli.target.validate()?;

    match cli.command {
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
