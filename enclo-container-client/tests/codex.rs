use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::Command,
};
#[test]
fn setup_installs_private_placeholder_preserves_config_and_refuses_real_login() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "# keep me\nmodel = \"test-model\"\n",
    )
    .unwrap();
    let socket = root.path().join("secrets.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["name"], "CODEX_ACCOUNT_ID");
            writeln!(
                stream,
                "{{\"requestId\":0,\"status\":\"ok\",\"value\":\"test-account\"}}"
            )
            .unwrap();
        }
    });
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_enclo-container-client"))
            .args(["auth", "setup", "codex", "--socket"])
            .arg(&socket)
            .env("CODEX_HOME", &home)
            .output()
            .unwrap()
    };
    for _ in 0..2 {
        let out = run();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    server.join().unwrap();
    let auth: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["tokens"]["account_id"], "test-account");
    assert_eq!(auth["tokens"]["refresh_token"], "");
    assert!(auth["tokens"]["access_token"]
        .as_str()
        .unwrap()
        .ends_with("enclo-placeholder"));
    assert_eq!(
        fs::metadata(home.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let config = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(config.contains("# keep me"));
    assert!(config.contains("test-model"));
    fs::write(
        home.join("auth.json"),
        "{\"tokens\":{\"access_token\":\"real-login\"}}",
    )
    .unwrap();
    assert!(!run().status.success());
    assert!(fs::read_to_string(home.join("auth.json"))
        .unwrap()
        .contains("real-login"));
}
