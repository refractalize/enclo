use anyhow::{Context as _, Result};
use zbus::proxy;

#[proxy(
    interface = "org.insectjar.Manager",
    default_service = "org.insectjar",
    default_path = "/org/insectjar"
)]
trait Manager {
    async fn start(&self, name: &str, config_path: &str) -> zbus::Result<()>;
    async fn stop(&self, name: &str) -> zbus::Result<()>;
    async fn status(&self, name: &str) -> zbus::Result<String>;
    async fn list_containers(&self) -> zbus::Result<Vec<(String, String)>>;
    async fn restart(&self, name: &str, config_path: &str) -> zbus::Result<()>;
    async fn ensure_running(&self, name: &str, config_path: &str) -> zbus::Result<()>;
}

async fn connect() -> Result<ManagerProxy<'static>> {
    let conn = zbus::Connection::session().await
        .context("failed to connect to D-Bus session bus (is insectjar serve running?)")?;
    ManagerProxy::new(&conn).await
        .context("failed to connect to org.insectjar — is insectjar serve running?")
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
    } else {
        let list = proxy.list_containers().await
            .context("failed to list containers")?;
        if list.is_empty() {
            println!("no containers known to daemon");
        } else {
            for (n, s) in &list {
                println!("{}: {}", n, s);
            }
        }
    }
    Ok(())
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
