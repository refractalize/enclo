use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::{Command, Stdio},
};

#[test]
fn aws_uses_selected_connection_and_emits_only_credentials() {
    let home = tempfile::tempdir().unwrap();
    let generation = tempfile::Builder::new()
        .prefix("enclo-svc.")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = generation.path().join("secrets.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_enclo-container-client"));
        cmd.env("HOME", home.path());
        for key in ["ENCLO_CONNECTION", "ENCLO_SECRETS_SOCKET", "ENCLO_PROXY"] {
            cmd.env_remove(key);
        }
        cmd
    };
    let mut install = command()
        .args(["service-install", "work"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let settings = serde_json::json!({
        "generation": generation.path(),
        "env": {"ENCLO_SECRETS_SOCKET": socket}
    });
    write!(install.stdin.take().unwrap(), "{settings}").unwrap();
    assert!(install.wait().unwrap().success());
    let credentials = r#"{"Version":1,"AccessKeyId":"synthetic","SecretAccessKey":"synthetic-secret","SessionToken":"synthetic-token","Expiration":"2030-01-01T00:00:00Z"}"#;
    let server = std::thread::spawn(move || {
        for status in ["ok", "notFound", "error"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request["name"],
                "AWS_CREDENTIALS?profile=team%2Fdev%20%26admin%3Dno"
            );
            let response = serde_json::json!({
                "requestId": request["requestId"], "status": status,
                "value": credentials, "error": "sensitive provider diagnostic"
            });
            writeln!(stream, "{response}").unwrap();
        }
    });
    for success in [true, false, false] {
        let result = command()
            .args([
                "--connection",
                "work",
                "aws-credentials",
                "team/dev &admin=no",
            ])
            .output()
            .unwrap();
        assert_eq!(result.status.success(), success);
        if success {
            assert_eq!(
                String::from_utf8_lossy(&result.stdout),
                format!("{credentials}\n")
            );
            assert!(result.stderr.is_empty());
        } else {
            assert!(result.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&result.stderr).contains("sensitive provider"));
        }
    }
    server.join().unwrap();
}
