#![cfg(target_os = "linux")]
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_socket(path: &Path, child: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "{}",
            fs::read_to_string(path.with_extension("log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn get(path: &Path) -> serde_json::Value {
    let mut c = UnixStream::connect(path).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    writeln!(c, r#"{{"requestId":0,"name":"TOKEN"}}"#).unwrap();
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
#[test]
fn proxy_delegates_to_private_host_store_without_exposing_unrestricted_rpc() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let backend = bin.join("enclo-secret-store-fixture");
    let fetched = root.path().join("fetches");
    fs::write(&backend,r#"#!/usr/bin/python3
import sys,json,os
for line in sys.stdin:
 r=json.loads(line)
 with open(os.environ['FETCHES'],'a') as f:f.write('fetch\n')
 print(json.dumps({'requestId':r['requestId'],'status':'ok','value':os.environ['VALUE'],'expiresAt':'2099-01-01T00:00:00Z'}),flush=True)
"#).unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o755)).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key).unwrap();
    fs::write(root.path().join("ca.pem"), cert.pem()).unwrap();
    fs::write(root.path().join("ca.key"), key.serialize_pem()).unwrap();
    let config = root.path().join("config.toml");
    let private = root.path().join("host.sock");
    let public = root.path().join("public.sock");
    let text = format!(
        r#"
[store.cache]
idle_ttl="1h"
max_ttl="1h"
[[store.routes]]
patterns=["TOKEN"]
backend="fixture"
[[store.backends]]
name="fixture"
[store.backends.env]
VALUE="before"
[secrets.access]
mode="deny-all"
[proxy]
enabled=true
[proxy.tls]
ca_cert="ca.pem"
ca_key="ca.key"
[[proxy.rules]]
domain="127.0.0.1"
auth={{type="bearer",secret="TOKEN"}}
"#
    );
    fs::write(&config, &text).unwrap();
    let mut host = Process(
        Command::new(env!("CARGO_BIN_EXE_enclo-secrets"))
            .args(["serve", "--store-only", "--config"])
            .arg(&config)
            .arg("--socket")
            .arg(&private)
            .env("PATH", &bin)
            .env("FETCHES", &fetched)
            .stdout(fs::File::create(private.with_extension("log")).unwrap())
            .stderr(
                fs::OpenOptions::new()
                    .append(true)
                    .open(private.with_extension("log"))
                    .unwrap(),
            )
            .spawn()
            .unwrap(),
    );
    wait_socket(&private, &mut host);
    assert_eq!(
        fs::metadata(&private).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let fd = listener.as_raw_fd();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo-secrets"));
    cmd.args(["serve", "--config"])
        .arg(&config)
        .arg("--store-socket")
        .arg(&private)
        .arg("--socket")
        .arg(&public)
        .arg("--listen-fd")
        .arg(fd.to_string())
        .env("PATH", root.path().join("missing"));
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut proxy = Process(
        cmd.stdout(fs::File::create(public.with_extension("log")).unwrap())
            .stderr(
                fs::OpenOptions::new()
                    .append(true)
                    .open(public.with_extension("log"))
                    .unwrap(),
            )
            .spawn()
            .unwrap(),
    );
    drop(listener);
    wait_socket(&public, &mut proxy);
    assert_eq!(get(&public)["status"], "notFound");
    assert!(!fetched.exists());
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = upstream.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let (mut c, _) = upstream.accept().unwrap();
        let mut reader = BufReader::new(c.try_clone().unwrap());
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer before"));
        c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });
    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        client,
        "GET http://{target}/ HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"));
    worker.join().unwrap();
    assert_eq!(fs::read_to_string(&fetched).unwrap().lines().count(), 1);
    let cached = get(&private);
    assert_eq!(cached["value"], "before");
    assert!(cached.get("expiresAt").is_some());
    assert_eq!(fs::read_to_string(&fetched).unwrap().lines().count(), 1);
    fs::write(&config, text.replace("VALUE=\"before\"", "VALUE=\"after\"")).unwrap();
    unsafe {
        libc::kill(host.0.id() as i32, libc::SIGHUP);
        libc::kill(proxy.0.id() as i32, libc::SIGHUP);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if get(&private)["value"] == "after" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(get(&public)["status"], "notFound");
}
