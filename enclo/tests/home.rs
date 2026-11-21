//! Exercise initialization and restart behavior through the CLI with a fake Podman transport.
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "enclo-home-test-{}-{}",
            std::process::id(),
            rand_core::OsRng.next_u64()
        ));
        for dir in [
            "home",
            "bin",
            "repo",
            "config/enclo/hosts",
            "state/enclo/trust",
            "runtime",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let fake = root.join("bin/podman");
        fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import os, sys, subprocess, time, json, signal
from pathlib import Path
args = sys.argv[1:]
home = Path(os.environ['HOME'])
with open(home / 'podman-log', 'a') as log:
    log.write(json.dumps(args) + '\n')
if args[:2] == ['network', 'create']:
    if (home / 'fail-network').exists():
        print('network creation denied', file=sys.stderr)
        sys.exit(1)
    sys.exit(0)
if args[0] == 'run':
    if '--entrypoint=sh' in args:
        # The real preparation helper mounts this volume and chowns its root.
        sys.exit(0)
    stop = home / 'stopped'
    stop.unlink(missing_ok=True)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if stop.exists() or (home / '.enclo-init/local').exists():
            time.sleep(0.1)
            sys.exit(0)
        time.sleep(0.02)
    sys.exit(1)
if args[0] == 'inspect':
    if '{{json .Mounts}}' in args:
        import hashlib
        name = 'enclo-home-' + hashlib.sha256(args[-1].encode()).hexdigest()[:32]
        print(json.dumps([{'Type': 'volume', 'Name': name, 'Destination': str(home)}]))
    else:
        print('running')
    sys.exit(0)
if args[0] == 'stop':
    pidfile = home / 'exec-pid'
    if pidfile.exists():
        try:
            os.killpg(int(pidfile.read_text()), signal.SIGKILL)
        except ProcessLookupError:
            pass
    (home / 'stopped').touch()
    sys.exit(0)
if args[0] == 'exec':
    idx = next(i for i, arg in enumerate(args) if arg == 'sh')
    env = dict(os.environ)
    for arg in args[1:idx]:
        if arg.startswith('--env='):
            key, value = arg[6:].split('=', 1)
            env[key] = value
        if arg.startswith('--env-file='):
            for line in Path(arg.split('=', 1)[1]).read_text().splitlines():
                if '=' in line:
                    key, value = line.split('=', 1)
                    env[key] = value
    child = subprocess.Popen(args[idx:], cwd=home, env=env, start_new_session=True)
    pidfile = home / 'exec-pid'
    pidfile.write_text(str(child.pid))
    code = child.wait()
    pidfile.unlink(missing_ok=True)
    sys.exit(code)
raise SystemExit('unexpected fake podman invocation: ' + str(args))
"#,
        )
        .unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            root.join("config/enclo/hosts/default.toml"),
            r#"
