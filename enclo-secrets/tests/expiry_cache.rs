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
fn backend_expiry_overrides_long_cache_limits_and_legacy_responses_still_cache() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let backend = bin.join("enclo-secret-store-test");
    fs::write(&backend,r#"#!/usr/bin/python3
import sys,json,datetime
counts={}
for line in sys.stdin:
 r=json.loads(line);name=r['name'];counts[name]=counts.get(name,0)+1
 out={'requestId':r['requestId'],'status':'ok','value':str(counts[name])}
 if name!='LEGACY':
  seconds=-1 if name=='EXPIRED' else 62 if counts[name]==1 else 3600
  out['expiresAt']=(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(seconds=seconds)).isoformat()
 print(json.dumps(out),flush=True)
"#).unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = root.path().join("secrets.sock");
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        format!(
            r#"
[store.cache]
idle_ttl="1h"
max_ttl="1h"
[[store.routes]]
patterns=["*"]
backend="test"
[[store.backends]]
name="test"
[secrets.socket]
path={:?}
[secrets.access]
mode="allow-all"
"#,
            socket.to_str().unwrap()
        ),
    )
    .unwrap();
    let mut child = Process(
        Command::new(env!("CARGO_BIN_EXE_enclo-secrets"))
            .args(["serve", "--config"])
            .arg(&config)
            .env("PATH", &bin)
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
    let first = request(&socket, "TOKEN");
    assert_eq!(first["value"], "1");
    assert!(first.get("expiresAt").is_some());
    let hit = request(&socket, "TOKEN");
    assert_eq!(hit, first);
    std::thread::sleep(Duration::from_millis(2100));
    let refreshed = request(&socket, "TOKEN");
    assert_eq!(refreshed["value"], "2");
    assert_ne!(refreshed["expiresAt"], first["expiresAt"]);
    assert_eq!(request(&socket, "TOKEN"), refreshed);
    assert_eq!(request(&socket, "LEGACY")["value"], "1");
    let legacy = request(&socket, "LEGACY");
    assert_eq!(legacy["value"], "1");
    assert!(legacy.get("expiresAt").is_none());
    assert_eq!(request(&socket, "EXPIRED")["status"], "notFound");
}
