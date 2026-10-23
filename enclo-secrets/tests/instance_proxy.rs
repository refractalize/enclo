use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Writes a proxy-only secrets config with a throwaway CA.
fn write_config(dir: &Path, listen: Option<SocketAddr>) -> PathBuf {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["test.invalid".into()]).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key).unwrap();
    fs::write(dir.join("ca.pem"), cert.pem()).unwrap();
    fs::write(dir.join("ca.key"), key.serialize_pem()).unwrap();
    let listen = listen.map(|a| format!("listen='{a}'\n")).unwrap_or_default();
    let config = dir.join("secrets.toml");
    fs::write(&config, format!("[store]\n[secrets.socket]\nenabled=false\n[proxy]\nenabled=true\n{listen}[proxy.tls]\nca_cert='ca.pem'\nca_key='ca.key'\n")).unwrap();
    config
}

/// Returns a currently free local port.
fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

fn spawn(config: &Path, listen: Option<&str>, log: &Path) -> Process {
    let log = fs::File::create(log).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_enclo-secrets"));
    command.args(["serve", "--config"]).arg(config);
    if let Some(listen) = listen {
        command.args(["--listen", listen]);
    }
    Process(
        command
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    )
}

/// Spawns a sidecar on an inherited listener, as the enclo secrets module does.
fn spawn_with_fd(config: &Path, log: &Path) -> (Process, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let fd = listener.as_raw_fd();
    let log = fs::File::create(log).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_enclo-secrets"));
    command
        .args(["serve", "--config"])
        .arg(config)
        .args(["--listen-fd", &fd.to_string()])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (Process(command.spawn().unwrap()), addr)
}

fn wait_log(path: &Path, process: &mut Process, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = fs::read_to_string(path).unwrap_or_default();
        assert!(!log.contains("config reload failed"), "{log}");
        if log.contains(needle) {
            return;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "sidecar exited before logging {needle:?}: {log}"
        );
        assert!(Instant::now() < deadline, "sidecar did not log {needle:?}: {log}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_exit(process: &mut Process) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "sidecar did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sidecars_share_config_but_not_inherited_listeners_across_reload() {
    let dir = tempfile::tempdir().unwrap();
    // Occupy the configured port. Both sidecars must serve their inherited
    // listener, including after SIGHUP, rather than attempting to bind it.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(dir.path(), Some(occupied.local_addr().unwrap()));
    let (a_log, b_log) = (dir.path().join("a.log"), dir.path().join("b.log"));
    let (mut first, a) = spawn_with_fd(&config, &a_log);
    let (mut second, b) = spawn_with_fd(&config, &b_log);
    wait_log(&a_log, &mut first, &format!("HTTP listener on {a}"));
    wait_log(&b_log, &mut second, &format!("HTTP listener on {b}"));
    assert_ne!(a, b);
    assert!(TcpStream::connect(a).is_ok());
    assert!(TcpStream::connect(b).is_ok());
    unsafe {
        libc::kill(first.0.id() as i32, libc::SIGHUP);
    }
    wait_log(&a_log, &mut first, "config reloaded");
    assert!(TcpStream::connect(a).is_ok());
    assert!(first.0.try_wait().unwrap().is_none());
    assert!(second.0.try_wait().unwrap().is_none());
}

#[test]
fn inherited_listener_does_not_require_configured_address() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), None);
    let log = dir.path().join("sidecar.log");
    let (mut process, addr) = spawn_with_fd(&config, &log);
    wait_log(&log, &mut process, &format!("HTTP listener on {addr}"));
}

#[test]
fn listen_override_replaces_configured_address_across_reload() {
    let dir = tempfile::tempdir().unwrap();
    // Occupy the configured port. Both sidecars must use their --listen
    // override, including after SIGHUP, rather than attempting to bind it.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(dir.path(), Some(occupied.local_addr().unwrap()));
    let (a, b) = (free_addr(), free_addr());
    let (a_log, b_log) = (dir.path().join("a.log"), dir.path().join("b.log"));
    let mut first = spawn(&config, Some(&a.to_string()), &a_log);
    let mut second = spawn(&config, Some(&b.to_string()), &b_log);
    wait_log(&a_log, &mut first, &format!("HTTP listener on {a}"));
    wait_log(&b_log, &mut second, &format!("HTTP listener on {b}"));
    assert!(TcpStream::connect(a).is_ok());
    assert!(TcpStream::connect(b).is_ok());
    unsafe {
        libc::kill(first.0.id() as i32, libc::SIGHUP);
    }
    wait_log(&a_log, &mut first, "config reloaded");
    assert!(TcpStream::connect(a).is_ok());
    assert!(first.0.try_wait().unwrap().is_none());
    assert!(second.0.try_wait().unwrap().is_none());
}

#[test]
fn configured_address_is_used_without_override() {
    let dir = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let config = write_config(dir.path(), Some(addr));
    let log = dir.path().join("sidecar.log");
    let mut process = spawn(&config, None, &log);
    wait_log(&log, &mut process, &format!("HTTP listener on {addr}"));
    assert!(TcpStream::connect(addr).is_ok());
}

#[test]
fn port_in_use_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(dir.path(), Some(occupied.local_addr().unwrap()));
    let log = dir.path().join("sidecar.log");
    let mut process = spawn(&config, None, &log);
    assert!(!wait_exit(&mut process).success());
    assert!(fs::read_to_string(&log).unwrap().contains("failed to bind"));
}

#[test]
fn dynamic_ports_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for (configured, listen) in [
        ("127.0.0.1:0".parse().unwrap(), None),
        (free_addr(), Some("127.0.0.1:0")),
    ] {
        let config = write_config(dir.path(), Some(configured));
        let log = dir.path().join("sidecar.log");
        let mut process = spawn(&config, listen, &log);
        assert!(!wait_exit(&mut process).success());
        assert!(fs::read_to_string(&log).unwrap().contains("fixed port"));
    }
}
