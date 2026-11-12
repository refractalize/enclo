//! Opt-in real OpenSSH integration test. Starts only temporary user-owned sshd,
//! keys, forwards and HTTP fixtures; never uses systemd, D-Bus or installation.
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    children: Vec<Child>,
    http_stop: Arc<AtomicBool>,
    http: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for child in self.children.iter_mut().rev() {
            if child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(8);
                while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        self.http_stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.http.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "fixture readiness timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "essh-{}-{}",
            std::process::id(),
            rand_core::RngCore::next_u32(&mut rand_core::OsRng)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            children: vec![],
            http_stop: Arc::new(AtomicBool::new(false)),
            http: None,
        }
    }
    fn cli(&self, action: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo"));
        cmd.arg(action)
            .arg("--profile")
            .arg(self.root.join("profile.toml"))
            .arg("--runtime-dir")
            .arg(&self.root);
        cmd
    }
    fn named_cli(&self, action: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo"));
        cmd.arg(action)
            .arg("--runtime-dir")
            .arg(&self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"));
        cmd
    }
    fn services_run(&self) -> Command {
        let mut cmd = self.cli("services");
        cmd.arg("run");
        cmd
    }
    fn shell(&self, args: &[&str]) -> std::process::Output {
        self.named_cli("session")
            .arg("profile")
            .arg("--")
            .args(args)
            .output()
            .unwrap()
    }
    fn log(&self, name: &str) -> Stdio {
        Stdio::from(fs::File::create(self.root.join(name)).unwrap())
    }
    fn start_sshd(&mut self) {
        for name in ["host", "client"] {
            assert!(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(self.root.join(name))
                .status()
                .unwrap()
                .success());
        }
        let port = unused_port();
        fs::write(self.root.join("sshd_config"), format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/host\nPidFile {root}/sshd.pid\nAuthorizedKeysFile {root}/client.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowTcpForwarding yes\nAllowStreamLocalForwarding yes\nGatewayPorts no\n", root=self.root.display())).unwrap();
        let host_key = fs::read_to_string(self.root.join("host.pub")).unwrap();
        fs::write(
            self.root.join("known_hosts"),
            format!("[127.0.0.1]:{port} {host_key}"),
        )
        .unwrap();
        let user =
            String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        fs::write(self.root.join("ssh_config"), format!("Host fixture\n HostName 127.0.0.1\n Port {port}\n User {}\n IdentityFile {root}/client\n IdentitiesOnly yes\n UserKnownHostsFile {root}/known_hosts\n GlobalKnownHostsFile /dev/null\n StrictHostKeyChecking yes\n", user.trim(), root=self.root.display())).unwrap();
        let sshd = std::env::var_os("ENCLO_TEST_SSHD")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/sbin/sshd"));
        let child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(self.root.join("sshd_config"))
            .stderr(self.log("sshd.log"))
            .spawn()
            .unwrap();
        self.children.push(child);
        wait_for(|| {
            if let Some(status) = self.children[0].try_wait().unwrap() {
                panic!(
                    "sshd exited {status}: {}",
                    fs::read_to_string(self.root.join("sshd.log")).unwrap()
                );
            }
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
    }
    fn start_http(&mut self) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = self.http_stop.clone();
        let request_log = self.root.join("http-requests");
        self.http = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = [0u8; 1024];
                    let length = stream.read(&mut request).unwrap_or(0);
                    if length > 0 {
                        let mut log = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&request_log)
                            .unwrap();
                        log.write_all(&request[..length]).unwrap();
                        let _ =
                            stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello");
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }));
        port
    }
    fn ready(&self) -> bool {
        self.root.join("connections/profile/ready").is_file()
    }
}
fn http_get(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
#[ignore = "requires local OpenSSH server/client and permission to bind fixture sockets"]
fn forwards_outlive_shells_and_disconnect_leaves_machine_running() {
    let mut f = Fixture::new();
    f.start_sshd();
    let http = f.start_http();
    let remote = unused_port();
    let local = unused_port();
    fs::write(f.root.join("profile.toml"), format!("[target]\nssh='fixture'\nconfig='ssh_config'\n[[forwards]]\ndirection='remote'\nspec='127.0.0.1:{remote}:127.0.0.1:{http}'\n[[forwards]]\ndirection='local'\nspec='127.0.0.1:{local}:127.0.0.1:{http}'\n")).unwrap();
    let central = f.root.join("config/enclo/connections");
    fs::create_dir_all(&central).unwrap();
    std::os::unix::fs::symlink(f.root.join("profile.toml"), central.join("profile.toml")).unwrap();
    std::os::unix::fs::symlink(f.root.join("ssh_config"), central.join("ssh_config")).unwrap();
    let status = f.cli("services").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["supervisor_running"], false);
    let child = f
        .named_cli("services")
        .args(["run", "profile"])
        .stderr(f.log("connect.log"))
        .spawn()
        .unwrap();
    f.children.push(child);
    wait_for(|| {
        if let Some(status) = f.children[1].try_wait().unwrap() {
            panic!(
                "connect exited {status}: {}",
                fs::read_to_string(f.root.join("connect.log")).unwrap()
            );
        }
        f.ready()
    });
    let all_services = f.named_cli("services").arg("list").output().unwrap();
    assert!(
        all_services.status.success(),
        "{}",
        String::from_utf8_lossy(&all_services.stderr)
    );
    let all_services: serde_json::Value = serde_json::from_slice(&all_services.stdout).unwrap();
    assert_eq!(all_services.as_array().unwrap().len(), 1);
    assert_eq!(all_services[0]["connection"], "profile");
    let duplicate = f.services_run().output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already have"));
    for _ in 0..2 {
        let output = f.shell(&["printf", "%s", "literal ' ; $(false)"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"literal ' ; $(false)");
        assert!(http_get(remote).ends_with("hello"));
        assert!(http_get(local).ends_with("hello"));
    }
    // Unexpected transport loss leaves the supervisor available for recovery.
    assert!(Command::new("ssh")
        .args(["-F", "none", "-S"])
        .arg(f.root.join("connections/profile/ssh-control"))
        .args(["-O", "exit", "fixture"])
        .output()
        .unwrap()
        .status
        .success());
    wait_for(|| !f.ready());
    let status = f.cli("services").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["supervisor_running"], true);
    assert_eq!(status["transport_ready"], false);
    assert!(status["last_error"].is_string());
    assert!(f.children[1].try_wait().unwrap().is_none());
    assert!(f.shell(&["true"]).status.success());
    assert!(http_get(remote).ends_with("hello"));
    assert_eq!(f.shell(&["sh", "-c", "exit 23"]).status.code(), Some(23));
    for index in 0..2 {
        let marker = f.root.join(format!("session-{index}"));
        let script = format!("touch '{}'; sleep 20", marker.display());
        let active = f
            .cli("session")
            .args(["--", "sh", "-c", &script])
            .stderr(f.log(&format!("shell-{index}.log")))
            .spawn()
            .unwrap();
        f.children.push(active);
        wait_for(|| marker.exists());
    }
    // Reload reconciles forwards while independent shell connections remain live.
    let profile_path = f.root.join("profile.toml");
    let original = fs::read_to_string(&profile_path).unwrap();
    let extra = unused_port();
    let desired = format!(
        "{original}\n[[forwards]]\ndirection='local'\nspec='127.0.0.1:{extra}:127.0.0.1:{http}'\n"
    );
    fs::write(&profile_path, &desired).unwrap();
    let reload = f.cli("services").arg("reload").output().unwrap();
    assert!(
        reload.status.success(),
        "{}",
        String::from_utf8_lossy(&reload.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&reload.stdout).unwrap();
    assert_eq!(status["transport_ready"], true);
    assert_eq!(status["forwards"].as_array().unwrap().len(), 3);
    assert!(http_get(extra).ends_with("hello"));
    fs::write(&profile_path, "invalid = [").unwrap();
    assert!(!f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    assert!(http_get(extra).ends_with("hello"));
    let status = f.cli("services").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["supervisor_running"], true);
    assert_eq!(status["transport_ready"], true);
    assert!(status["last_error"].is_string());
    // Failed application restores the last working configuration.
    let occupied_reload = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied_reload.local_addr().unwrap().port();
    fs::write(&profile_path, format!("{desired}\n[[forwards]]\ndirection='local'\nspec='127.0.0.1:{port}:127.0.0.1:{http}'\n")).unwrap();
    let failed = f.cli("services").arg("reload").output().unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("previous configuration restored"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(http_get(extra).ends_with("hello"));
    fs::write(&profile_path, &original).unwrap();
    assert!(f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    assert!(TcpStream::connect(("127.0.0.1", extra)).is_err());
    for index in 2..4 {
        assert!(f.children[index].try_wait().unwrap().is_none());
    }
    assert!(f
        .cli("services")
        .arg("stop")
        .output()
        .unwrap()
        .status
        .success());
    wait_for(|| f.children[1].try_wait().unwrap().is_some());
    std::thread::sleep(Duration::from_millis(800));
    for index in 2..4 {
        assert!(f.children[index].try_wait().unwrap().is_none());
    }
    assert!(!f.ready());
    assert!(TcpStream::connect(("127.0.0.1", remote)).is_err());
    assert!(TcpStream::connect(("127.0.0.1", local)).is_err());
    assert!(f.children[0].try_wait().unwrap().is_none());
    let list = f.cli("sessions").arg("list").output().unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let sessions: Vec<enclo::session_registry::SessionInfo> =
        serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(sessions.len(), 2);
    let all_sessions = f.named_cli("sessions").arg("list").output().unwrap();
    assert!(all_sessions.status.success());
    let all_sessions: serde_json::Value = serde_json::from_slice(&all_sessions.stdout).unwrap();
    assert_eq!(all_sessions.as_array().unwrap().len(), 2);
    assert_eq!(all_sessions[0]["connection"], "profile");

    // The registry remains discoverable after a profile edit and service restart.
    let profile = f.root.join("profile.toml");
    fs::write(
        &profile,
        format!("{}\n# edited\n", fs::read_to_string(&profile).unwrap()),
    )
    .unwrap();
    let restarted = f
        .services_run()
        .stderr(f.log("restarted.log"))
        .spawn()
        .unwrap();
    f.children.push(restarted);
    wait_for(|| f.ready());
    for index in 2..4 {
        assert!(f.children[index].try_wait().unwrap().is_none());
    }
    assert!(http_get(remote).ends_with("hello"));
    // Explicit termination must also work with no service supervisor running.
    assert!(f
        .cli("services")
        .arg("stop")
        .output()
        .unwrap()
        .status
        .success());
    wait_for(|| f.children[4].try_wait().unwrap().is_some());
    let stopped = f
        .named_cli("sessions")
        .args(["stop", &sessions[0].id])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let list = f.cli("sessions").arg("list").output().unwrap();
    let remaining: Vec<enclo::session_registry::SessionInfo> =
        serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, sessions[1].id);
    assert!(TcpStream::connect(("127.0.0.1", remote)).is_err());
    assert!(f
        .cli("sessions")
        .args(["stop", "--all"])
        .status()
        .unwrap()
        .success());
    for index in 2..4 {
        wait_for(|| f.children[index].try_wait().unwrap().is_some());
        assert_eq!(f.children[index].wait().unwrap().code(), Some(143));
    }
    assert_eq!(
        f.cli("sessions").arg("list").output().unwrap().stdout,
        b"[]\n"
    );
    // Dead socket files are ignored and can never cause PID-based signalling.
    let registry_dir = f.root.join("connections/profile/sessions");
    let stale = registry_dir.join("00000000000000000000000000000000");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert_eq!(
        f.cli("sessions").arg("list").output().unwrap().stdout,
        b"[]\n"
    );
    assert!(!f
        .cli("sessions")
        .args(["stop", "00000000000000000000000000000000"])
        .output()
        .unwrap()
        .status
        .success());
    // A failed later forward must roll back earlier successful forwards and
    // never publish services as ready.
    let occupied = TcpListener::bind(("127.0.0.1", remote)).unwrap();
    let broken = format!("[target]\nssh='fixture'\nconfig='ssh_config'\n[[forwards]]\ndirection='local'\nspec='127.0.0.1:{local}:127.0.0.1:{http}'\n[[forwards]]\ndirection='remote'\nspec='127.0.0.1:{remote}:127.0.0.1:{http}'\n");
    fs::write(f.root.join("profile.toml"), broken).unwrap();
    let failed = f.services_run().output().unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("failed to establish forward"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(!f.ready());
    assert!(TcpStream::connect(("127.0.0.1", local)).is_err());
    drop(occupied);

    // Unknown host keys must not be silently accepted or persisted.
    fs::write(f.root.join("known_hosts"), "").unwrap();
    let untrusted = f.services_run().output().unwrap();
    assert!(!untrusted.status.success());
    assert!(String::from_utf8_lossy(&untrusted.stderr).contains("Host key verification failed"));
    assert!(fs::read(f.root.join("known_hosts")).unwrap().is_empty());
    assert!(f.children[0].try_wait().unwrap().is_none());
}

#[test]
#[ignore = "requires local sshd, curl and built enclo-secrets/enclo-container-client binaries"]
fn supervised_secrets_and_proxy_preserve_policies_and_shell_lifetimes() {
    let mut f = Fixture::new();
    f.start_sshd();
    let http = f.start_http();
    let port = unused_port();
    let binaries = PathBuf::from(env!("CARGO_BIN_EXE_enclo"))
        .parent()
        .unwrap()
        .to_owned();
    let service = binaries.join("enclo-secrets");
    let client = binaries.join("enclo-container-client");
    assert!(
        service.is_file() && client.is_file(),
        "build enclo-secrets and enclo-container-client first"
    );
    assert!(Command::new(&service)
        .args(["gen-ca", "--output-dir"])
        .arg(f.root.join("certs"))
        .output()
        .unwrap()
        .status
        .success());
    let bin = f.root.join("bin");
    fs::create_dir(&bin).unwrap();
    let backend = bin.join("enclo-secret-store-fixture");
    fs::write(&backend, format!(r#"#!/usr/bin/python3
import sys,json,os
from pathlib import Path
Path({pid:?}).write_text(str(os.getpid()))
for line in sys.stdin:
    request=json.loads(line)
    with open({requests:?},'a') as log: log.write(request['name']+'\n')
    print(json.dumps({{'requestId':request['requestId'],'status':'ok','value':'synthetic-fixture-value'}}),flush=True)
"#,pid=f.root.join("backend.pid").to_str().unwrap(), requests=f.root.join("requests").to_str().unwrap())).unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o700)).unwrap();
    let policy = r#"
[store]
[[store.backends]]
name = "fixture"
[[store.routes]]
patterns = ["*"]
backend = "fixture"
[store.confirm]
enabled = false
[secrets.access]
mode = "deny-all"
allow = ["ALLOWED"]
[proxy]
enabled = true
[proxy.tls]
ca_cert = "certs/ca.crt"
ca_key = "certs/ca.key"
[[proxy.rules]]
domain = "127.0.0.1"
[proxy.rules.auth]
type = "bearer"
secret = "PROXY_ONLY"
"#;
    fs::write(f.root.join("secrets.toml"), policy).unwrap();
    let profile = format!("[target]\nssh='fixture'\nconfig='ssh_config'\n[services.secrets]\nconfig='secrets.toml'\nclient_binary='{}'\nremote_proxy_port={port}\n", client.display());
    fs::write(f.root.join("profile.toml"), &profile).unwrap();
    // Relative runtime roots must still work when the credential process uses
    // the policy directory as its working directory.
    let mut command = Command::new(env!("CARGO_BIN_EXE_enclo"));
    command
        .args(["services", "run", "--profile"])
        .arg(f.root.join("profile.toml"))
        .args(["--runtime-dir", "."])
        .current_dir(&f.root);
    command
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stderr(f.log("secrets-services.log"));
    f.children.push(command.spawn().unwrap());
    wait_for(|| {
        if let Some(status) = f.children[1].try_wait().unwrap() {
            panic!(
                "services exited {status}: {}",
                fs::read_to_string(f.root.join("secrets-services.log")).unwrap()
            );
        }
        f.ready()
    });
    let run = |f: &Fixture, args: &[&str]| f.cli("session").arg("--").args(args).output().unwrap();
    let allowed = run(&f, &["enclo", "get", "ALLOWED"]);
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&allowed.stdout).unwrap()["ALLOWED"],
        "synthetic-fixture-value"
    );
    let denied = run(&f, &["enclo", "get", "PROXY_ONLY"]);
    assert!(!denied.status.success());
    assert!(!fs::read_to_string(f.root.join("requests"))
        .unwrap()
        .contains("PROXY_ONLY"));
    let proxy_args = [
        "enclo",
        "proxy",
        "--",
        "curl",
        "--silent",
        "--show-error",
        "--fail",
        "--max-time",
        "5",
        "--noproxy",
        "",
        &format!("http://127.0.0.1:{http}/"),
    ];
    let proxied = run(&f, &proxy_args);
    assert!(
        proxied.status.success(),
        "{}",
        String::from_utf8_lossy(&proxied.stderr)
    );
    assert_eq!(proxied.stdout, b"hello");
    let request = fs::read_to_string(f.root.join("http-requests"))
        .unwrap()
        .to_lowercase();
    assert!(
        request.contains("authorization: bearer synthetic-fixture-value"),
        "{request}"
    );
    assert!(!request.contains("proxy-authorization:"));
    let unauthenticated = run(
        &f,
        &[
            "curl",
            "--silent",
            "--max-time",
            "5",
            "--noproxy",
            "",
            "--proxy",
            &format!("http://127.0.0.1:{port}"),
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            &format!("http://127.0.0.1:{http}/"),
        ],
    );
    assert_eq!(unauthenticated.stdout, b"407");

    assert!(fs::read_to_string(f.root.join("requests"))
        .unwrap()
        .contains("PROXY_ONLY"));
    let status = f.cli("services").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["secrets"]["running"], true);
    let remote = PathBuf::from(status["secrets"]["remote_directory"].as_str().unwrap());
    assert_eq!(
        fs::metadata(&remote).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!remote.join("ca.key").exists());
    let environment = fs::read_to_string(remote.join("env.sh")).unwrap();
    let serialized = status.to_string();
    assert!(!serialized.contains("http://enclo:"));
    assert!(environment.contains("ENCLO_PROXY="));
    assert!(!environment.contains("export HTTP_PROXY="));
    // Parse/TLS validation fails before disturbing the working service.
    fs::write(f.root.join("secrets.toml"), "[broken").unwrap();
    assert!(!f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    assert!(run(&f, &["enclo", "get", "ALLOWED"]).status.success());
    fs::write(f.root.join("secrets.toml"), policy).unwrap();
    // A valid profile whose forwarding fails rolls back both transport and policy.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    fs::write(
        f.root.join("profile.toml"),
        profile.replace(
            &format!("remote_proxy_port={port}"),
            &format!(
                "remote_proxy_port={}",
                occupied.local_addr().unwrap().port()
            ),
        ),
    )
    .unwrap();
    let failed = f.cli("services").arg("reload").output().unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("previous configuration restored"));
    fs::write(f.root.join("profile.toml"), &profile).unwrap();
    assert!(run(&f, &["enclo", "get", "ALLOWED"]).status.success());
    // Policy reload takes effect for new requests, while sessions remain independent.
    let marker = f.root.join("long-shell");
    f.children.push(
        f.cli("session")
            .args([
                "--",
                "sh",
                "-c",
                &format!("touch '{}'; sleep 30", marker.display()),
            ])
            .stderr(f.log("long-shell.log"))
            .spawn()
            .unwrap(),
    );
    wait_for(|| marker.exists());
    fs::write(
        f.root.join("secrets.toml"),
        policy.replace("allow = [\"ALLOWED\"]", "allow = []"),
    )
    .unwrap();
    assert!(f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    assert!(!run(&f, &["enclo", "get", "ALLOWED"]).status.success());
    assert!(f.children[2].try_wait().unwrap().is_none());
    // A dead credential process makes the whole services generation unready.
    let status = f.cli("services").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let pid = status["secrets"]["pid"].as_u64().unwrap() as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    wait_for(|| !f.ready());
    assert!(f.children[2].try_wait().unwrap().is_none());
    assert!(run(&f, &["true"]).status.success());
    assert!(f.ready());
    assert!(!run(&f, &["enclo", "get", "ALLOWED"]).status.success());
    // Both individual transports can be disabled without changing access policy.
    fs::write(
        f.root.join("secrets.toml"),
        policy.replace("[proxy]\nenabled = true", "[proxy]\nenabled = false"),
    )
    .unwrap();
    assert!(f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    assert!(run(&f, &["enclo", "get", "ALLOWED"]).status.success());
    assert!(run(&f, &["sh", "-c", "test -z \"${ENCLO_PROXY-}\""])
        .status
        .success());
    fs::write(
        f.root.join("secrets.toml"),
        format!("{policy}\n[secrets.socket]\nenabled=false\n"),
    )
    .unwrap();
    fs::write(
        f.root.join("profile.toml"),
        format!("{profile}proxy_mode='implicit'\n"),
    )
    .unwrap();
    assert!(f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    assert!(
        run(&f, &["sh", "-c", "test \"$HTTP_PROXY\" = \"$ENCLO_PROXY\""])
            .status
            .success()
    );
    assert!(!run(&f, &["enclo", "get", "ALLOWED"]).status.success());
    assert!(run(&f, &proxy_args).status.success());
    let backend_pid: i32 = fs::read_to_string(f.root.join("backend.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(f
        .cli("services")
        .arg("stop")
        .output()
        .unwrap()
        .status
        .success());
    assert!(f.children[2].try_wait().unwrap().is_none());
    wait_for(|| unsafe { libc::kill(backend_pid, 0) } == -1);
    assert!(f
        .cli("sessions")
        .args(["stop", "--all"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!f.ready());
    assert!(!f.root.join("connections/profile/services-env").exists());
}

#[test]
#[ignore = "requires temporary sshd and local sockets"]
fn background_start_reports_readiness_failure_and_preserves_sessions() {
    let mut f = Fixture::new();
    f.start_sshd();
    let http = f.start_http();
    let port = unused_port();
    let profile = format!("[target]\nssh='fixture'\nconfig='ssh_config'\n[[forwards]]\ndirection='local'\nspec='127.0.0.1:{port}:127.0.0.1:{http}'\n");
    fs::write(f.root.join("profile.toml"), &profile).unwrap();
    let central = f.root.join("config/enclo/connections");
    fs::create_dir_all(&central).unwrap();
    std::os::unix::fs::symlink(f.root.join("profile.toml"), central.join("profile.toml")).unwrap();
    std::os::unix::fs::symlink(f.root.join("ssh_config"), central.join("ssh_config")).unwrap();
    // Always stop the detached owner, even if an assertion panics.
    struct Stop(Command);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.output();
        }
    }
    let mut stop = f.cli("services");
    stop.arg("stop");
    let _stop = Stop(stop);
    let one = f
        .cli("session")
        .args(["--", "printf", "one"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let two = f
        .cli("session")
        .args(["--", "printf", "two"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for (child, expected) in [(one, "one"), (two, "two")] {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, expected.as_bytes());
    }
    let state = f.cli("services").arg("status").output().unwrap();
    let state: serde_json::Value = serde_json::from_slice(&state.stdout).unwrap();
    assert_eq!(state["transport_ready"], true);
    assert!(http_get(port).ends_with("hello"));
    assert_eq!(
        fs::metadata(f.root.join("connections/profile/supervisor.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(f.shell(&["true"]).status.success());
    let duplicate = f.cli("services").arg("start").output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already have"));
    assert!(f.shell(&["true"]).status.success());
    assert!(f
        .cli("services")
        .arg("reload")
        .output()
        .unwrap()
        .status
        .success());
    let child = f
        .cli("session")
        .args(["--", "sleep", "30"])
        .spawn()
        .unwrap();
    f.children.push(child);
    wait_for(|| {
        let result = f.cli("sessions").arg("list").output().unwrap();
        serde_json::from_slice::<serde_json::Value>(&result.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == 1
    });
    assert!(f
        .cli("services")
        .arg("stop")
        .output()
        .unwrap()
        .status
        .success());
    assert!(f.children.last_mut().unwrap().try_wait().unwrap().is_none());
    let mut failed = None;
    let occupied = TcpListener::bind(("127.0.0.1", port)).unwrap();
    wait_for(|| {
        let out = f.cli("services").arg("start").output().unwrap();
        if String::from_utf8_lossy(&out.stderr).contains("already have") {
            return false;
        }
        failed = Some(out);
        true
    });
    let failed = failed.unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("failed to establish forward"));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("supervisor.log"));
    assert!(!f.ready());
    let blocked = f
        .cli("session")
        .args(["--", "touch"])
        .arg(f.root.join("must-not-execute"))
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(!f.root.join("must-not-execute").exists());
    drop(occupied);
    assert!(f
        .cli("services")
        .arg("start")
        .output()
        .unwrap()
        .status
        .success());
    assert!(f.shell(&["true"]).status.success());
    assert!(f
        .cli("services")
        .arg("stop")
        .output()
        .unwrap()
        .status
        .success());
    assert!(f
        .cli("sessions")
        .args(["stop", "--all"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
#[ignore = "binds a temporary unresponsive SSH endpoint"]
fn background_start_timeout_cleans_up() {
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    fs::write(
        f.root.join("ssh_config"),
        format!(
            "Host fixture\n HostName 127.0.0.1\n Port {}\n",
            listener.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    fs::write(
        f.root.join("profile.toml"),
        "[target]\nssh='fixture'\nconfig='ssh_config'\n",
    )
    .unwrap();
    let now = Instant::now();
    let result = f
        .cli("services")
        .args(["start", "--timeout-secs", "1"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("timed out"));
    assert!(now.elapsed() < Duration::from_secs(15));
    assert!(!f.ready());
    let result = f.cli("services").arg("status").output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(status["supervisor_running"], false);
}
