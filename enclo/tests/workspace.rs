use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "enclo-workspace-test-{}-{}",
            std::process::id(),
            rand_core::OsRng.next_u64()
        ));
        let workspace = root.join("repo");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(root.join("config/enclo/hosts")).unwrap();
        fs::write(
            root.join("config/enclo/hosts/default.toml"),
            "[container]\nimage='test'\nvolumes=['home:%h']\n[modules]\n",
        )
        .unwrap();
        fs::write(
            workspace.join(".enclo.toml"),
            "[container]\nfrom='default'\n",
        )
        .unwrap();
        Self { root, workspace }
    }
    fn command(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_enclo"))
            .current_dir(cwd)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .args(args)
            .output()
            .unwrap()
    }
    fn approve(&self) {
        let dir = self.root.join("state/enclo/trust");
        fs::create_dir_all(&dir).unwrap();
        let root = self.workspace.canonicalize().unwrap();
        let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let digest = format!(
            "{:x}",
            Sha256::digest(fs::read(self.workspace.join(".enclo.toml")).unwrap())
        );
        fs::write(dir.join(key), digest).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
use rand_core::RngCore;

#[test]
fn discovery_trust_changes_and_revocation() {
    let f = Fixture::new();
    let cwd = f.workspace.join("src");
    let run = || f.command(&cwd, &["setup-volume", "--dry-run"]);
    let out = run();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("untrusted workspace"));
    f.approve();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Copying approved bytes into a different directory does not transfer trust.
    let other = f.root.join("other");
    fs::create_dir(&other).unwrap();
    fs::copy(f.workspace.join(".enclo.toml"), other.join(".enclo.toml")).unwrap();
    assert!(!f
        .command(&other, &["setup-volume", "--dry-run"])
        .status
        .success());
    // The nearest configuration shadows the parent and unknown fields are rejected.
    fs::write(
        cwd.join(".enclo.toml"),
        "[container]\nfrom='default'\nimage='evil'\n",
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&run().stderr).contains("unknown field"));
    fs::remove_file(cwd.join(".enclo.toml")).unwrap();
    fs::write(
        f.workspace.join(".enclo.toml"),
        "[container]\nfrom='default'\ndirectory='/workspace'\n",
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&run().stderr).contains("untrusted workspace"));
    f.approve();
    assert!(run().status.success());
    assert!(f.command(&cwd, &["untrust"]).status.success());
    assert!(!run().status.success());
    // Explicit central selection bypasses workspace discovery and approval.
    assert!(f
        .command(&cwd, &["setup-volume", "-C", "default", "--dry-run"])
        .status
        .success());
}

#[test]
fn rejects_colliding_mounts() {
    let f = Fixture::new();
    fs::write(
        f.workspace.join(".enclo.toml"),
        "[container]\nfrom='default'\ndirectory='/usr/local'\n",
    )
    .unwrap();
    f.approve();
    let out = f.command(&f.workspace, &["setup-volume", "--dry-run"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("protected path"));
}

#[test]
fn explicit_directory_is_strict_and_implicit_selection_is_visible() {
    let f = Fixture::new();
    let outside = f.root.join("outside");
    fs::create_dir(&outside).unwrap();
    // Force a missing workspace rather than depending on ancestors of the test temp dir.
    let out = f.command(&outside, &["setup-volume", "-d", ".", "--dry-run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no .enclo.toml found"));
    let out = f.command(&outside, &["setup-volume", "--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr)
        .contains("No workspace configuration found; using central container: default"));
    f.approve();
    let out = f.command(&f.workspace, &["setup-volume", "--dry-run"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Using workspace"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("profile: default"));
    // Explicit workspace selection works from elsewhere and searches parents.
    let directory = f.workspace.join("src");
    let out = f.command(
        &outside,
        &[
            "setup-volume",
            "-d",
            directory.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("Using workspace"));
    // A discovered bad file always fails, even though a usable central default exists.
    fs::write(f.workspace.join(".enclo.toml"), "invalid").unwrap();
    let out = f.command(&f.workspace, &["setup-volume", "--dry-run"]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("using central container"));
    assert!(f
        .command(
            &f.workspace,
            &["setup-volume", "-C", "default", "--dry-run"]
        )
        .status
        .success());
    // Explicit config remains available as -c and bypasses discovery too.
    let config = f.root.join("config/enclo/hosts/default.toml");
    assert!(f
        .command(
            &f.workspace,
            &["setup-volume", "-c", config.to_str().unwrap(), "--dry-run"]
        )
        .status
        .success());
}

#[test]
fn untrust_supports_directory_and_rejects_other_selectors() {
    let f = Fixture::new();
    f.approve();
    let out = f.command(&f.root, &["untrust", "-d", f.workspace.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(!f
        .command(&f.workspace, &["setup-volume", "--dry-run"])
        .status
        .success());
    assert!(!f
        .command(&f.workspace, &["untrust", "-C", "default"])
        .status
        .success());
    assert!(!f
        .command(&f.workspace, &["serve", "-d", "."])
        .status
        .success());
}

#[test]
fn mixed_selectors_across_command_levels_fail_before_resolution() {
    let f = Fixture::new();
    for args in [
        vec!["-C", "default", "setup-volume", "-d", ".", "--dry-run"],
        vec!["-d", ".", "home", "init", "-c", "missing.toml"],
    ] {
        let out = f.command(&f.workspace, &args);
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("mutually exclusive") || error.contains("cannot be used with"),
            "{error}"
        );
        assert!(!error.contains("untrusted"));
    }
}
