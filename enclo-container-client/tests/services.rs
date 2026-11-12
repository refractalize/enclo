use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};
fn command(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo-container-client"));
    cmd.env("HOME", home);
    for name in ["ENCLO_CONNECTION", "ENCLO_PROXY", "ENCLO_SECRETS_SOCKET"] {
        cmd.env_remove(name);
    }
    cmd
}
fn install(
    home: &std::path::Path,
    name: &str,
    generation: &std::path::Path,
) -> std::process::Output {
    let mut child = command(home)
        .args(["service-install", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let settings = serde_json::json!({"generation":generation,"env":{"ENCLO_PROXY":format!("http://fixture-{name}@127.0.0.1:18080")}});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(settings.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
#[test]
fn installed_client_discovers_selects_refreshes_and_withdraws_settings() {
    let home = tempfile::tempdir().unwrap();
    let a = tempfile::Builder::new()
        .prefix("enclo-svc.")
        .tempdir_in("/tmp")
        .unwrap();
    let b = tempfile::Builder::new()
        .prefix("enclo-svc.")
        .tempdir_in("/tmp")
        .unwrap();
    assert!(install(home.path(), "work", a.path()).status.success());
    let binary = home.path().join(".local/bin/enclo");
    assert!(binary.exists());
    let active = home
        .path()
        .join(".local/state/enclo/connections/work/active.json");
    assert_eq!(
        fs::metadata(&active).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let result = command(home.path())
        .args(["proxy", "--env"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("fixture-work"));
    assert!(install(home.path(), "other", b.path()).status.success());
    let ambiguous = command(home.path())
        .args(["proxy", "--env"])
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("multiple active"));
    let selected = command(home.path())
        .args(["--connection", "work", "proxy", "--env"])
        .output()
        .unwrap();
    assert!(selected.status.success());
    assert!(String::from_utf8_lossy(&selected.stdout).contains("fixture-work"));
    // An old owner cannot withdraw another generation's settings.
    assert!(command(home.path())
        .args(["service-remove", "work"])
        .arg(b.path())
        .status()
        .unwrap()
        .success());
    assert!(active.exists());
    assert!(command(home.path())
        .args(["service-remove", "work"])
        .arg(a.path())
        .status()
        .unwrap()
        .success());
    assert!(!active.exists());
    assert!(binary.exists());
    assert!(command(home.path())
        .args(["service-remove", "other"])
        .arg(b.path())
        .status()
        .unwrap()
        .success());
    let stopped = command(home.path())
        .args(["proxy", "--env"])
        .output()
        .unwrap();
    assert!(!stopped.status.success());
    assert!(String::from_utf8_lossy(&stopped.stderr).contains("no active enclo services"));
}
#[test]
fn installer_preserves_unmanaged_client_and_rejects_unsafe_settings() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".local/bin")).unwrap();
    let binary = home.path().join(".local/bin/enclo");
    fs::write(&binary, "user's executable").unwrap();
    let generation = tempfile::Builder::new()
        .prefix("enclo-svc.")
        .tempdir_in("/tmp")
        .unwrap();
    let rejected = install(home.path(), "work", generation.path());
    assert!(!rejected.status.success());
    assert_eq!(fs::read_to_string(binary).unwrap(), "user's executable");
    assert!(!install(home.path(), "../escape", generation.path())
        .status
        .success());
}
