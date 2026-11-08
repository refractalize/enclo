use rand_core::RngCore;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("enclo-auth-{:x}", rand_core::OsRng.next_u64()));
        for path in ["bin", "home/.codex", "data"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(
            root.join("home/.codex/auth.json"),
            "host login remains unchanged",
        )
        .unwrap();
        let fake = root.join("bin/codex");
        fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$RECORD/args"
printf '%s\n' "$CODEX_HOME" > "$RECORD/codex-home"
pwd > "$RECORD/cwd"
umask > "$RECORD/umask"
printf 'Device login instructions\n'
if [ "${FAKE_NO_AUTH:-}" = 1 ]; then exit 0; fi
# Simulate a CLI writing a credential file.
printf '{}\n' > "$CODEX_HOME/auth.json"
if [ "${FAKE_FAILURE:-}" = 1 ]; then exit 7; fi
"#,
        )
        .unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        Self(root)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo"));
        cmd.current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .env("CODEX_HOME", self.0.join("home/.codex"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("PATH", self.0.join("bin"))
            .env("RECORD", &self.0);
        cmd
    }
    fn run(&self) -> Output {
        self.command()
            .args(["auth", "login", "codex"])
            .output()
            .unwrap()
    }
    fn assert_host_unchanged(&self) {
        assert_eq!(
            fs::read_to_string(self.0.join("home/.codex/auth.json")).unwrap(),
            "host login remains unchanged"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn dedicated_device_login_uses_private_xdg_profile_and_preserves_host_login() {
    let f = Fixture::new();
    let out = f.run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let profile = f.0.join("data/enclo/codex");
    assert_eq!(
        fs::read_to_string(f.0.join("codex-home")).unwrap().trim(),
        profile.to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(f.0.join("cwd")).unwrap().trim(),
        profile.to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(f.0.join("args")).unwrap(),
        "-c\ncli_auth_credentials_store=\"file\"\nlogin\n--device-auth\n"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("umask"))
            .unwrap()
            .trim()
            .trim_start_matches('0'),
        "77"
    );
    assert_eq!(
        fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(profile.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Device login instructions"));
    f.assert_host_unchanged();
}

#[test]
fn default_profile_ignores_the_ordinary_codex_home_and_relative_xdg_path() {
    let f = Fixture::new();
    for xdg in [None, Some("relative-data")] {
        let mut cmd = f.command();
        cmd.env_remove("XDG_DATA_HOME");
        if let Some(xdg) = xdg {
            cmd.env("XDG_DATA_HOME", xdg);
        }
        assert!(cmd
            .args(["auth", "login", "codex"])
            .output()
            .unwrap()
            .status
            .success());
        assert!(f
            .0
            .join("home/.local/share/enclo/codex/auth.json")
            .is_file());
        f.assert_host_unchanged();
    }
}

#[test]
fn failed_device_login_is_reported_without_changing_normal_credentials() {
    let f = Fixture::new();
    let out = f
        .command()
        .env("FAKE_FAILURE", "1")
        .args(["auth", "login", "codex"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Codex device login failed"));
    assert_eq!(
        fs::metadata(f.0.join("data/enclo/codex/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    f.assert_host_unchanged();
}

#[test]
fn symlinked_profile_and_auth_file_are_rejected_before_launch() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("data/enclo")).unwrap();
    let profile = f.0.join("data/enclo/codex");
    symlink(f.0.join("home/.codex"), &profile).unwrap();
    assert!(!f.run().status.success());
    fs::remove_file(&profile).unwrap();
    fs::create_dir(&profile).unwrap();
    symlink(f.0.join("home/.codex/auth.json"), profile.join("auth.json")).unwrap();
    assert!(!f.run().status.success());
    assert!(!f.0.join("args").exists());
    f.assert_host_unchanged();
}

#[test]
fn selectors_and_unknown_providers_fail_without_starting_login() {
    let f = Fixture::new();
    for args in [
        vec!["auth", "login", "codex", "-C", "forbes"],
        vec!["-d", ".", "auth", "login", "codex"],
        vec!["auth", "login", "codex", "-c", "anything.toml"],
        vec!["auth", "login", "unknown"],
    ] {
        assert!(!f.command().args(args).output().unwrap().status.success());
    }
    assert!(!f.0.join("args").exists());
}

#[test]
fn missing_codex_reports_an_actionable_error() {
    let f = Fixture::new();
    fs::remove_file(f.0.join("bin/codex")).unwrap();
    let out = f.run();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Codex CLI is not installed"));
    assert!(!f.0.join("data/enclo/codex").exists());
}

#[test]
fn success_without_a_file_cache_is_reported_as_a_failure() {
    let f = Fixture::new();
    let out = f
        .command()
        .env("FAKE_NO_AUTH", "1")
        .args(["auth", "login", "codex"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("without a usable auth.json"));
    f.assert_host_unchanged();
}


#[test]
fn login_can_acquire_codex_native_auth_lock_while_enclo_serializes_profile() {
    let f = Fixture::new();
    fs::write(f.0.join("bin/codex"), r#"#!/usr/bin/python3
import fcntl, os
from pathlib import Path
home = Path(os.environ['CODEX_HOME'])
with open(home / 'auth.lock', 'a') as native_lock:
    fcntl.flock(native_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    with open(home / 'enclo-auth.lock', 'a') as enclo_lock:
        try:
            fcntl.flock(enclo_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            raise AssertionError('Enclo profile lock must remain held during login')
    (home / 'auth.json').write_text('{}')
"#).unwrap();
    let out = f.run();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    f.assert_host_unchanged();
}
