use std::io::ErrorKind;

use enclo_backend_protocol::{server, ResponseResult};
use tokio::process::Command;

const SUPPORTED_SECRETS: &[&str] = &["GITHUB_TOKEN", "GH_TOKEN"];

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("enclo-secret-store-github: {e}");
        std::process::exit(1);
    }
}

async fn handle(_id: u64, name: String) -> ResponseResult {
    if !SUPPORTED_SECRETS.contains(&name.as_str()) {
        return ResponseResult::NotFound;
    }

    let output = match Command::new("gh").args(["auth", "token"]).output().await {
        Ok(o) => o,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return ResponseResult::Error {
                error: "GitHub CLI command 'gh' was not found on PATH".into(),
            };
        }
        Err(e) => {
            return ResponseResult::Error {
                error: format!("failed to run gh auth token: {e}"),
            };
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        let detail = if msg.is_empty() {
            "gh auth token exited unsuccessfully; run 'gh auth login'".to_string()
        } else {
            msg.to_string()
        };
        return ResponseResult::Error {
            error: format!("gh auth token failed: {detail}"),
        };
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(e) => {
            return ResponseResult::Error {
                error: format!("gh auth token output is not valid UTF-8: {e}"),
            };
        }
    };

    let value = stdout.trim_end_matches(|c| c == '\r' || c == '\n').to_string();
    if value.is_empty() {
        return ResponseResult::Error {
            error: "gh auth token returned an empty token; run 'gh auth login'".into(),
        };
    }

    ResponseResult::Ok { value }
}
