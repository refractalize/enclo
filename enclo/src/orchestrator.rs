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
    module::{Module, ModuleOutput, PostStartCmd},
    podman,
};

pub struct Orchestrator {
    pub modules: Vec<Arc<dyn Module>>,
}

struct SetupState {
    combined: ModuleOutput,
    active: Vec<Arc<dyn Module>>,
}

struct VolumePlan {
    home_volume_name: String,
    home_str: String,
    dirs: Vec<String>,
    files: Vec<String>,
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

        create_enclo_dir(ctx).await?;
        let setup = match self.setup_modules(&ordered, ctx, timestamps.as_ref()).await {
            Ok(setup) => setup,
            Err((module_name, error)) => {
                let err = Err(error).context(format!("module {} setup failed", module_name));
                notify_ready_error(ready, err.as_ref().unwrap_err().to_string());
                return err;
            }
        };

        write_exec_env(ctx, &setup.combined.exec_env_vars).await?;

        let cmd = build_podman_run_args(config, ctx, &setup.combined);
        info!("launching container: podman run {}", format_args_preview(&cmd));

        let mut container_proc = podman::run(&cmd).await?;

        if let Err(error) = self
            .wait_until_running(
                ctx,
                &setup.active,
                &setup.combined.post_start_cmds,
                ready.is_some(),
                &mut container_proc,
            )
            .await
        {
            notify_ready_error(ready, error.to_string());
            return Err(error);
        }

        notify_ready_ok(ready);

