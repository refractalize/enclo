//! Exercise the full staged lifecycle without needing a running Podman service.
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "esn-{}-{:x}",
            std::process::id(),
            rand_core::OsRng.next_u64()
        ));
        for dir in ["bin", "home", "runtime", "config"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let f = Self(root);
        fs::copy(env!("CARGO_BIN_EXE_enclo"), f.0.join("bin/enclo")).unwrap();
        f.script(
            "podman",
            r#"#!/usr/bin/env python3
import os, sys, json, time
from pathlib import Path
root = Path(os.environ['FIXTURE'])
a = sys.argv[1:]
with open(root/'events', 'a') as log: log.write(json.dumps(a)+'\n')
if a[0] == 'info': print(os.environ['ROOTLESS']); sys.exit(0)
if a[:2] == ['network', 'create']: sys.exit(0)
if a[:2] == ['network', 'inspect']: print('127.0.0.2'); sys.exit(0)
if a[0] == 'create':
    if os.environ.get('FAIL') == 'create': print('create rejected', file=sys.stderr); sys.exit(1)
    # All mount sources must already exist, even before secrets activation.
    for i, v in enumerate(a):
        if v == '--volume':
            mount = a[i+1].split(':')
            assert Path(mount[0]).exists()
            assert mount[1] != '/run/enclo', 'secrets must not cover the shared runtime directory'
            if 'secrets-service' in mount[0]:
                assert mount[1:] == ['/run/enclo/secrets', 'ro']
    sys.exit(0)
if a[0] == 'init':
    if os.environ.get('FAIL') == 'init': print('init rejected', file=sys.stderr); sys.exit(1)
    (root/'initialized').touch(); sys.exit(0)
if a[0] == 'unshare':
    assert (root/'initialized').exists()
    os.execvp(a[2], a[2:])
if a[0] in ['start', 'run']:
    assert (root/'service-ready').exists(), 'container started before secrets readiness'
    time.sleep(.2); sys.exit(0)
if a[0] == 'inspect': print('running'); sys.exit(0)
if a[0] == 'exec' and a[-1] == 'nested-setup':
    assert '--user=root' in a
    if os.environ.get('FAIL') == 'hook': print('setup rejected', file=sys.stderr); sys.exit(1)
if a[0] in ['exec', 'rm', 'stop']: sys.exit(0)
raise SystemExit('unexpected podman command: '+str(a))
"#,
        );
        f.script(
            "enclo-secrets",
            r#"#!/usr/bin/env python3
import os, sys, socket, signal, json
from pathlib import Path
root = Path(os.environ['FIXTURE'])
a = sys.argv[1:]
if a[0] == 'generate-basic-auth': print('password_hash = "test"'); sys.exit(0)
with open(root/'events', 'a') as log: log.write(json.dumps(['service']+a)+'\n')
(root/'service-pid').write_text(str(os.getpid()))
if os.environ.get('FAIL') == 'activation': sys.exit(1)
if '--listen' in a:
    assert (root/'initialized').exists(), 'bridge proxy bound before network init'
    addr = a[a.index('--listen')+1]
    assert addr == '127.0.0.2:18080'
    listener = socket.socket(); listener.bind(('127.0.0.2', 0)); listener.listen()
else:
    assert not (root/'initialized').exists(), 'host mode must retain its original lifecycle'
    listener = socket.socket(fileno=int(a[a.index('--listen-fd')+1]))
    assert listener.getsockname()[0] == '127.0.0.1'
path = a[a.index('--socket')+1]
sock = socket.socket(socket.AF_UNIX); sock.bind(path); sock.listen()
(root/'service-ready').touch()
while True:
    conn, _ = sock.accept(); conn.close()
"#,
        );
        fs::write(f.0.join("ca.pem"), "test CA").unwrap();
        fs::write(
            f.0.join("secrets.toml"),
            "[proxy]\nenabled=true\n[proxy.tls]\nca_cert='ca.pem'\n",
        )
        .unwrap();
        f
    }
    fn script(&self, name: &str, script: &str) {
        let path = self.0.join("bin").join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn run(&self, network: &str, rootless: bool, fail: &str) -> std::process::Output {
        fs::write(
            self.0.join("container.toml"),
            format!(
                r#"
[container]
name = "test-secrets"
image = "test"
network = "{network}"
devices = ["/dev/fuse"]
security_opts = ["seccomp=unconfined", "unmask=ALL"]
env = ["XDG_RUNTIME_DIR=%t"]
post_start = [{{ args = ["nested-setup"], user = "root" }}]
[modules.secrets]
enabled = true
config = "{}"
[modules.container_client]
enabled = false
[modules.terminfo]
enabled = false
"#,
                self.0.join("secrets.toml").display()
            ),
        )
        .unwrap();
        Command::new(self.0.join("bin/enclo"))
            .args(["run", "--config"])
            .arg(self.0.join("container.toml"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.0.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("HOME", self.0.join("home"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("FIXTURE", &self.0)
            .env("ROOTLESS", if rootless { "true" } else { "false" })
            .env("FAIL", fail)
            .output()
            .unwrap()
    }
    fn events(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.0.join("events"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn assert_stopped(&self) {
        if let Ok(pid) = fs::read_to_string(self.0.join("service-pid")) {
            assert!(
                !PathBuf::from(format!("/proc/{}", pid.trim())).exists(),
                "secrets process leaked"
            );
        }
        assert!(!self.0.join("runtime/enclo/test-secrets").exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
use rand_core::RngCore;

#[test]
fn bridge_activation_order_and_failures_and_host_regression() {
    for (network, rootless, failure) in [
        ("isolated", true, ""),
        ("isolated", false, ""),
        ("host", true, ""),
        ("isolated", true, "create"),
        ("isolated", true, "init"),
        ("isolated", true, "activation"),
        ("isolated", true, "hook"),
    ] {
        let f = Fixture::new();
        let out = f.run(network, rootless, failure);
        assert_eq!(
            out.status.success(),
            failure.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let events = f.events();
        let index = |op: &str| events.iter().position(|e| e[0] == op);
        if network == "host" {
            assert!(index("create").is_none());
            assert!(index("init").is_none());
            assert!(index("unshare").is_none());
            assert!(index("run").is_some());
        } else {
            assert!(index("run").is_none());
            if failure == "create" {
                assert!(index("init").is_none());
            } else {
                assert!(index("create").unwrap() < index("init").unwrap());
            }
            if failure.is_empty() {
                assert!(index("init").unwrap() < index("service").unwrap());
                assert!(index("service").unwrap() < index("start").unwrap());
                assert_eq!(index("unshare").is_some(), rootless);
                let create = &events[index("create").unwrap()];
                assert!(
                    create.iter().any(|a| a.contains("@127.0.0.2:18080")),
                    "wrong proxy address: {create:?}"
                );
            } else {
                if failure == "hook" {
                    assert!(index("start").is_some());
                    assert!(String::from_utf8_lossy(&out.stderr)
                        .contains("container.post_start command failed"));
                } else {
                    assert!(index("start").is_none());
                }
                assert!(index("rm").is_some());
            }
        }
        if failure.is_empty() {
            let launch = events
                .iter()
                .find(|a| a[0] == "create" || a[0] == "run")
                .unwrap();
            assert!(launch.contains(&"--device=/dev/fuse".into()));
            assert!(launch.contains(&"--security-opt=seccomp=unconfined".into()));
            assert!(launch.contains(&"--security-opt=unmask=ALL".into()));
            assert!(launch
                .iter()
                .any(|arg| arg.starts_with("--env=XDG_RUNTIME_DIR=") && !arg.contains("%t")));
            assert!(events
                .iter()
                .any(|a| a[0] == "exec" && a.last().is_some_and(|a| a == "nested-setup")));
        }
        f.assert_stopped();
    }
}
