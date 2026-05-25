use anyhow::{Context as _, Result};
use zbus::proxy;

#[proxy(
    interface = "org.enclo.Manager",
    default_service = "org.enclo",
    default_path = "/org/enclo"
)]
trait Manager {
    async fn start(&self, name: &str, config_path: &str) -> zbus::Result<()>;
    async fn stop(&self, name: &str) -> zbus::Result<()>;
    async fn status(&self, name: &str) -> zbus::Result<String>;
    async fn list_containers(&self) -> zbus::Result<Vec<(String, String)>>;
    async fn restart(&self, name: &str, config_path: &str) -> zbus::Result<()>;
    async fn ensure_running(&self, name: &str, config_path: &str) -> zbus::Result<()>;
    async fn reload(&self, name: &str) -> zbus::Result<()>;
    async fn module_statuses(&self, name: &str) -> zbus::Result<Vec<(String, String, i64)>>;
}

async fn connect() -> Result<ManagerProxy<'static>> {
    let conn = zbus::Connection::session().await
        .context("failed to connect to D-Bus session bus (is enclo serve running?)")?;
    ManagerProxy::new(&conn).await
        .context("failed to connect to org.enclo — is enclo serve running?")
}

pub async fn start(name: &str, config_path: &str) -> Result<()> {
    let proxy = connect().await?;
    proxy.start(name, config_path).await
        .with_context(|| format!("failed to start container '{}'", name))
}

pub async fn stop(name: &str) -> Result<()> {
    let proxy = connect().await?;
    proxy.stop(name).await
        .with_context(|| format!("failed to stop container '{}'", name))
}

pub async fn status(name: Option<&str>) -> Result<()> {
    let proxy = connect().await?;
    if let Some(n) = name {
        let s = proxy.status(n).await
            .with_context(|| format!("failed to get status for '{}'", n))?;
        println!("{}: {}", n, s);

        if let Ok(modules) = proxy.module_statuses(n).await {
            print_module_table(&modules);
        }
    } else {
        let list = proxy.list_containers().await
            .context("failed to list containers")?;
        if list.is_empty() {
            println!("no containers known to daemon");
        } else {
            for (n, s) in &list {
                println!("{}: {}", n, s);
                if let Ok(modules) = proxy.module_statuses(n).await {
                    print_module_table(&modules);
                }
                println!();
            }
        }
    }
    Ok(())
}

fn print_module_table(modules: &[(String, String, i64)]) {
    if modules.is_empty() { return; }
    let name_width = modules.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0).max(6);
    println!("  {:<width$}  {}", "MODULE", "PROCESS", width = name_width);
    for (mname, process, _) in modules {
        println!("  {:<width$}  {}", mname, process, width = name_width);
    }
}

pub async fn restart(name: &str, config_path: &str) -> Result<()> {
    let proxy = connect().await?;
    proxy.restart(name, config_path).await
        .with_context(|| format!("failed to restart container '{}'", name))
}

pub async fn ensure_running(name: &str, config_path: &str) -> Result<()> {
    let proxy = connect().await?;
    proxy.ensure_running(name, config_path).await
        .with_context(|| format!("failed to ensure container '{}' is running", name))
}

pub async fn reload(name: &str) -> Result<()> {
    let proxy = connect().await?;
    proxy.reload(name).await
        .with_context(|| format!("failed to reload container '{}'", name))
}
