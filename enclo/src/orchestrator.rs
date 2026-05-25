use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    config::Config,
    context::Context,
    module::{Module, ModuleOutput},
    podman,
};

pub struct Orchestrator {
    pub modules: Vec<Arc<dyn Module>>,
}

impl Orchestrator {
    pub fn new(modules: Vec<Arc<dyn Module>>) -> Self {
        Self { modules }
    }

    /// Returns enabled modules in topological order.
    fn sorted(modules: &[Arc<dyn Module>]) -> Result<Vec<Arc<dyn Module>>> {
        let by_name: HashMap<&str, Arc<dyn Module>> = modules
            .iter()
            .filter(|m| m.enabled())
            .map(|m| (m.name(), Arc::clone(m)))
            .collect();
        let enabled_set: HashSet<&str> = by_name.keys().copied().collect();

        // Kahn's algorithm
        let nodes: Vec<&str> = by_name.keys().copied().collect();
        let mut in_degree: HashMap<&str, usize> = nodes.iter().map(|&n| (n, 0)).collect();
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

        for &node in &nodes {
            if let Some(m) = by_name.get(node) {
                for &dep in m.dependencies() {
                    if enabled_set.contains(dep) {
                        adj.entry(dep).or_default().push(node);
                        *in_degree.entry(node).or_insert(0) += 1;
                    }
                }
            }
        }

        let mut queue: VecDeque<&str> = nodes
            .iter()
            .filter(|&&n| *in_degree.get(n).unwrap_or(&0) == 0)
            .copied()
            .collect();

        let mut order = Vec::new();
        while let Some(n) = queue.pop_front() {
            order.push(n);
            if let Some(neighbors) = adj.get(n) {
                for &nb in neighbors {
                    let deg = in_degree.entry(nb).or_insert(0);
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(nb);
                    }
                }
            }
        }

        if order.len() != nodes.len() {
            anyhow::bail!("cycle detected in module dependencies");
        }

