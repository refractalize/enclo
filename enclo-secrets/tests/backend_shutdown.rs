#![cfg(target_os = "linux")]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
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
fn check_shutdown(with_proxy: bool) {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let backend = bin.join("enclo-secret-store-lifecycle");
    let pid_file = root.path().join("backend.pid");
    fs::write(
        &backend,
        r#"#!/usr/bin/python3
import os,sys,time
from pathlib import Path
Path(os.environ['BACKEND_PID']).write_text(str(os.getpid()))
for line in sys.stdin:
 Path(os.environ['STARTED']).touch()
 time.sleep(60)
"#,
    )
    .unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o755)).unwrap();
    let config = root.path().join("config.toml");
    let started = root.path().join("started");
    let listener = with_proxy.then(|| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let socket = root.path().join("secrets.sock");
    fs::write(
        &config,
        format!(
            r#"
[[store.backends]]
name="lifecycle"
[[store.routes]]
patterns=["WAIT"]
backend="lifecycle"
[secrets.access]
allow=["WAIT"]
[secrets.socket]
path={:?}
"#,
            socket.to_str().unwrap()
        ),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo-secrets"));
    cmd.args(["serve", "--config"])
        .arg(&config)
        .env("PATH", &bin)
        .env("BACKEND_PID", &pid_file)
        .env("STARTED", &started)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(listener) = &listener {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        fs::write(root.path().join("ca.pem"), cert.pem()).unwrap();
        fs::write(root.path().join("ca.key"), key.serialize_pem()).unwrap();
        use std::io::Write;
        writeln!(
            fs::OpenOptions::new().append(true).open(&config).unwrap(),
            "[proxy]\nenabled=true\n[proxy.tls]\nca_cert='ca.pem'\nca_key='ca.key'"
        )
        .unwrap();
        let fd = listener.as_raw_fd();
        cmd.arg("--listen-fd").arg(fd.to_string());
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = Process(cmd.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() || !pid_file.exists() {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let backend_pid = fs::read_to_string(pid_file).unwrap();
    let _idle = listener
        .as_ref()
        .map(|listener| std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap());
    let _request = if with_proxy {
        use std::io::Write;
        let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        writeln!(stream, r#"{{"requestId":0,"name":"WAIT"}}"#).unwrap();
        while !started.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(stream)
    } else {
        None
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    while std::path::Path::new(&format!("/proc/{backend_pid}")).exists() {
        assert!(
            Instant::now() < cleanup_deadline,
            "backend cleanup waited for proxy drain"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !std::path::Path::new(&format!("/proc/{backend_pid}")).exists(),
        "credential backend was not reaped"
    );
}

#[test]
fn graceful_service_shutdown_reaps_host_credential_backends() {
    check_shutdown(false);
}
#[test]
fn backend_cleanup_precedes_idle_proxy_drain() {
    check_shutdown(true);
}