        tokio::select! {
            _ = container_proc.wait() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("interrupted — stopping container");
                podman::stop(&ctx.container_name).await.ok();
                let _ = container_proc.wait().await;
            }
        }

        self.teardown_modules(ctx, &setup.active).await;
        Ok(())
    }


    pub async fn setup_volume(&self, config: &Config, ctx: &Context, dry_run: bool) -> Result<()> {
        let plan = build_volume_plan(config, ctx)?;

        if plan.dirs.is_empty() && plan.files.is_empty() {
            println!("no entries to create in volume '{}'", plan.home_volume_name);
            return Ok(());
        }

        if dry_run {
            println!("in volume '{}':", plan.home_volume_name);
            for d in &plan.dirs {
                println!("  mkdir  {}", d);
            }
            for f in &plan.files {
                println!("  touch  {}", f);
            }
            println!("  (then fix any remaining root-owned entries under {})", plan.home_str);
            return Ok(());
        }
        let sh_cmd = build_volume_setup_command(ctx.uid, &plan);
        info!("setting up home volume '{}'", plan.home_volume_name);
        let status = tokio::process::Command::new("podman")
            .args([
                "run", "--rm",
                "--user", "root",
                "--userns", "keep-id",
                "--volume", &format!("{}:{}", plan.home_volume_name, plan.home_str),
                &config.container.image,
                "sh", "-c", &sh_cmd,
            ])
            .status()
            .await
            .context("failed to spawn podman run for setup-volume")?;

        if !status.success() {
            anyhow::bail!("setup-volume podman run exited with {}", status);
        }

        info!("volume '{}' set up successfully", plan.home_volume_name);
        Ok(())
    }

    async fn setup_modules(
        &self,
        ordered: &[Arc<dyn Module>],
        ctx: &Context,
        timestamps: Option<&Arc<Mutex<HashMap<String, SystemTime>>>>,
    ) -> std::result::Result<SetupState, (String, anyhow::Error)> {
        let mut combined = ModuleOutput::default();
        let mut active: Vec<Arc<dyn Module>> = Vec::new();

        for module in ordered {
            if !ctx.support.supports_module(module.name()) {
                info!("skipping module {} (unsupported platform)", module.name());
                continue;
            }
            info!("setting up module: {}", module.name());
            match module.setup(ctx).await {
                Ok(output) => {
                    if let Some(ts) = timestamps {
                        ts.lock().await.insert(module.name().to_string(), SystemTime::now());
                    }
                    combined.merge(output);
                    active.push(Arc::clone(module));
                }
                Err(error) => {
                    warn!("module {} failed: {:#}", module.name(), error);
                    remove_exec_env(ctx).await;
                    self.teardown_modules(ctx, &active).await;
                    return Err((module.name().to_string(), error));
                }
            }
        }

        Ok(SetupState { combined, active })
    }

    async fn wait_until_running(
        &self,
        ctx: &Context,
        active: &[Arc<dyn Module>],
        post_start_cmds: &[PostStartCmd],
        wait_for_ready: bool,
        container_proc: &mut tokio::process::Child,
    ) -> Result<()> {
        if !wait_for_ready && post_start_cmds.is_empty() {
            return Ok(());
        }

        if let Err(error) = podman::wait_running(&ctx.container_name, 30).await {
            podman::stop(&ctx.container_name).await.ok();
            let _ = container_proc.wait().await;
            self.teardown_modules(ctx, active).await;
            return Err(anyhow::anyhow!("{:#}", error));
        }

        run_post_start_cmds(&ctx.container_name, post_start_cmds).await;
        Ok(())
    }

    async fn teardown_modules(&self, ctx: &Context, active: &[Arc<dyn Module>]) {
        remove_exec_env(ctx).await;
        for module in active.iter().rev() {
            info!("tearing down module: {}", module.name());
            if let Err(error) = module.teardown(ctx).await {
                warn!("module {} teardown error: {:#}", module.name(), error);
            }
        }
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

fn notify_ready_ok(ready: Option<tokio::sync::oneshot::Sender<Result<()>>>) {
    if let Some(tx) = ready {
        let _ = tx.send(Ok(()));
    }
}

fn notify_ready_error(ready: Option<tokio::sync::oneshot::Sender<Result<()>>>, message: String) {
    if let Some(tx) = ready {
        let _ = tx.send(Err(anyhow::anyhow!("{}", message)));
    }
}

async fn run_post_start_cmds(container_name: &str, post_start_cmds: &[PostStartCmd]) {
    for post in post_start_cmds {
        let user = post.user.as_deref().unwrap_or("root");
        info!("running post-start: {:?} (user={})", post.args, user);
        if let Err(error) = podman::exec_as(container_name, user, &post.args).await {
            warn!("post-start command failed: {:#}", error);
        }
    }
}

fn build_volume_plan(config: &Config, ctx: &Context) -> Result<VolumePlan> {
    let home_str = ctx.home.to_string_lossy().into_owned();
    let home_volume_name = config.container.volumes.iter().find_map(|volume| {
        let (host, container, _opts) = split_volume(volume, ctx);
        if container == home_str {
            Some(host)
        } else {
            None
        }
    }).ok_or_else(|| anyhow::anyhow!(
        "no volume mapped to {} found in [container].volumes", home_str
    ))?;

    if home_volume_name.starts_with('/') || home_volume_name.starts_with('.') {
        anyhow::bail!(
            "home volume '{}' is a host path, not a named podman volume",
            home_volume_name
        );
    }

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for volume in &config.container.volumes {
        let (host, container, _opts) = split_volume(volume, ctx);
        if container == home_str || !container.starts_with(&home_str) {
            continue;
        }
        if std::path::Path::new(&host).is_file() {
            if let Some(parent) = std::path::Path::new(&container).parent() {
                dirs.push(parent.to_string_lossy().into_owned());
            }
            files.push(container);
        } else {
            dirs.push(container);
        }
    }

    Ok(VolumePlan {
        home_volume_name,
        home_str: home_str.clone(),
        dirs: expand_volume_dirs(&home_str, dirs),
        files: dedupe_sorted(files),
    })
}

fn expand_volume_dirs(home_str: &str, dirs: Vec<String>) -> Vec<String> {
    let home_path = std::path::Path::new(home_str);
    let mut all_dirs = std::collections::BTreeSet::new();
    for dir in &dirs {
        let mut path = std::path::Path::new(dir);
        while let Some(parent) = path.parent() {
            if path == home_path || parent == home_path || !path.starts_with(home_path) {
                break;
            }
            all_dirs.insert(path.to_string_lossy().into_owned());
            path = parent;
        }
        all_dirs.insert(dir.clone());
    }
    all_dirs.into_iter().collect()
}

fn dedupe_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn build_volume_setup_command(uid: u32, plan: &VolumePlan) -> String {
    let mut sh_parts = Vec::new();
    if !plan.dirs.is_empty() {
        let args = plan.dirs.iter().map(|dir| shell_quote(dir)).collect::<Vec<_>>().join(" ");
        sh_parts.push(format!("mkdir -p {}", args));
    }
    if !plan.files.is_empty() {
        let args = plan.files.iter().map(|file| shell_quote(file)).collect::<Vec<_>>().join(" ");
        sh_parts.push(format!("touch {}", args));
    }

    let explicit: Vec<String> = plan
        .dirs
        .iter()
        .chain(plan.files.iter())
        .map(|path| shell_quote(path))
        .collect();
    sh_parts.push(format!("chown {}:{} {}", uid, uid, explicit.join(" ")));
    sh_parts.push(format!(
        "find {} -uid 0 -exec chown {}:{} {{}} +",
        shell_quote(&plan.home_str), uid, uid
    ));

    sh_parts.join(" && ")
}
