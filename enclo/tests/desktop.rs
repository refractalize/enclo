//! Real proxy against an isolated bus; never opens the user's desktop bus.
use enclo::desktop::{DbusConfig, Desktop, DesktopConfig, Plan};
use std::{fs, os::unix::fs::DirBuilderExt, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Echo;
#[zbus::interface(name = "org.enclo.Test")]
impl Echo {
    fn ping(&self) -> &str {
        "pong"
    }
}

#[tokio::test]
#[ignore = "requires dbus-daemon, xdg-dbus-proxy and permission to bind local fixture sockets"]
async fn filtered_bus_permissions_lock_revocation_and_restart() {
    let root = std::env::temp_dir().join(format!(
        "ed-{}",
        rand_core::RngCore::next_u64(&mut rand_core::OsRng)
    ));
    let _fixture = Fixture(root.clone());
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.join("sockets"))
        .unwrap();
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
        .arg(format!(
            "--address=unix:path={}/private-bus",
            root.display()
        ))
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .await
        .unwrap();
    let address = address.trim();
    let _allowed = zbus::connection::Builder::address(address)
        .unwrap()
        .name("org.enclo.Allowed")
        .unwrap()
        .serve_at("/test", Echo)
        .unwrap()
        .build()
        .await
        .unwrap();
    let _denied = zbus::connection::Builder::address(address)
        .unwrap()
        .name("org.enclo.Denied")
        .unwrap()
        .serve_at("/test", Echo)
        .unwrap()
        .build()
        .await
        .unwrap();
    let plan = Plan {
        directory: root.clone(),
        app_id: "fixture".into(),
        config: DesktopConfig {
            wayland: false,
            dbus: DbusConfig {
                address: Some(address.into()),
                enabled: true,
                talk: vec!["org.enclo.Allowed".into()],
                ..Default::default()
            },
        },
    };
    let mut service = Desktop::start(plan.clone()).await.unwrap();
    assert!(Desktop::start(plan.clone()).await.is_err());
    let proxy_address = format!("unix:path={}/sockets/bus", root.display());
    let client = zbus::connection::Builder::address(proxy_address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let response = client
        .call_method(
            Some("org.enclo.Allowed"),
            "/test",
            Some("org.enclo.Test"),
            "Ping",
            &(),
        )
        .await
        .unwrap();
    assert_eq!(response.body().deserialize::<String>().unwrap(), "pong");
    assert!(client
        .call_method(
            Some("org.enclo.Denied"),
            "/test",
            Some("org.enclo.Test"),
            "Ping",
            &()
        )
        .await
        .is_err());
    assert!(!service.exited().unwrap());
    service.stop().await.unwrap();
    drop(service);
    assert!(!root.join("sockets/bus").exists());
    assert!(client
        .call_method(
            Some("org.enclo.Allowed"),
            "/test",
            Some("org.enclo.Test"),
            "Ping",
            &()
        )
        .await
        .is_err());
    let mut restarted = Desktop::start(plan).await.unwrap();
    let client = zbus::connection::Builder::address(proxy_address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    client
        .call_method(
            Some("org.enclo.Allowed"),
            "/test",
            Some("org.enclo.Test"),
            "Ping",
            &(),
        )
        .await
        .unwrap();
    restarted.stop().await.unwrap();
    daemon.kill().await.unwrap();
}
