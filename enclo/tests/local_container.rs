//! Provider contract tests: fake Podman/SSH, real keys, no host services.
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};
struct Fixture {
    dir: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("elc-{:x}", rand_core::OsRng.next_u64()));
        for sub in ["bin", "config/hosts", "config/connections"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        fs::write(dir.join("config/hosts/dev.toml"), "[container]\nimage='fixture:latest'\nvolumes=['volume:/home/dev']\n[ssh]\nuser='dev'\n").unwrap();
        fs::write(
            dir.join("config/connections/work.toml"),
            "[target]\nhost='dev'\n",
        )
        .unwrap();
        fs::write(dir.join("bin/podman"), r#"#!/usr/bin/python3
import sys, os, json, subprocess
from pathlib import Path
root = Path(os.environ['FIXTURE'])
a = sys.argv[1:]
assert a.pop(0) == '--remote=false', a
with (root/'calls').open('a') as f: f.write(json.dumps(a)+'\n')
p = root/'machine.json'
v = json.loads(p.read_text()) if p.exists() else None
if a[:2] == ['container','exists']: sys.exit(0 if v else 1)
if a[:2] == ['image','inspect']:
    if a[2] == 'missing:latest': sys.exit(125)
    print('{}')
elif a[0] == 'volume':
    vp=root/'volume.json'
    if a[1]=='exists': sys.exit(0 if vp.exists() else 1)
    if a[1]=='create':
        labels=dict(a[i+1].split('=',1) for i,x in enumerate(a) if x=='--label')
        vp.write_text(json.dumps({'Labels':labels})); print(a[-1])
    elif a[1]=='inspect': print('['+vp.read_text()+']')
elif a[0]=='run' and '--rm' in a:
    assert '--network=none' in a
    if '--entrypoint=getent' in a: print(f'dev:x:{os.getuid()}:{os.getgid()}::/home/dev:/bin/sh')
    else: (root/'home').mkdir(exist_ok=True)
elif a[0] == 'rm':
    assert '--volumes' not in a and '-v' not in a
    assert a[-1] == v['Id']
    assert not v['State']['Running'] or '--force' in a
    p.unlink()
elif a[0] == 'inspect': print(json.dumps([v]))
elif a[0] == 'run':
    assert '--detach' in a and '--userns=keep-id' in a
    assert ('--systemd=always' in a and '--init' not in a) or ('--systemd=false' in a and '--init' in a)
    publish=next(x for x in a if x.startswith('--publish='))
    assert publish.startswith('--publish=127.0.0.1:') and publish.endswith(':22') and '--network=bridge' in a
    if (root/'port-busy').exists(): print('bind: address already in use',file=sys.stderr); sys.exit(125)
    published_port=publish.split(':')[1] or ((root/'ssh_port').read_text().strip() if (root/'ssh_port').exists() else '23456')
    assert a[-1] == '/run/enclo-ssh/boot.sh'
    assert a[a.index('--entrypoint')+1] == '/bin/sh'
    labels = dict(a[i+1].split('=',1) for i,x in enumerate(a) if x == '--label')
    mount = a[a.index('--volume')+1].split(':')[0]
    counter = root/'count'
    count = int(counter.read_text())+1 if counter.exists() else 1
    counter.write_text(str(count))
    v = {'Id':format(0xabc122+count,'x'), 'State':{'Running':True}, 'Config':{'Labels':labels}, 'Mounts':[{'Source':mount,'Destination':'/run/enclo-ssh'}], 'NetworkSettings':{'Ports':{'22/tcp':[{'HostIp':'127.0.0.1','HostPort':published_port}]}}}
    if 'io.enclo.home' in labels:
        home=json.loads(labels['io.enclo.home'])
        assert home['volume']+':'+home['path']+':nocopy' in a
        v['Mounts'].append({'Type':'volume','Name':home['volume'],'Destination':home['path']})
    p.write_text(json.dumps(v)); print('abc123')
elif a[0] in ['start','stop']:
    v['State']['Running'] = a[0] == 'start'
    if a[0] == 'start': v['NetworkSettings']['Ports']['22/tcp'][0]['HostPort']=(root/'ssh_port').read_text().strip() if (root/'ssh_port').exists() else '23457'
    p.write_text(json.dumps(v))
elif a[0] == 'ps': print(json.dumps([v] if v else []))
elif a[0] == 'exec':
    if 'getent' in a:
        print('root:x:0:0::/root:/bin/sh' if a[-1] == 'root' else f'dev:x:{os.getuid()}:{os.getgid()}::/home/dev:/bin/sh')
    elif 'enclo-container-checkpoint' in a:
        args=a[a.index('sh'):]
        args[3]=args[3].replace('/var/lib/enclo',str(root/'checkpoint'))
        sys.exit(subprocess.run(args).returncode)
    elif 'enclo-container-init' in a:
        work=root/'init-work'; work.mkdir(exist_ok=True)
        assert '--workdir' in a and a[a.index('--workdir')+1]=='/'
        (root/'hook-user').write_text(a[a.index('--user')+1])
        sys.exit(subprocess.run(a[a.index('sh'):],cwd=work).returncode)
    elif 'enclo-home-init' in a:
        assert '--workdir' in a and 'HOME=/home/dev' in a
        sys.exit(subprocess.run(a[a.index('sh'):], cwd=root/'home').returncode)
    elif 'is-system-running' in a: print('running')
    elif 'python3' in a:
        assert 'cap_setuid=ep' in a[a.index('-c')+1]
        (root/'nested-ready').touch()
        if (root/'fail-nested').exists(): sys.exit(7)
    elif 'unshare' in a:
        assert (root/'nested-ready').exists() and '--remote=false' in a
        assert '--mount-proc' in a[-1] or 'mount --rbind /proc' in a[-1]
        if (root/'fail-proc').exists(): sys.exit(9)
    elif 'id' in a: print(os.getuid())
    elif 'cat' in a: print((root/'host.pub').read_text() if (root/'host.pub').exists() else 'ssh-ed25519 AAAATEST fixture')
    elif 'sh' in a: (root/'unit').write_text(sys.stdin.read())
    elif 'ssh-keygen' in a or 'systemctl' in a or 'test' in a: pass
    else: raise Exception(a)
else: raise Exception(a)
"#).unwrap();
        fs::write(
            dir.join("bin/ssh"),
            r#"#!/usr/bin/python3
import sys, os
from pathlib import Path
a=sys.argv[1:]
config=Path(a[a.index('-F')+1]).read_text()
assert 'StrictHostKeyChecking yes' in config
assert 'HostKeyAlias enclo-abc' in config
assert 'IdentitiesOnly yes' in config
assert 'HostName 127.0.0.1' in config
assert a[-1]=='true'
"#,
        )
        .unwrap();
        for name in ["ssh", "podman"] {
            fs::set_permissions(
                dir.join("bin").join(name),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        Self { dir }
    }
    fn state(&self) -> PathBuf {
        let scope = format!(
            "{:x}",
            Sha256::digest(
                self.dir
                    .join("config")
                    .canonicalize()
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
            )
        );
        self.dir
            .join("state/containers")
            .join(&scope[..12])
            .join("dev")
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo"));
        cmd.args(args)
            .env("FIXTURE", &self.dir)
            .env("ENCLO_CONFIG_DIR", self.dir.join("config"))
            .env("ENCLO_RUNTIME_DIR", self.dir.join("runtime"))
            .env("ENCLO_STATE_DIR", self.dir.join("state"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("bin").display()),
            );
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let result = self.run(args);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }
}
#[test]
fn independent_machine_lifecycle_and_fresh_endpoint_discovery() {
    let fixture = Fixture::new();
    assert!(fixture.ok(&["host", "status", "dev"]).is_null());
    let endpoint = fixture.ok(&["host", "start", "dev"]);
    assert_eq!(endpoint["port"], 23456);
    let runtime = fixture.dir.join("runtime/containers/dev");
    assert!(fixture.state().join("client_ed25519").exists());
    assert!(!fixture.state().join("bootstrap/client_ed25519").exists());
    assert!(!fixture.dir.join("unit").exists());
    assert!(!fs::read_to_string(fixture.dir.join("calls"))
        .unwrap()
        .contains("systemctl"));
    assert!(
        fs::read_to_string(fixture.state().join("bootstrap/boot.sh"))
            .unwrap()
            .contains("exec /usr/sbin/sshd -D")
    );
    // Idempotent start doesn't create a second machine or change its key.
    let key = fs::read(fixture.state().join("client_ed25519")).unwrap();
    fixture.ok(&["host", "start", "dev"]);
    assert_eq!(
        key,
        fs::read(fixture.state().join("client_ed25519")).unwrap()
    );
    assert_eq!(
        fixture.ok(&["host", "list"]).as_array().unwrap().len(),
        1
    );
    let config_path = fixture.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    fs::write(&config_path, format!("{original}\n# comment only\n")).unwrap();
    fixture.ok(&["host", "start", "dev"]);
    let bootstrap_before = fs::read(fixture.state().join("bootstrap/sshd_config")).unwrap();
    fs::write(&config_path, original.replace("user='dev'", "user='other'")).unwrap();
    let changed = fixture.run(&["host", "start", "dev"]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("configuration changed"));
    assert_eq!(
        bootstrap_before,
        fs::read(fixture.state().join("bootstrap/sshd_config")).unwrap()
    );
    fs::write(&config_path, original).unwrap();
    fixture.ok(&["host", "stop", "dev"]);
    assert_eq!(
        fixture.ok(&["host", "status", "dev"])["running"],
        false
    );
    assert_eq!(fixture.ok(&["host", "start", "dev"])["port"], 23457);
    assert!(runtime.join("ssh_config-abc123-23457").exists());
    let calls = fs::read_to_string(fixture.dir.join("calls")).unwrap();
    assert_eq!(
        calls.lines().filter(|l| l.starts_with("[\"run\"")).count(),
        1
    );
}
#[test]
fn rejects_foreign_containers_and_wrong_state_without_stopping_them() {
    let fixture = Fixture::new();
    fixture.ok(&["host", "start", "dev"]);
    let result = fixture.run(&[
        "host",
        "start",
        "dev",
        "--state-dir",
        fixture.dir.join("other").to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("persistent state directory"));
    let path = fixture.dir.join("machine.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["Config"]["Labels"]["io.enclo.scope"] = "foreign".into();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    let result = fixture.run(&["host", "stop", "dev", "--force"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not owned"));
    assert!(!fs::read_to_string(fixture.dir.join("calls"))
        .unwrap()
        .lines()
        .any(|l| l.starts_with("[\"stop\"")));
}

#[test]
#[ignore = "requires temporary local sshd and permission to bind fixture sockets"]
fn container_services_and_sessions_use_real_ssh() {
    use std::{
        net::{TcpListener, TcpStream},
        process::{Child, Stdio},
        time::{Duration, Instant},
    };
    struct Children(Vec<Child>);
    impl Drop for Children {
        fn drop(&mut self) {
            for child in self.0.iter_mut().rev() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(Instant::now() < deadline, "fixture readiness timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let fixture = Fixture::new();
    let mut children = Children(vec![]);
    fs::remove_file(fixture.dir.join("bin/ssh")).unwrap();
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    fs::write(
        fixture.dir.join("config/hosts/dev.toml"),
        format!(
            "[container]\nimage='fixture'\n[ssh]\nuser='{}'\n",
            user.trim()
        ),
    )
    .unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fs::write(fixture.dir.join("ssh_port"), port.to_string()).unwrap();
    assert!(Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(fixture.dir.join("host"))
        .status()
        .unwrap()
        .success());
    fs::write(fixture.dir.join("sshd_config"),format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/host\nPidFile {root}/sshd.pid\nAuthorizedKeysFile {state}/bootstrap/authorized_keys\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowTcpForwarding yes\n",root=fixture.dir.display(), state=fixture.state().display())).unwrap();
    children.0.push(
        Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(fixture.dir.join("sshd_config"))
            .stderr(Stdio::from(
                fs::File::create(fixture.dir.join("sshd.log")).unwrap(),
            ))
            .spawn()
            .unwrap(),
    );
    wait(|| TcpStream::connect(("127.0.0.1", port)).is_ok());
    children.0.push(
        fixture
            .command(&["services", "run", "work"])
            .stderr(Stdio::from(
                fs::File::create(fixture.dir.join("services.log")).unwrap(),
            ))
            .spawn()
            .unwrap(),
    );
    wait(|| {
        if let Some(status) = children.0[1].try_wait().unwrap() {
            panic!(
                "services failed {status}: {}",
                fs::read_to_string(fixture.dir.join("services.log")).unwrap()
            );
        }
        fixture.dir.join("runtime/connections/work/ready").exists()
    });
    assert_eq!(
        fixture.ok(&["services", "status", "work"])["transport_ready"],
        true
    );
    let result = fixture.run(&[
        "session",
        "work",
        "--",
        "printf",
        "%s",
        "literal ' ; $(false)",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"literal ' ; $(false)");
    children.0.push(
        fixture
            .command(&["session", "work", "--", "sleep", "20"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait(|| {
        !fixture
            .ok(&["sessions", "list"])
            .as_array()
            .unwrap()
            .is_empty()
    });
    let result = fixture.run(&["host", "stop", "dev"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("active sessions"));
    fixture.ok(&["services", "stop", "work"]);
    assert!(children.0[2].try_wait().unwrap().is_none());
    assert_eq!(fixture.ok(&["host", "status", "dev"])["running"], true);
    let result = fixture.run(&["sessions", "stop", "--connection", "work", "--all"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fixture.ok(&["host", "stop", "dev"]);
    struct Stop(Command);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.output();
        }
    }
    let _stop = Stop(fixture.command(&["services", "stop", "work"]));
    let result = fixture.run(&["session", "work", "--", "true"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fixture.ok(&["host", "status", "dev"])["running"], true);
    assert_eq!(
        fixture.ok(&["services", "status", "work"])["transport_ready"],
        true
    );
    fixture.ok(&["host", "recreate", "dev", "--force"]);
    let result = fixture.run(&["session", "work", "--", "true"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fixture.ok(&["services", "status", "work"])["target"],
        "enclo-abc124"
    );
    fixture.ok(&["services", "stop", "work"]);
    fixture.ok(&["host", "stop", "dev"]);
}

#[test]
fn systemd_is_opt_in_and_masks_are_applied_before_init() {
    let fixture = Fixture::new();
    let path = fixture.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{original}\n[systemd]\nenabled=true\nmask=['systemd-oomd.service','systemd-oomd.socket']\n")).unwrap();
    fixture.ok(&["host", "start", "dev"]);
    let boot = fs::read_to_string(fixture.state().join("bootstrap/boot.sh")).unwrap();
    assert!(boot.contains(
        "systemctl --root=/ mask -- 'sshd.service' 'sshd.socket' 'ssh.service' 'ssh.socket' 'systemd-oomd.service' 'systemd-oomd.socket'\nexec /sbin/init"
    ));
    assert!(fs::read_to_string(fixture.dir.join("unit"))
        .unwrap()
        .contains("KillMode=process"));
}

#[test]
fn invalid_mask_configuration_never_touches_podman() {
    let fixture = Fixture::new();
    let path = fixture.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&path).unwrap();
    for section in [
        "[systemd]\nmask=['a.service']",
        "[systemd]\nenabled=true\nmask=['../a.service']",
        "[systemd]\nenabled=true\nmask=['a.service;touch bad']",
        "[systemd]\nenabled=true\nmask=['--help.service']",
        "[systemd]\nenabled=true\nmask=['enclo-sshd.service']",
    ] {
        fs::write(&path, format!("{original}\n{section}\n")).unwrap();
        let result = fixture.run(&["host", "start", "dev"]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("mask"));
        assert!(!fixture.dir.join("calls").exists());
    }
}

#[test]
fn runtime_is_disposable_and_recreation_preserves_identity() {
    let fixture = Fixture::new();
    let first = fixture.ok(&["host", "start", "dev"]);
    let state = fixture.state();
    let key = fs::read(state.join("client_ed25519")).unwrap();
    let pin = fs::read(state.join("instances/abc123/known_hosts")).unwrap();
    fs::remove_dir_all(fixture.dir.join("runtime")).unwrap();
    assert_eq!(
        fixture.ok(&["host", "start", "dev"])["container_id"],
        first["container_id"]
    );
    assert_eq!(key, fs::read(state.join("client_ed25519")).unwrap());
    assert_eq!(
        pin,
        fs::read(state.join("instances/abc123/known_hosts")).unwrap()
    );
    fixture.ok(&[
        "host",
        "start",
        "dev",
        "--runtime-dir",
        fixture.dir.join("alternate").to_str().unwrap(),
    ]);
    for operation in ["remove", "recreate"] {
        let result = fixture.run(&["host", operation, "dev"]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("container is running"));
    }
    let config = fixture.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, "invalid toml [").unwrap();
    assert!(!fixture
        .run(&["host", "recreate", "dev", "--force"])
        .status
        .success());
    fs::write(
        &config,
        original.replace("fixture:latest", "missing:latest"),
    )
    .unwrap();
    let missing = fixture.run(&["host", "recreate", "dev", "--force"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("existing container retained"));
    assert_eq!(
        fixture.ok(&["host", "status", "dev"])["container_id"],
        first["container_id"]
    );
    fs::write(&config, original.replace("fixture:latest", "fixture:v2")).unwrap();
    let replacement = fixture.ok(&["host", "recreate", "dev", "--force"]);
    assert_ne!(replacement["container_id"], first["container_id"]);
    assert_eq!(key, fs::read(state.join("client_ed25519")).unwrap());
    assert!(state.join("instances/abc124/known_hosts").exists());
    fixture.ok(&["host", "stop", "dev"]);
    fs::remove_file(config).unwrap();
    assert!(fixture.ok(&["host", "remove", "dev"]).is_null());
    assert!(fixture.ok(&["host", "remove", "dev"]).is_null());
    assert!(fixture.ok(&["host", "status", "dev"]).is_null());
    assert_eq!(key, fs::read(state.join("client_ed25519")).unwrap());
    let calls = fs::read_to_string(fixture.dir.join("calls")).unwrap();
    assert!(!calls.contains("--volumes"));
}

#[test]
fn pinned_host_key_cannot_be_silently_replaced() {
    let fixture = Fixture::new();
    fixture.ok(&["host", "start", "dev"]);
    let pin = fixture.state().join("instances/abc123/known_hosts");
    fs::write(&pin, "enclo-abc123 ssh-ed25519 DIFFERENT\n").unwrap();
    let result = fixture.run(&["host", "start", "dev"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("host key changed"));
    assert!(fs::read_to_string(pin).unwrap().contains("DIFFERENT"));
}

#[test]
fn recreation_imports_legacy_runtime_identity() {
    let fixture = Fixture::new();
    fixture.ok(&["host", "start", "dev"]);
    let key_path = fixture.state().join("client_ed25519");
    let key = fs::read(&key_path).unwrap();
    let legacy = fixture.dir.join("runtime/containers/dev/client_ed25519");
    fs::rename(&key_path, &legacy).unwrap();
    fixture.ok(&["host", "recreate", "dev", "--force"]);
    assert_eq!(key, fs::read(&key_path).unwrap());
}

#[test]
fn state_root_precedence_and_private_permissions() {
    for mode in ["flag", "env", "xdg", "home", "relative"] {
        let fixture = Fixture::new();
        let home = fixture.dir.join("home");
        let xdg = fixture.dir.join("xdg");
        let explicit = fixture.dir.join("flag");
        let mut command = fixture.command(&["host", "start", "dev"]);
        command.env("HOME", &home).env("XDG_STATE_HOME", &xdg);
        let expected = match mode {
            "flag" => {
                command.arg("--state-dir").arg(&explicit);
                explicit
            }
            "env" => fixture.dir.join("state"),
            "xdg" => {
                command.env("ENCLO_STATE_DIR", "");
                xdg.join("enclo")
            }
            "home" => {
                command
                    .env_remove("ENCLO_STATE_DIR")
                    .env_remove("XDG_STATE_HOME");
                home.join(".local/state/enclo")
            }
            "relative" => {
                command
                    .current_dir(&fixture.dir)
                    .args(["--state-dir", "relative-state"]);
                fixture.dir.join("relative-state")
            }
            _ => unreachable!(),
        };
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}: {}",
            mode,
            String::from_utf8_lossy(&result.stderr)
        );
        let endpoint: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        let state = PathBuf::from(endpoint["state_directory"].as_str().unwrap());
        assert!(state.starts_with(expected));
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state.join("client_ed25519"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn lifecycle_lock_is_shared_across_runtime_directories() {
    use std::os::fd::AsRawFd;
    let fixture = Fixture::new();
    fixture.ok(&["host", "start", "dev"]);
    let lock = fs::File::open(fixture.state().join("lifecycle.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let result = fixture.run(&[
        "host",
        "start",
        "dev",
        "--runtime-dir",
        fixture.dir.join("another-runtime").to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("lifecycle operation already in progress")
    );
}

#[test]
fn managed_home_persists_and_initialization_is_explicit_and_retryable() {
    let f = Fixture::new();
    let path = f.dir.join("config/hosts/dev.toml");
    let config = "[container]\nimage='fixture:latest'\n[ssh]\nuser='dev'\n[home]\nenabled=true\n[home.init]\ntimeout_secs=1\ncommand='echo initial >> count'\n";
    fs::write(&path, config).unwrap();
    let first = f.ok(&["host", "start", "dev"]);
    assert_eq!(first["home"]["path"], "/home/dev");
    f.ok(&["host", "start", "dev"]);
    f.ok(&["host", "recreate", "dev", "--force"]);
    assert_eq!(
        fs::read_to_string(f.dir.join("home/count")).unwrap(),
        "initial\n"
    );
    fs::write(&path, config.replace("echo initial", "echo changed")).unwrap();
    f.ok(&["host", "start", "dev"]);
    assert_eq!(
        fs::read_to_string(f.dir.join("home/count")).unwrap(),
        "initial\n"
    );
    f.ok(&["host", "home-init", "dev", "--rerun"]);
    assert_eq!(
        fs::read_to_string(f.dir.join("home/count")).unwrap(),
        "initial\nchanged\n"
    );
    fs::write(&path, config.replace("echo initial >> count", "exit 42")).unwrap();
    let failed = f.run(&["host", "home-init", "dev", "--rerun"]);
    assert!(!failed.status.success());
    assert_eq!(f.ok(&["host", "status", "dev"])["running"], false);
    assert!(!f.dir.join("home/.enclo-init/central").exists());
    fs::write(&path, config).unwrap();
    f.ok(&["host", "start", "dev"]);
    f.ok(&["host", "remove", "dev", "--force"]);
    let next = f.ok(&["host", "start", "dev"]);
    assert_eq!(first["home"], next["home"]);
    assert_eq!(
        fs::read_to_string(f.dir.join("home/count")).unwrap(),
        "initial\nchanged\ninitial\n"
    );
    fs::write(
        &path,
        config.replace("echo initial >> count", "exec sleep 2"),
    )
    .unwrap();
    assert!(!f
        .run(&["host", "home-init", "dev", "--rerun"])
        .status
        .success());
    assert_eq!(f.ok(&["host", "status", "dev"])["running"], false);
}

#[test]
fn managed_home_rejects_mount_conflicts_and_foreign_volumes() {
    let f = Fixture::new();
    let path = f.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{original}\n[home]\nenabled=true\n")).unwrap();
    assert!(!f.run(&["host", "start", "dev"]).status.success());
    assert!(!f.dir.join("machine.json").exists());
    fs::write(
        &path,
        format!(
            "{}\n[home]\nenabled=true\n",
            original.replace("volumes=['volume:/home/dev']", "")
        ),
    )
    .unwrap();
    fs::write(f.dir.join("volume.json"), r#"{"Labels":{}}"#).unwrap();
    assert!(!f.run(&["host", "start", "dev"]).status.success());
    assert!(!f.dir.join("machine.json").exists());
}

#[test]
fn machine_features_wire_devices_setup_and_local_desktop_without_host_services() {
    for systemd in [false, true] {
        let f = Fixture::new();
        let path = f.dir.join("config/hosts/dev.toml");
        let config = format!("[container]\nimage='fixture:latest'\n[ssh]\nuser='dev'\n[home]\nenabled=true\n[nested_podman]\nenabled=true\n[gpu]\nnvidia=['all']\ndrm=true\ndrm_nodes=['renderD128']\n[desktop]\nwayland=true\n[desktop.dbus]\nenabled=true\ntalk=['org.example.Test']\n[systemd]\nenabled={systemd}\n");
        fs::write(&path, config).unwrap();
        f.ok(&["host", "start", "dev"]);
        let calls: Vec<Vec<String>> = fs::read_to_string(f.dir.join("calls"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let run = calls
            .iter()
            .find(|a| a.contains(&"--detach".into()))
            .unwrap();
        for flag in [
            "--device=nvidia.com/gpu=all",
            "--device=/dev/dri/renderD128",
            "--group-add=keep-groups",
            "--device=/dev/fuse",
            "--security-opt=seccomp=unconfined",
            "--security-opt=unmask=ALL",
            "--unsetenv=CONTAINER_HOST",
        ] {
            assert!(run.contains(&flag.into()), "{flag}");
        }
        assert!(f.dir.join("nested-ready").exists());
        assert!(f.state().join("desktop/sockets").is_dir());
        let machine: serde_json::Value =
            serde_json::from_slice(&fs::read(f.dir.join("machine.json")).unwrap()).unwrap();
        let env: serde_json::Value = serde_json::from_str(
            machine["Config"]["Labels"]["io.enclo.session-env"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(env["WAYLAND_DISPLAY"], "/run/enclo-desktop/wayland");
        assert_eq!(
            env["XDG_RUNTIME_DIR"],
            format!("/run/user/{}", unsafe { libc::geteuid() })
        );
        // No host compositor, D-Bus or proxy is started by container start.
        assert!(!f.state().join("desktop/sockets/bus").exists());
        fs::write(f.dir.join("fail-nested"), "").unwrap();
        assert!(!f.run(&["host", "start", "dev"]).status.success());
        assert_eq!(f.ok(&["host", "status", "dev"])["running"], false);
    }
}
#[test]
fn nested_proc_mode_is_explicit_and_probe_failure_stops_machine() {
    for mode in ["private", "host"] {
        let f = Fixture::new();
        fs::write(f.dir.join("config/hosts/dev.toml"), format!(
            "[container]\nimage='fixture:latest'\n[ssh]\nuser='dev'\n[home]\nenabled=true\n[nested_podman]\nenabled=true\nproc_mount='{mode}'\n"
        )).unwrap();
        f.ok(&["host", "start", "dev"]);
        let calls: Vec<Vec<String>> = fs::read_to_string(f.dir.join("calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let setup = calls
            .iter()
            .find(|args| args.contains(&"python3".into()))
            .unwrap();
        assert_eq!(setup.last().unwrap(), mode);
        let probe = calls
            .iter()
            .find(|args| args.contains(&"unshare".into()))
            .unwrap();
        assert_eq!(
            probe.last().unwrap().contains("--mount-proc"),
            mode == "private"
        );
        fs::write(f.dir.join("fail-proc"), "").unwrap();
        let failure = f.run(&["host", "start", "dev"]);
        assert!(!failure.status.success());
        assert!(String::from_utf8_lossy(&failure.stderr).contains("proc mount check failed"));
        assert_eq!(f.ok(&["host", "status", "dev"])["running"], false);
    }
}

#[test]
fn machine_features_reject_conflicts_before_podman() {
    for extra in [
        "[nested_podman]\nenabled=true",
        "[nested_podman]\nproc_mount='host'",
        "[nested_podman]\nproc_mount='invalid'",
        "[gpu]\nnvidia=['all --privileged']",
        "[desktop.dbus]\ntalk=['org.example.Test']",
        "[desktop.dbus]\nenabled=true\ncall=['bad rule']",
    ] {
        let f = Fixture::new();
        let path = f.dir.join("config/hosts/dev.toml");
        let original = fs::read_to_string(&path).unwrap();
        fs::write(path, format!("{original}\n{extra}\n")).unwrap();
        assert!(!f.run(&["host", "start", "dev"]).status.success());
        assert!(!f.dir.join("calls").exists());
    }
}

#[test]
fn ssh_export_uses_persistent_trust_and_add_updates_only_its_block() {
    let f = Fixture::new();
    let home = f.dir.join("export-home");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    let config = home.join(".ssh/config");
    let original =
        "# unrelated configuration\nHost *\n Port 2222\nHost other\n HostName example.org\n";
    fs::write(&config, original).unwrap();
    f.ok(&["host", "start", "dev"]);
    let export = f.run(&["host", "export", "dev"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let text = String::from_utf8(export.stdout).unwrap();
    assert!(text.starts_with("Host enclo-dev\n"));
    assert!(text.contains("StrictHostKeyChecking yes"));
    assert!(text.contains("HostKeyAlias enclo-"));
    assert!(!text.contains("HostKeyAlias enclo-abc123"));
    assert!(!text.contains("/runtime/"));
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    let add = || {
        f.command(&["host", "export", "dev", "--add"])
            .env("HOME", &home)
            .output()
            .unwrap()
    };
    assert!(add().status.success());
    let first = fs::read_to_string(&config).unwrap();
    assert!(first.ends_with(original));
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(add().status.success());
    assert_eq!(fs::read_to_string(&config).unwrap(), first);
    let effective = Command::new("/usr/bin/ssh")
        .args(["-G", "-F"])
        .arg(&config)
        .arg("enclo-dev")
        .output()
        .unwrap();
    assert!(effective.status.success());
    assert!(String::from_utf8_lossy(&effective.stdout).contains("port 23456\n"));
    // The target file is updated while a dotfile-manager symlink is preserved.
    let target = home.join("dotfile");
    fs::rename(&config, &target).unwrap();
    std::os::unix::fs::symlink(&target, &config).unwrap();
    f.ok(&["host", "recreate", "dev", "--force"]);
    assert!(add().status.success());
    assert!(fs::symlink_metadata(&config)
        .unwrap()
        .file_type()
        .is_symlink());
    let updated = fs::read_to_string(&config).unwrap();
    assert_eq!(updated.matches("Host enclo-dev\n").count(), 1);
    assert_eq!(updated, first);
    assert!(!updated.contains("HostKeyAlias enclo-abc123"));
    assert!(updated.ends_with(original));
    let custom = f.run(&["host", "export", "dev", "--host", "enclo-work"]);
    assert!(custom.status.success());
    assert!(custom.stdout.starts_with(b"Host enclo-work\n"));
    assert!(!f
        .run(&["host", "export", "dev", "--host", "bad\nHost *"])
        .status
        .success());
    fs::write(&target, "Host enclo-dev\n HostName user-managed.example\n").unwrap();
    assert!(!add().status.success());
    assert!(fs::read_to_string(&target)
        .unwrap()
        .contains("user-managed.example"));
    f.ok(&["host", "stop", "dev"]);
    let stopped = f.run(&["host", "export", "dev"]);
    assert!(!stopped.status.success());
    assert!(String::from_utf8_lossy(&stopped.stderr).contains("container is stopped"));
}

#[test]
fn fixed_ssh_port_and_stable_export_trust_survive_recreation() {
    let f = Fixture::new();
    let config = f.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{original}host_port=2222\n")).unwrap();
    assert_eq!(f.ok(&["host", "start", "dev"])["port"], 2222);
    let before = f.run(&["host", "export", "dev"]);
    assert!(before.status.success());
    let pin = f.state().join("known_hosts");
    let first = fs::read_to_string(&pin).unwrap();
    // A changed key for the same instance remains forbidden, including export trust.
    fs::write(f.dir.join("host.pub"), "ssh-ed25519 AAAANEW fixture").unwrap();
    assert!(!f.run(&["host", "start", "dev"]).status.success());
    assert_eq!(fs::read_to_string(&pin).unwrap(), first);
    assert_eq!(
        f.ok(&["host", "recreate", "dev", "--force"])["port"],
        2222
    );
    assert!(fs::read_to_string(&pin).unwrap().contains("AAAANEW"));
    assert_eq!(f.run(&["host", "export", "dev"]).stdout, before.stdout);
    assert!(
        fs::read_to_string(f.state().join("instances/abc123/known_hosts"))
            .unwrap()
            .contains("AAAATEST")
    );
    fs::write(&config, format!("{original}host_port=2223\n")).unwrap();
    assert!(!f.run(&["host", "start", "dev"]).status.success());
    fs::write(f.dir.join("port-busy"), "").unwrap();
    let failure = f.run(&["host", "recreate", "dev", "--force"]);
    assert!(!failure.status.success());
    let error = String::from_utf8_lossy(&failure.stderr);
    assert!(error.contains("127.0.0.1:2223") && error.contains("address already in use"));
}

#[test]
fn invalid_host_ports_are_rejected_before_podman() {
    for value in ["0", "65536", "-1", "'2222'"] {
        let f = Fixture::new();
        let config = f.dir.join("config/hosts/dev.toml");
        let original = fs::read_to_string(&config).unwrap();
        fs::write(&config, format!("{original}host_port={value}\n")).unwrap();
        assert!(!f.run(&["host", "start", "dev"]).status.success());
        assert!(!f.dir.join("calls").exists());
    }
}

#[test]
fn container_init_follows_home_and_tracks_instance_not_home_lifetime() {
    let f = Fixture::new();
    let path = f.dir.join("config/hosts/dev.toml");
    let config = "[container]\nimage='fixture:latest'\n[container.init]\ncommand='test -f ../home/count; echo init >> count'\n[ssh]\nuser='dev'\n[home]\nenabled=true\n[home.init]\ncommand='echo home >> count'\n";
    fs::write(&path, config).unwrap();
    f.ok(&["host", "start", "dev"]);
    assert_eq!(fs::read_to_string(f.dir.join("hook-user")).unwrap(), "0:0");
    f.ok(&["host", "start", "dev"]);
    f.ok(&["host", "init", "dev"]);
    f.ok(&["host", "stop", "dev"]);
    f.ok(&["host", "start", "dev"]);
    assert_eq!(fs::read_to_string(f.dir.join("init-work/count")).unwrap(), "init\n");
    fs::write(&path, config.replace("echo init", "echo changed")).unwrap();
    f.ok(&["host", "start", "dev"]);
    assert_eq!(fs::read_to_string(f.dir.join("init-work/count")).unwrap(), "init\n");
    f.ok(&["host", "init", "dev", "--rerun"]);
    f.ok(&["host", "recreate", "dev", "--force"]);
    assert_eq!(fs::read_to_string(f.dir.join("init-work/count")).unwrap(), "init\nchanged\nchanged\n");
    assert_eq!(fs::read_to_string(f.dir.join("home/count")).unwrap(), "home\n");
}

#[test]
fn container_init_failure_and_timeout_stop_machine_and_are_retryable() {
    for script in ["echo intentional >&2; exit 42", "sleep 2"] {
        let f = Fixture::new();
        let path = f.dir.join("config/hosts/dev.toml");
        let config = format!("[container]\nimage='fixture:latest'\n[container.init]\nuser='dev'\ntimeout_secs=1\ncommand='{script}'\n[ssh]\nuser='dev'\n");
        fs::write(&path, &config).unwrap();
        let out = f.run(&["host", "start", "dev"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("container initialization"));
        assert_eq!(f.ok(&["host", "status", "dev"])["running"], false);
        assert!(!f.dir.join("checkpoint/container-init").exists());
        fs::write(&path, config.replace(script, "echo success >> count")).unwrap();
        f.ok(&["host", "start", "dev"]);
        assert_eq!(fs::read_to_string(f.dir.join("hook-user")).unwrap(), format!("{}:{}", unsafe { libc::geteuid() }, unsafe { libc::getegid() }));
        assert!(f.dir.join("checkpoint/container-init").exists());
        fs::write(&path, config).unwrap();
        assert!(!f.run(&["host", "init", "dev", "--rerun"]).status.success());
        assert!(!f.dir.join("checkpoint/container-init").exists());
    }
}

#[test]
fn container_init_rejects_invalid_configuration() {
    for options in ["timeout_secs=0", "user='--root'", "command=\"bad\\u0000command\""] {
        let f = Fixture::new();
        fs::write(f.dir.join("config/hosts/dev.toml"), format!("[container]\nimage='fixture:latest'\n[container.init]\n{options}\n[ssh]\nuser='dev'\n")).unwrap();
        assert!(!f.run(&["host", "start", "dev"]).status.success());
        assert!(!f.dir.join("calls").exists());
    }
}

#[test]
fn hostname_is_passed_to_podman_and_changes_require_recreation() {
    let f = Fixture::new();
    let path = f.dir.join("config/hosts/dev.toml");
    let original = fs::read_to_string(&path).unwrap();
    f.ok(&["host", "start", "dev"]);
    let initial_calls = fs::read_to_string(f.dir.join("calls")).unwrap();
    assert!(!initial_calls.contains("--hostname"));
    fs::write(&path, original.replace("[container]", "[container]\nhostname='forbes'")).unwrap();
    let changed = f.run(&["host", "start", "dev"]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("configuration changed"));
    f.ok(&["host", "recreate", "dev", "--force"]);
    let calls: Vec<Vec<String>> = fs::read_to_string(f.dir.join("calls")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    let run = calls.iter().rev().find(|args| args[0] == "run").unwrap();
    assert_eq!(run[run.iter().position(|arg| arg == "--hostname").unwrap() + 1], "forbes");
    assert!(!run.iter().any(|arg| arg.starts_with("--privileged")));
}

#[test]
fn invalid_hostnames_are_rejected_before_podman() {
    for name in ["", "-bad", "bad-", "two words", "a..b", "a/b", "forbes\nother", "é", &"a".repeat(65)] {
        let f = Fixture::new();
        let path = f.dir.join("config/hosts/dev.toml");
        fs::write(&path, format!("[container]\nimage='fixture:latest'\nhostname={}\n[ssh]\nuser='dev'\n", serde_json::to_string(name).unwrap())).unwrap();
        assert!(!f.run(&["host", "start", "dev"]).status.success());
        assert!(!f.dir.join("calls").exists());
    }
}