[container]
image = "test"
[container.home]
enabled = true
[container.home.init]
command = '''
printf 'central\n' >> "$HOME/order"
'''
[modules.container_client]
enabled = false
[modules.terminfo]
enabled = false
"#,
        )
        .unwrap();
        let f = Self { root };
        f.local("test -f \"$HOME/allow\"\nprintf 'local\\n' >> \"$HOME/order\"");
        f
    }
    fn local(&self, script: &str) {
        let bytes = format!("[container]\nfrom='default'\n[container.home.init]\ncommand_after='''\n{script}\n'''\n");
        fs::write(self.root.join("repo/.enclo.toml"), &bytes).unwrap();
        let path = self.root.join("repo").canonicalize().unwrap();
        fs::write(
            self.root.join("state/enclo/trust").join(format!(
                "{:x}",
                Sha256::digest(path.as_os_str().as_encoded_bytes())
            )),
            format!("{:x}", Sha256::digest(bytes.as_bytes())),
        )
        .unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_at(args, &self.root.join("repo"))
    }
    fn run_at(&self, args: &[&str], cwd: &std::path::Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_enclo"))
            .current_dir(cwd)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .args(args)
            .output()
            .unwrap()
    }
    fn mark_running(&self) {
        let root = self.root.join("repo").canonicalize().unwrap();
        let identity = format!(
            "enclo-workspace-{}",
            &format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()))[..32]
        );
        let runtime = self.root.join("runtime/enclo").join(identity);
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("exec-env"), "").unwrap();
        fs::write(runtime.join("home-ready"), "").unwrap();
        let local = fs::read(root.join(".enclo.toml")).unwrap();
        let central = fs::read(self.root.join("config/enclo/hosts/default.toml")).unwrap();
        let mut digest = Sha256::new();
        digest.update((local.len() as u64).to_le_bytes());
        digest.update(local);
        digest.update(central);
        fs::write(
            runtime.join("workspace-fingerprint"),
            format!("{:x}", digest.finalize()),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn initialization_checkpoints_retry_changes_and_explicit_rerun() {
    let f = Fixture::new();
    let out = f.run(&["run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("local home initialization failed"));
    assert_eq!(
        fs::read_to_string(f.root.join("home/order")).unwrap(),
        "central\n"
    );
    assert!(f.root.join("home/.enclo-init/central").exists());
    assert!(!f.root.join("home/.enclo-init/local").exists());
    fs::write(f.root.join("home/allow"), "").unwrap();
    let out = f.run(&["run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("home/order")).unwrap(),
        "central\nlocal\n"
    );
    assert!(f.run(&["run"]).status.success());
    assert_eq!(
        fs::read_to_string(f.root.join("home/order")).unwrap(),
        "central\nlocal\n"
    );
    f.local("printf 'changed\\n' >> \"$HOME/order\"");
    let out = f.run(&["run"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("script changed")
            || String::from_utf8_lossy(&out.stderr).contains("script changed")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("home/order")).unwrap(),
        "central\nlocal\n"
    );
    f.mark_running();
    let out = f.run(&["home", "init", "--rerun"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("home/order")).unwrap(),
        "central\nlocal\ncentral\nchanged\n"
    );
    let log = fs::read_to_string(f.root.join("home/podman-log")).unwrap();
    assert!(log.contains("/enclo-home:nocopy"));
    assert!(log.contains("--env=HOME="));
}

#[test]
fn local_script_cannot_enable_home_or_override_central_script() {
    let f = Fixture::new();
    fs::write(
        f.root.join("repo/.enclo.toml"),
        "[container]\nfrom='default'\n[container.home]\nenabled=true\n",
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&f.run(&["run"]).stderr).contains("unknown field"));
    fs::write(
        f.root.join("repo/.enclo.toml"),
        "[container]\nfrom='default'\n[container.home.init]\ncommand='evil'\n",
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&f.run(&["run"]).stderr).contains("unknown field"));
}

#[test]
fn central_failure_prevents_local_and_timeout_stops_container() {
    let f = Fixture::new();
    let path = f.root.join("config/enclo/hosts/default.toml");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        original.replace("printf 'central\\n'", "false\nprintf 'central\\n'"),
    )
    .unwrap();
    let out = f.run(&["run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("central home initialization failed"));
    assert!(!f.root.join("home/order").exists());
    assert!(!f.root.join("home/.enclo-init/central").exists());
    fs::write(
        &path,
        original
            .replace("command = '''", "timeout_secs = 1\ncommand = '''")
            .replace("printf 'central\\n'", "sleep 10\nprintf 'central\\n'"),
    )
    .unwrap();
    let out = f.run(&["run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out after 1 seconds"));
    assert!(!f.root.join("home/.enclo-init/central").exists());
    assert!(f.root.join("home/stopped").exists());
}

#[test]
fn explicit_directory_maps_workdir_from_another_workspace() {
    let f = Fixture::new();
    let subdir = f.root.join("repo/src");
    fs::create_dir(&subdir).unwrap();
    f.mark_running();
    let out = f.run_at(
        &[
            "exec",
            "-d",
            subdir.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "true",
        ],
        &f.root,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = fs::read_to_string(f.root.join("home/podman-log")).unwrap();
    assert!(log.contains("--workdir=/workspace/src"));
    fs::write(f.root.join("home/podman-log"), "").unwrap();
    let out = f.run_at(
        &[
            "exec",
            "-d",
            subdir.to_str().unwrap(),
            "-w",
            "/override",
            "--",
            "sh",
            "-c",
            "true",
        ],
        &f.root,
    );
    assert!(out.status.success());
    assert!(fs::read_to_string(f.root.join("home/podman-log"))
        .unwrap()
        .contains("--workdir=/override"));
}

#[test]
fn workspace_network_is_created_before_launch_and_reused() {
    let f = Fixture::new();
    f.local("printf 'local\\n' >> \"$HOME/order\"");
    let local = f.root.join("repo/.enclo.toml");
    let script = fs::read_to_string(&local)
        .unwrap()
        .replace("from='default'", "from='default'\nworkspace_network=true");
    fs::write(&local, &script).unwrap();
    let root = f.root.join("repo").canonicalize().unwrap();
    let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    fs::write(
        f.root.join("state/enclo/trust").join(key),
        format!("{:x}", Sha256::digest(script.as_bytes())),
    )
    .unwrap();
    fs::write(f.root.join("home/fail-network"), "").unwrap();
    let out = f.run(&["run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("network creation denied"));
    assert!(!f.root.join("home/order").exists());
    fs::remove_file(f.root.join("home/fail-network")).unwrap();
    for _ in 0..2 {
        let out = f.run(&["run"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let log = fs::read_to_string(f.root.join("home/podman-log")).unwrap();
    let calls: Vec<Vec<String>> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let creates: Vec<_> = calls
        .iter()
        .filter(|a| a.starts_with(&["network".into(), "create".into()]))
        .collect();
    assert_eq!(creates.len(), 3);
    let name = creates[0].last().unwrap();
    assert!(name.starts_with("enclo-network-"));
    assert!(creates
        .iter()
        .all(|a| a.last() == Some(name) && a.contains(&"--ignore".into())));
    for run in calls
        .iter()
        .filter(|a| a[0] == "run" && a.contains(&"--name".into()))
    {
        let network = run.iter().position(|a| a == "--network").unwrap();
        assert_eq!(&run[network + 1], name);
        let creation = calls
            .iter()
            .position(|a| a.starts_with(&["network".into(), "create".into()]))
            .unwrap();
        assert!(creation < calls.iter().position(|a| a == run).unwrap());
    }
}
