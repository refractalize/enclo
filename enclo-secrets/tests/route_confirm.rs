#![cfg(target_os = "linux")]
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
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
fn request(socket: &std::path::Path, name: &str) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(stream, "{}", serde_json::json!({"requestId":0,"name":name})).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
#[test]
fn only_opted_out_routes_bypass_confirmation_even_after_cache_expiry_and_reload() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, source) in [
        (
            "enclo-secret-store-test",
            r#"#!/usr/bin/python3
import sys,json,os
for line in sys.stdin:
 r=json.loads(line)
 with open(os.environ['FETCH_LOG'],'a') as f:f.write(r['name']+'\n')
 print(json.dumps({'requestId':r['requestId'],'status':'ok','value':'dummy'}),flush=True)
"#,
        ),
        (
            "pinentry",
            r#"#!/usr/bin/python3
import sys,os
print('OK mock pinentry',flush=True)
for line in sys.stdin:
 if line.startswith('CONFIRM'):
  with open(os.environ['PROMPT_LOG'],'a') as f:f.write('prompt\n')
  print('ERR 83886179 cancelled',flush=True)
 elif line.startswith('BYE'):break
 else:print('OK',flush=True)
"#,
        ),
    ] {
        let path = bin.join(name);
        fs::write(&path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let socket = root.path().join("secrets.sock");
    let config = root.path().join("secrets.toml");
    let config_text = format!(
        r#"
[store.confirm]
enabled=true
debounce_ms=1
[[store.routes]]
patterns=["CODEX_CREDENTIALS","CODEX_ACCOUNT_ID"]
backend="test"
confirm=false
cache=true
idle_ttl="1ms"
max_ttl="1ms"
[[store.routes]]
patterns=["*"]
backend="test"
[[store.backends]]
name="test"
[secrets.socket]
path={:?}
[secrets.access]
mode="allow-all"
deny=["BLOCKED"]
"#,
        socket.to_str().unwrap()
    );
    fs::write(&config, &config_text).unwrap();
    let fetch_log = root.path().join("fetch.log");
    let prompt_log = root.path().join("prompt.log");
    let mut child = Process(
        Command::new(env!("CARGO_BIN_EXE_enclo-secrets"))
            .args(["serve", "--config"])
            .arg(&config)
            .env("PATH", &bin)
            .env("FETCH_LOG", &fetch_log)
            .env("PROMPT_LOG", &prompt_log)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..2 {
        for name in ["CODEX_CREDENTIALS", "CODEX_ACCOUNT_ID"] {
            assert_eq!(request(&socket, name)["status"], "ok");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!prompt_log.exists());
    assert_eq!(fs::read_to_string(&fetch_log).unwrap().lines().count(), 4);
    assert_eq!(request(&socket, "OTHER")["status"], "notFound");
    assert_eq!(fs::read_to_string(&prompt_log).unwrap().lines().count(), 1);
    assert_eq!(request(&socket, "BLOCKED")["status"], "notFound");
    assert!(!fs::read_to_string(&fetch_log).unwrap().contains("OTHER"));
    // Reload keeps the backend process, but must apply the new approval policy.
    fs::write(
        &config,
        config_text.replace("confirm=false", "confirm=true"),
    )
    .unwrap();
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGHUP);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if request(&socket, "CODEX_CREDENTIALS")["status"] == "notFound" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(&prompt_log).unwrap().lines().count(), 2);
}
