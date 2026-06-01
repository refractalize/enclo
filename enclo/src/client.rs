use anyhow::Result;
#[cfg(not(target_os = "macos"))]
use anyhow::Context as _;

// ── Linux / D-Bus client ─────────────────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
mod dbus {
    use anyhow::{Context as _, Result};
    use zbus::proxy;

    #[proxy(
        interface = "org.enclo.Manager",
        default_service = "org.enclo",
        default_path = "/org/enclo"
    )]
    pub(super) trait Manager {
        async fn start(&self, name: &str, config_path: &str) -> zbus::Result<()>;
        async fn stop(&self, name: &str, force: bool) -> zbus::Result<()>;
        async fn status(&self, name: &str) -> zbus::Result<String>;
        async fn list_containers(&self) -> zbus::Result<Vec<(String, String)>>;
        async fn restart(&self, name: &str, config_path: &str, force: bool) -> zbus::Result<()>;
        async fn ensure_running(&self, name: &str, config_path: &str) -> zbus::Result<()>;
        async fn reload(&self, name: &str) -> zbus::Result<()>;
        async fn module_statuses(&self, name: &str) -> zbus::Result<Vec<(String, String, i64)>>;
        async fn user_exec_count(&self, name: &str) -> zbus::Result<u32>;
    }

    pub async fn connect() -> Result<ManagerProxy<'static>> {
        let conn = zbus::Connection::session().await
            .context("failed to connect to D-Bus session bus (is enclo serve running?)")?;
        ManagerProxy::new(&conn).await
            .context("failed to connect to org.enclo — is enclo serve running?")
    }
}

// ── macOS / Unix socket client ───────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod socket {
    use anyhow::{bail, Context as _, Result};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use crate::ipc::{Request, Response};
    use crate::platform::daemon_socket;

    pub async fn call(req: &Request) -> Result<Response> {
        let path = daemon_socket();
        let stream = UnixStream::connect(&path).await
            .with_context(|| format!(
                "failed to connect to enclo daemon at {} — is enclo serve running?",
                path.display()
            ))?;

        let (read_half, mut write_half) = stream.into_split();
        let json = serde_json::to_string(req).context("failed to serialize request")?;
        write_half.write_all((json + "\n").as_bytes()).await
            .context("failed to send request to daemon")?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.context("failed to read response from daemon")?;

        serde_json::from_str::<Response>(line.trim()).context("failed to parse daemon response")
    }

    pub async fn checked(req: &Request, context: &str) -> Result<Response> {
        let resp = call(req).await?;
        if !resp.ok {
            bail!("{}: {}", context, resp.error.as_deref().unwrap_or("unknown error"));
        }
        Ok(resp)
    }
}

// ── Public interface ─────────────────────────────────────────────────────────

pub async fn start(name: &str, config_path: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        socket::checked(
            &crate::ipc::Request::Start { name: name.to_string(), config_path: config_path.to_string() },
            &format!("failed to start container '{name}'"),
        ).await?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proxy = dbus::connect().await?;
        proxy.start(name, config_path).await
            .with_context(|| format!("failed to start container '{name}'"))
    }
}

pub async fn stop(name: &str, force: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        socket::checked(
            &crate::ipc::Request::Stop { name: name.to_string(), force },
            &format!("failed to stop container '{name}'"),
        ).await?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proxy = dbus::connect().await?;
        proxy.stop(name, force).await
            .with_context(|| format!("failed to stop container '{name}'"))
    }
}

pub async fn restart(name: &str, config_path: &str, force: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        socket::checked(
            &crate::ipc::Request::Restart { name: name.to_string(), config_path: config_path.to_string(), force },
            &format!("failed to restart container '{name}'"),
        ).await?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proxy = dbus::connect().await?;
        proxy.restart(name, config_path, force).await
            .with_context(|| format!("failed to restart container '{name}'"))
    }
}

pub async fn reload(name: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        socket::checked(
            &crate::ipc::Request::Reload { name: name.to_string() },
            &format!("failed to reload container '{name}'"),
        ).await?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proxy = dbus::connect().await?;
        proxy.reload(name).await
            .with_context(|| format!("failed to reload container '{name}'"))
    }
}

pub async fn ensure_running(name: &str, config_path: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        socket::checked(
            &crate::ipc::Request::EnsureRunning { name: name.to_string(), config_path: config_path.to_string() },
            &format!("failed to ensure container '{name}' is running"),
        ).await?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proxy = dbus::connect().await?;
        proxy.ensure_running(name, config_path).await
            .with_context(|| format!("failed to ensure container '{name}' is running"))
    }
}

pub async fn status(name: Option<&str>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use crate::ipc::Request;

        if let Some(n) = name {
            let resp = socket::checked(
                &Request::Status { name: n.to_string() },
                &format!("failed to get status for '{n}'"),
            ).await?;
            let status_str = resp.value
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let sessions = socket::call(&Request::UserExecCount { name: n.to_string() }).await
                .ok()
                .and_then(|r| r.value)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            println!("{}", format_status_line(n, status_str, sessions));

            if let Ok(resp2) = socket::call(&Request::ModuleStatuses { name: n.to_string() }).await {
                if resp2.ok {
                    if let Some(v) = resp2.value {
                        if let Ok(modules) = serde_json::from_value::<Vec<(String, String, i64)>>(v) {
                            print_module_table(&modules);
                        }
                    }
                }
            }
        } else {
            let resp = socket::checked(&Request::ListContainers, "failed to list containers").await?;
            if let Some(v) = resp.value {
                if let Ok(list) = serde_json::from_value::<Vec<(String, String)>>(v) {
                    if list.is_empty() {
                        println!("no containers known to daemon");
                    } else {
                        for (n, s) in &list {
                            let sessions = socket::call(&Request::UserExecCount { name: n.clone() }).await
                                .ok()
                                .and_then(|r| r.value)
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32;
                            println!("{}", format_status_line(n, s, sessions));
                            if let Ok(resp2) = socket::call(&Request::ModuleStatuses { name: n.clone() }).await {
                                if resp2.ok {
                                    if let Some(v2) = resp2.value {
                                        if let Ok(modules) = serde_json::from_value::<Vec<(String, String, i64)>>(v2) {
                                            print_module_table(&modules);
                                        }
                                    }
                                }
                            }
                            println!();
                        }
                    }
                }
            }
        }
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proxy = dbus::connect().await?;
        if let Some(n) = name {
            let s = proxy.status(n).await
                .with_context(|| format!("failed to get status for '{n}'"))?;
            let sessions = proxy.user_exec_count(n).await.unwrap_or(0);
            println!("{}", format_status_line(n, &s, sessions));
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
                    let sessions = proxy.user_exec_count(n).await.unwrap_or(0);
                    println!("{}", format_status_line(n, s, sessions));
                    if let Ok(modules) = proxy.module_statuses(n).await {
                        print_module_table(&modules);
                    }
                    println!();
                }
            }
        }
        Ok(())
    }
}

fn format_status_line(name: &str, status: &str, user_sessions: u32) -> String {
    if user_sessions == 0 {
        format!("{name}: {status}")
    } else {
        let noun = if user_sessions == 1 { "session" } else { "sessions" };
        format!("{name}: {status} ({user_sessions} exec {noun})")
    }
}

fn print_module_table(modules: &[(String, String, i64)]) {
    if modules.is_empty() { return; }
    let name_width = modules.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0).max(6);
    println!("  {:<width$}  {}", "MODULE", "PROCESS", width = name_width);
    for (mname, process, _) in modules {
        println!("  {:<width$}  {}", mname, process, width = name_width);
    }
}
