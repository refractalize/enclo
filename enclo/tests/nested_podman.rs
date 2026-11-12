#![cfg(target_os = "linux")]
use rand_core::RngCore;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn nested_setup_is_required_and_precedes_central_startup_hooks() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for (failure, systemd) in [(false, false), (true, false), (false, true), (true, true)] {
        let root =
            std::env::temp_dir().join(format!("enclo-nested-{:x}", rand_core::OsRng.next_u64()));
        let fixture = Fixture(root.clone());
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let podman = root.join("bin/podman");
        fs::write(
            &podman,
            r#"#!/usr/bin/python3
import sys,os,json,time
from pathlib import Path
root=Path(os.environ['FIXTURE']);a=sys.argv[1:]
with open(root/'events','a') as f:f.write(json.dumps(a)+'\n')
if a[0]=='run':time.sleep(.4)
elif a[0]=='inspect':print('running')
elif a[0]=='exec':
 if 'is-system-running' in ' '.join(a):
  assert '--user=root' in a
  if os.environ['FAIL']=='true':sys.exit(8)
  (root/'systemd-ready').touch()
 elif 'python3' in a:
  if os.environ['SYSTEMD']=='true':assert (root/'systemd-ready').exists()
  assert '--user=root' in a
  assert 'cap_setuid=ep' in a[a.index('-c')+1]
  if os.environ['FAIL']=='true':print('nested setup failed',file=sys.stderr);sys.exit(7)
  (root/'configured').touch()
 elif a[-1]=='after-nested':assert (root/'configured').exists()
 elif a[-2:]==['unshare','true']:
  assert '--remote=false' in a and (root/'configured').exists()
  assert '--user='+str(os.getuid())+':'+str(os.getgid()) in a
elif a[0] not in ['rm','stop']:raise AssertionError(a)
"#,
        )
        .unwrap();
        fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("container.toml");
        fs::write(
            &config,
            r#"
[container]
name="nested-fixture"
image="fixture"
post_start=[{args=["after-nested"],user="root"}]
[modules.nested_podman]
enabled=true
[modules.container_client]
enabled=false
[modules.terminfo]
enabled=false
"#,
        )
        .unwrap();
        if systemd {
            let mut text = fs::read_to_string(&config).unwrap();
            text.push_str("\n[modules.systemd]\nenabled=true\n");
            fs::write(&config, text).unwrap();
        }
        let out = Command::new(env!("CARGO_BIN_EXE_enclo"))
            .args(["run", "--config"])
            .arg(&config)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("FIXTURE", &root)
            .env("FAIL", failure.to_string())
            .env("SYSTEMD", systemd.to_string())
            .output()
            .unwrap();
        assert_eq!(
            out.status.success(),
            !failure,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let events: Vec<Vec<String>> = fs::read_to_string(root.join("events"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let run = events.iter().find(|a| a[0] == "run").unwrap();
        for flag in [
            "--device=/dev/fuse",
            "--device=/dev/net/tun",
            "--security-opt=seccomp=unconfined",
            "--security-opt=unmask=ALL",
        ] {
            assert!(run.contains(&flag.to_string()));
        }
        if systemd {
            assert!(run.contains(&"--user=0:0".into()));
            assert!(!run.contains(&format!("--user={}:{}", unsafe { libc::geteuid() }, unsafe { libc::getegid() })));
            assert_eq!(run.last().unwrap(), "/sbin/init");
        }
        let custom = events
            .iter()
            .position(|a| a.last().map(String::as_str) == Some("after-nested"));
        if failure {
            assert!(custom.is_none());
            assert!(events.iter().any(|a| a[0] == "rm"));
        } else {
            let setup = events
                .iter()
                .position(|a| a.contains(&"python3".to_string()))
                .unwrap();
            assert!(setup < custom.unwrap());
        }
        drop(fixture);
    }
}