        Ok(order
            .into_iter()
            .filter_map(|name| by_name.get(name).map(Arc::clone))
            .collect())
    }

    /// Start the container. If `ready` is provided, it is resolved once the
    /// container is running and post-start commands have completed, allowing
    /// callers (e.g. the daemon) to signal "Running" before the container exits.
    /// If `timestamps` is provided, it is updated with the time each module's
    /// setup completes (and again on each reload via the daemon's reload handler).
    pub async fn start(
        &self,
        config: &Config,
        ctx: &Context,
        ready: Option<tokio::sync::oneshot::Sender<Result<()>>>,
        timestamps: Option<Arc<Mutex<HashMap<String, SystemTime>>>>,
    ) -> Result<()> {
        let ordered = Self::sorted(&self.modules)?;
        let platform = &ctx.platform;

        create_enclo_dir(ctx).await?;

        let mut combined = ModuleOutput::default();
        let mut active: Vec<Arc<dyn Module>> = Vec::new();

        for module in &ordered {
            if !module.supported_platforms().contains(platform) {
                info!("skipping module {} (unsupported platform)", module.name());
                continue;
            }
            info!("setting up module: {}", module.name());
            match module.setup(ctx).await {
                Ok(output) => {
                    if let Some(ts) = &timestamps {
                        ts.lock().await.insert(module.name().to_string(), SystemTime::now());
                    }
                    combined.merge(output);
                    active.push(Arc::clone(module));
                }
                Err(e) => {
                    warn!("module {} failed: {:#}", module.name(), e);
                    remove_exec_env(ctx).await;
                    for m in active.iter().rev() {
                        let _ = m.teardown(ctx).await;
                    }
                    let err = Err(e).context(format!("module {} setup failed", module.name()));
                    if let Some(tx) = ready {
                        let msg = err.as_ref().unwrap_err().to_string();
                        let _ = tx.send(Err(anyhow::anyhow!("{}", msg)));
                    }
                    return err;
                }
            }
        }

        write_exec_env(ctx, &combined.exec_env_vars).await?;

        let cmd = build_podman_run_args(config, ctx, &combined);
        info!("launching container: podman run {}", format_args_preview(&cmd));

        let mut container_proc = podman::run(&cmd).await?;

        if ready.is_some() || !combined.post_start_cmds.is_empty() {
            match podman::wait_running(&ctx.container_name, 30).await {
                Ok(()) => {
                    for post in &combined.post_start_cmds {
                        let user = post.user.as_deref().unwrap_or("root");
                        info!("running post-start: {:?} (user={})", post.args, user);
                        if let Err(e) = podman::exec_as(&ctx.container_name, user, &post.args).await {
                            warn!("post-start command failed: {:#}", e);
                        }
                    }
                }
                Err(e) => {
                    podman::stop(&ctx.container_name).await.ok();
                    let _ = container_proc.wait().await;
                    remove_exec_env(ctx).await;
                    for m in active.iter().rev() {
                        let _ = m.teardown(ctx).await;
                    }
                    let err = anyhow::anyhow!("{:#}", e);
                    if let Some(tx) = ready {
                        let _ = tx.send(Err(anyhow::anyhow!("{}", err)));
                    }
                    return Err(err);
                }
            }
        }

        // Signal that the container is now running
        if let Some(tx) = ready {
            let _ = tx.send(Ok(()));
        }

        tokio::select! {
            _ = container_proc.wait() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("interrupted — stopping container");
                podman::stop(&ctx.container_name).await.ok();
                let _ = container_proc.wait().await;
            }
        }

        // Container exited — teardown in reverse
        remove_exec_env(ctx).await;
        for m in active.iter().rev() {
            info!("tearing down module: {}", m.name());
            if let Err(e) = m.teardown(ctx).await {
                warn!("module {} teardown error: {:#}", m.name(), e);
            }
        }


        Ok(())
    }


    pub async fn setup_volume(&self, config: &Config, ctx: &Context, dry_run: bool) -> Result<()> {
        let home_str = ctx.home.to_string_lossy();

        // Find the volume entry whose container path is exactly the home directory.
        let home_volume_name = config.container.volumes.iter().find_map(|v| {
            let (host, container, _opts) = split_volume(v, ctx);
            if container == home_str.as_ref() {
                Some(host)
            } else {
                None
            }
        }).ok_or_else(|| anyhow::anyhow!(
            "no volume mapped to {} found in [container].volumes", home_str
        ))?;

        // Reject host-path bind mounts — must be a named podman volume.
        if home_volume_name.starts_with('/') || home_volume_name.starts_with('.') {
            anyhow::bail!(
                "home volume '{}' is a host path, not a named podman volume",
                home_volume_name
            );
        }

        // Collect mount-point directories and files inside the home volume.
        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();

        for v in &config.container.volumes {
            let (host, container, _opts) = split_volume(v, ctx);
            if container == home_str.as_ref() {
                continue; // skip the home volume itself
            }
            if !container.starts_with(home_str.as_ref()) {
                continue; // not inside home
            }
            let host_path = std::path::Path::new(&host);
            if host_path.is_file() {
                // File mount: touch the file and ensure its parent directory exists.
                if let Some(parent) = std::path::Path::new(&container).parent() {
                    dirs.push(parent.to_string_lossy().into_owned());
                }
                files.push(container);
            } else {
                dirs.push(container);
            }
        }

        // Expand each dir to include all intermediate ancestors between home and the leaf,
        // so that mkdir's intermediate dirs also get chowned.
        let home_path = std::path::Path::new(home_str.as_ref());
        let mut all_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for d in &dirs {
            let mut p = std::path::Path::new(d);
            while let Some(parent) = p.parent() {
                if p == home_path || parent == home_path || !p.starts_with(home_path) {
                    break;
                }
                all_dirs.insert(p.to_string_lossy().into_owned());
                p = parent;
            }
            all_dirs.insert(d.clone());
        }
        let dirs: Vec<String> = all_dirs.into_iter().collect();

        files.sort();
        files.dedup();

        if dirs.is_empty() && files.is_empty() {
            println!("no entries to create in volume '{}'", home_volume_name);
            return Ok(());
        }

        if dry_run {
            println!("in volume '{}':", home_volume_name);
            for d in &dirs {
                println!("  mkdir  {}", d);
            }
            for f in &files {
                println!("  touch  {}", f);
            }
            println!("  (then fix any remaining root-owned entries under {})", home_str);
            return Ok(());
        }

        // Build sh command: mkdir dirs, touch files, chown explicit entries,
        // then fix any remaining root-owned items (e.g. module-produced mount points).
        let uid = ctx.uid;
        let mut sh_parts: Vec<String> = Vec::new();

        if !dirs.is_empty() {
            let args = dirs.iter().map(|d| shell_quote(d)).collect::<Vec<_>>().join(" ");
            sh_parts.push(format!("mkdir -p {}", args));
        }
        if !files.is_empty() {
            let args = files.iter().map(|f| shell_quote(f)).collect::<Vec<_>>().join(" ");
            sh_parts.push(format!("touch {}", args));
        }
        let explicit: Vec<String> = dirs.iter().chain(files.iter())
            .map(|p| shell_quote(p))
            .collect();
        sh_parts.push(format!("chown {}:{} {}", uid, uid, explicit.join(" ")));
        // Fix any remaining root-owned entries (e.g. from previous runs or module mounts).
        sh_parts.push(format!(
            "find {} -uid 0 -exec chown {}:{} {{}} +",
            shell_quote(home_str.as_ref()), uid, uid
        ));

        let sh_cmd = sh_parts.join(" && ");

        info!("setting up home volume '{}'", home_volume_name);
        let status = tokio::process::Command::new("podman")
            .args([
                "run", "--rm",
                "--user", "root",
                "--userns", "keep-id",
                "--volume", &format!("{}:{}", home_volume_name, home_str),
                &config.container.image,
                "sh", "-c", &sh_cmd,
            ])
            .status()
            .await
            .context("failed to spawn podman run for setup-volume")?;

        if !status.success() {
            anyhow::bail!("setup-volume podman run exited with {}", status);
        }

        info!("volume '{}' set up successfully", home_volume_name);
        Ok(())
    }
}

