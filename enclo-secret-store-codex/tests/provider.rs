use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn jwt(exp: u64) -> String {
    format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"exp":exp}).to_string())
    )
}
struct Fixture {
    root: tempfile::TempDir,
    token: String,
}
impl Fixture {
    fn new(expired: bool) -> Self {
        let f = Self {
            root: tempfile::tempdir().unwrap(),
            token: jwt(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600),
        };
        fs::create_dir_all(f.home()).unwrap();
        fs::set_permissions(f.home(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(f.root.path().join("bin")).unwrap();
        let cache = json!({"tokens":{"access_token":if expired {jwt(1)} else {f.token.clone()},"account_id":"account-test","refresh_token":"NEVER_RETURN_REFRESH_TOKEN","id_token":"NEVER_RETURN_ID_TOKEN"}});
        fs::write(f.home().join("auth.json"), cache.to_string()).unwrap();
        fs::set_permissions(
            f.home().join("auth.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let script = f.root.path().join("bin/codex");
        fs::write(
            &script,
            r#"#!/usr/bin/python3
import os,sys,json,time,fcntl
from pathlib import Path
home=Path(os.environ['CODEX_HOME'])
native_lock=open(home/'auth.lock','a')
fcntl.flock(native_lock,fcntl.LOCK_EX | fcntl.LOCK_NB)
assert str(home)==os.getcwd()
assert sys.argv[1:]==['-c','cli_auth_credentials_store="file"','app-server','--stdio']
with open(home/'refresh-count','a') as f: f.write('refresh\n')
for line in sys.stdin:
    req=json.loads(line)
    if req['method']=='initialize':
        assert req['params']['clientInfo']['name']=='enclo_codex_auth'
        print(json.dumps({'id':1,'result':{}}),flush=True)
    if req['method']=='account/read':
        assert req['params']['refreshToken'] is True
        if os.environ.get('FAIL_REFRESH'):
            print(json.dumps({'id':2,'error':{'message':'NEVER_LEAK_SERVER_DETAIL'}}),flush=True)
            continue
        time.sleep(.1)
        data=json.loads((home/'auth.json').read_text())
        data['tokens']['access_token']=os.environ['TEST_ACCESS_TOKEN']
        (home/'auth.json').write_text(json.dumps(data))
        print(json.dumps({'id':2,'result':{'account':{'type':'chatgpt'}}}),flush=True)
"#,
        )
        .unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        f
    }
    fn home(&self) -> std::path::PathBuf {
        self.root.path().join("data/enclo/codex")
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_enclo-secret-store-codex"));
        c.env("XDG_DATA_HOME", self.root.path().join("data"))
            .env(
                "CODEX_HOME",
                self.root.path().join("ordinary-codex-profile"),
            )
            .env("PATH", self.root.path().join("bin"))
            .env("TEST_ACCESS_TOKEN", &self.token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
    fn start(&self, name: &str) -> std::process::Child {
        let mut c = self.command().spawn().unwrap();
        writeln!(
            c.stdin.take().unwrap(),
            "{}",
            json!({"requestId":1,"name":name})
        )
        .unwrap();
        c
    }
    fn result(&self) -> Value {
        let out = self.start("CODEX_CREDENTIALS").wait_with_output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn current_credentials_are_returned_without_launching_codex_or_exposing_refresh_token() {
    let f = Fixture::new(false);
    let r = f.result();
    assert_eq!(r["status"], "ok");
    let value: Value = serde_json::from_str(r["value"].as_str().unwrap()).unwrap();
    assert_eq!(
        value,
        json!({"access_token":f.token,"account_id":"account-test"})
    );
    assert!(!r.to_string().contains("NEVER_RETURN"));
    assert!(!f.home().join("refresh-count").exists());
}
#[test]
fn expired_credentials_are_refreshed_once_across_processes() {
    let f = Fixture::new(true);
    let a = f.start("CODEX_CREDENTIALS");
    let b = f.start("CODEX_CREDENTIALS");
    for c in [a, b] {
        let out = c.wait_with_output().unwrap();
        let r: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(r["status"], "ok", "{r}");
        let value: Value = serde_json::from_str(r["value"].as_str().unwrap()).unwrap();
        assert_eq!(value["access_token"], f.token);
    }
    assert_eq!(
        fs::read_to_string(f.home().join("refresh-count")).unwrap(),
        "refresh\n"
    );
}
#[test]
fn refresh_errors_are_reported_without_echoing_rpc_details() {
    let f = Fixture::new(true);
    let mut cmd = f.command();
    cmd.env("FAIL_REFRESH", "1");
    let mut c = cmd.spawn().unwrap();
    writeln!(
        c.stdin.take().unwrap(),
        "{}",
        json!({"requestId":1,"name":"CODEX_CREDENTIALS"})
    )
    .unwrap();
    let out = c.wait_with_output().unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["status"], "error");
    assert!(!r.to_string().contains("NEVER_LEAK"));
    assert!(r.to_string().contains("enclo auth login codex"));
}
#[test]
fn unrelated_names_are_not_found_and_public_credentials_are_rejected() {
    let f = Fixture::new(false);
    let out = f.start("OTHER").wait_with_output().unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["status"], "notFound");
    fs::set_permissions(
        f.home().join("auth.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(f.result()["status"], "error");
}
#[test]
fn missing_login_is_actionable_and_never_uses_the_normal_codex_profile() {
    let f = Fixture::new(false);
    fs::remove_file(f.home().join("auth.json")).unwrap();
    let r = f.result();
    assert_eq!(r["status"], "error");
    assert!(r.to_string().contains("enclo auth login codex"));
}

#[test]
fn account_identity_lookup_returns_no_tokens_and_does_not_refresh() {
    let f = Fixture::new(true);
    let out = f.start("CODEX_ACCOUNT_ID").wait_with_output().unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["status"], "ok");
    assert_eq!(r["value"], "account-test");
    assert!(!r.to_string().contains("TOKEN"));
    assert!(!f.home().join("refresh-count").exists());
}
