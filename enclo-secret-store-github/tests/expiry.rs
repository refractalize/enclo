use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};
fn response(scenario: &str) -> serde_json::Value {
    let root = tempfile::tempdir().unwrap();
    let gh = root.path().join("gh");
    fs::write(&gh,r#"#!/usr/bin/python3
import sys,os,json
case=os.environ['SCENARIO']
if sys.argv[1:]==['auth','token']:print('original-token');sys.exit()
assert sys.argv[1:]==['auth','status','--active','--hostname','github.example','--json','hosts','--show-token']
if case=='unavailable':sys.exit(1)
entry={'active':True,'state':'success','token':'original-token'}
if case!='legacy':entry['tokenExpiresAt']='bad' if case=='malformed' else '2030-01-01T00:00:00Z'
if case=='rotated':entry.update(token='rotated-token',refreshable=True)
print(json.dumps({'hosts':{'github.example':[entry]}}))
"#).unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_enclo-secret-store-github"))
        .env("PATH", root.path())
        .env("GH_HOST", "github.example")
        .env("SCENARIO", scenario)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        r#"{{"requestId":1,"name":"GITHUB_TOKEN"}}"#
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn metadata_tracks_the_selected_token_and_older_cli_versions_keep_working() {
    for scenario in ["expiry", "rotated", "legacy", "unavailable"] {
        let r = response(scenario);
        assert_eq!(r["status"], "ok");
        assert_eq!(
            r["value"],
            if scenario == "rotated" {
                "rotated-token"
            } else {
                "original-token"
            }
        );
        assert_eq!(
            r.get("expiresAt").is_some(),
            matches!(scenario, "expiry" | "rotated")
        );
    }
    assert_eq!(response("malformed")["status"], "error");
}