fn build_podman_run_args(
    config: &Config,
    ctx: &Context,
    output: &ModuleOutput,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();

    args.push("--name".into());
    args.push(config.container.name.clone().into());

    args.push("--rm".into());

    args.push("--network".into());
    args.push(config.container.network.clone().into());

    args.push("--userns".into());
    args.push(config.container.userns.clone().into());

    if config.container.keep_groups {
        args.push("--group-add=keep-groups".into());
    }

    if config.container.security_label_disable {
        args.push("--security-opt=label=disable".into());
    }

    for (k, v) in &output.env_vars {
        args.push("--env".into());
        args.push(format!("{}={}", k, v).into());
    }

    for vol in &output.volumes {
        args.push("--volume".into());
        args.push(vol.to_arg());
    }

    for vol in &config.container.volumes {
        args.push("--volume".into());
        args.push(ctx.expand(vol).into());
    }

    for extra in &output.podman_args {
        args.push(extra.clone());
    }

    args.push(config.container.image.clone().into());

    for part in &config.container.command {
        args.push(part.into());
    }

    args
}

/// Splits a volume string `host:container[:opts]` into its three parts,
/// expanding specifiers in host and container paths.
fn split_volume<'a>(v: &'a str, ctx: &Context) -> (String, String, Option<&'a str>) {
    let parts: Vec<&str> = v.splitn(3, ':').collect();
    let host = ctx.expand(parts[0]);
    let container = if parts.len() > 1 { ctx.expand(parts[1]) } else { host.clone() };
    let opts = if parts.len() > 2 { Some(parts[2]) } else { None };
    (host, container, opts)
}

/// Quotes a path for use in a sh -c command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn exec_env_path(ctx: &Context) -> std::path::PathBuf {
    ctx.enclo_dir.join("exec-env")
}

async fn write_exec_env(ctx: &Context, vars: &[(String, String)]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = exec_env_path(ctx);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to create exec-env file at {}", path.display()))?;
    for (k, v) in vars {
        writeln!(f, "{}={}", k, v)?;
    }
    Ok(())
}

async fn remove_exec_env(ctx: &Context) {
    let path = exec_env_path(ctx);
    if path.exists() {
        let _ = tokio::fs::remove_file(&path).await;
    }
}

async fn create_enclo_dir(ctx: &Context) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(&ctx.enclo_dir).await?;
    std::fs::set_permissions(&ctx.enclo_dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn format_args_preview(args: &[OsString]) -> String {
    args.iter()
        .take(6)
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

