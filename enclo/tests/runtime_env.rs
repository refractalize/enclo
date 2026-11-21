//! Check environment precedence in child processes without changing the test
//! runner's environment, contacting daemons, or creating any sockets.
use rand_core::RngCore;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn runtime_override_precedence_applies_to_services_and_sessions() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "enclo-env-{}-{:x}",
        std::process::id(),
        rand_core::OsRng.next_u64()
    )));
    let good = fixture.0.join("good");
    let bad = fixture.0.join("bad");
    fs::create_dir_all(&good).unwrap();
    fs::create_dir_all(bad.join("enclo/connections")).unwrap();
    fs::create_dir_all(bad.join("connections")).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    // This deliberately invalid root makes selecting the wrong path observable.
    fs::set_permissions(bad.join("enclo"), fs::Permissions::from_mode(0o755)).unwrap();
    for action in ["services", "sessions"] {
        for (flag, env, xdg, succeeds) in [
            (None, Some(good.as_os_str()), &bad, true),
            (Some(&good), Some(bad.as_os_str()), &bad, true),
            (Some(&bad), Some(good.as_os_str()), &good, false),
            (None, None, &good, true),
            (None, None, &bad, false),
            (None, Some(std::ffi::OsStr::new("")), &bad, false),
        ] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo"));
            cmd.args([action, "list"])
                .env("XDG_RUNTIME_DIR", xdg)
                .env_remove("ENCLO_RUNTIME_DIR");
            if let Some(root) = env {
                cmd.env("ENCLO_RUNTIME_DIR", root);
            }
            if let Some(root) = flag {
                cmd.arg("--runtime-dir").arg(root);
            }
            let output = cmd.output().unwrap();
            assert_eq!(
                output.status.success(),
                succeeds,
                "{action}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if succeeds {
                assert_eq!(output.stdout, b"[]\n");
            } else {
                assert!(String::from_utf8_lossy(&output.stderr).contains("mode 0700"));
            }
        }
    }
}

#[test]
fn config_override_is_an_application_root_and_flag_wins() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "enclo-cfg-{}-{:x}",
        std::process::id(),
        rand_core::OsRng.next_u64()
    )));
    let good = fixture.0.join("good");
    let bad = fixture.0.join("missing");
    let xdg = fixture.0.join("xdg");
    let home = fixture.0.join("home");
    for root in [&good, &xdg.join("enclo"), &home.join(".config/enclo")] {
        fs::create_dir_all(root.join("connections")).unwrap();
        fs::write(
            root.join("connections/work.toml"),
            "[target]\nhost='../invalid'\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("hosts")).unwrap();
        // Parse failure proves selection without ever invoking Podman or a daemon.
        fs::write(
            root.join("hosts/work.toml"),
            "[container]\n[ssh]\nuser='dev'\n",
        )
        .unwrap();
    }
    for (flag, env, xdg_root, selected) in [
        (None, Some(good.as_os_str()), Some(&bad), true),
        (Some(&good), Some(bad.as_os_str()), Some(&bad), true),
        (Some(&bad), Some(good.as_os_str()), Some(&xdg), false),
        (None, None, Some(&xdg), true),
        (None, Some(std::ffi::OsStr::new("")), Some(&xdg), true),
        (None, None, None, true),
    ] {
        for command in ["services", "host"] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo"));
            cmd.args([
                command,
                if command == "host" {
                    "start"
                } else {
                    "run"
                },
                "work",
            ])
            .env("HOME", &home)
            .env_remove("ENCLO_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME");
            if let Some(root) = flag {
                cmd.arg("--config-dir").arg(root);
            }
            if let Some(root) = env {
                cmd.env("ENCLO_CONFIG_DIR", root);
            }
            if let Some(root) = xdg_root {
                cmd.env("XDG_CONFIG_HOME", root);
            }
            let output = cmd.output().unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            let expected = if command == "services" {
                "must contain"
            } else {
                "missing field `image`"
            };
            assert_eq!(error.contains(expected), selected, "{command}: {error}");
        }
    }
}

#[test]
fn missing_session_profile_names_the_selected_configuration_path() {
    let root = std::env::temp_dir().join(format!(
        "enclo-missing-{}",
        rand_core::RngCore::next_u64(&mut rand_core::OsRng)
    ));
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_enclo"))
        .arg("--config-dir")
        .arg(&root)
        .args(["session", "work", "--", "true"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains(root.join("connections/work.toml").to_str().unwrap()));
    assert!(error.contains("ENCLO_CONFIG_DIR"));
}
