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

    let value = stdout
        .trim_end_matches(|c| c == '\r' || c == '\n')
        .to_string();
    if value.is_empty() {
        return ResponseResult::Error {
            error: "gh auth token returned an empty token; run 'gh auth login'".into(),
        };
    }

    // Status JSON is available on older gh too; only newer versions include
    // tokenExpiresAt. Fetch the selected host/account in one snapshot so that a
    // native refresh cannot associate an old token with its replacement's TTL.
    let host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new("gh")
            .args([
                "auth",
                "status",
                "--active",
                "--hostname",
                &host,
                "--json",
                "hosts",
                "--show-token",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if let Ok(Ok(output)) = status {
        if output.status.success() {
            if let Some(result) = status_credentials(&output.stdout, &host, &value) {
                return result;
            }
        }
    }
    // Optional metadata must not break existing CLI versions or offline usage.
    ResponseResult::Ok {
        value,
        expires_at: None,
    }
}

fn status_credentials(bytes: &[u8], host: &str, original_token: &str) -> Option<ResponseResult> {
    let status: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let entries = status.get("hosts")?.get(host)?.as_array()?;
    let entry = entries.iter().find(|entry| {
        entry.get("active").and_then(|v| v.as_bool()) == Some(true)
            && entry.get("state").and_then(|v| v.as_str()) == Some("success")
    })?;
    let expiry = match entry.get("tokenExpiresAt") {
        None | Some(serde_json::Value::Null) => return None,
        value => value,
    };
    let value = entry.get("token")?.as_str()?;
    if value.is_empty() || value.contains("***") {
        return None;
    }
    // A changed token is accepted only for a refreshable selected credential.
    if value != original_token && entry.get("refreshable").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let expires_at = expiry.and_then(|v| v.as_str()).and_then(|expiry| {
        time::OffsetDateTime::parse(expiry, &time::format_description::well_known::Rfc3339).ok()
    });
    Some(match expires_at {
        Some(expires_at) => ResponseResult::Ok {
            value: value.to_string(),
            expires_at: Some(expires_at),
        },
        None => ResponseResult::Error {
            error: "gh returned invalid token expiration metadata".into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn status(token: &str, expiry: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"hosts":{"github.com":[{"active":true,"state":"success","token":token,"refreshable":true,"tokenExpiresAt":expiry}]}})).unwrap()
    }
    #[test]
    fn selected_account_expiry_and_rotated_token_are_taken_from_the_same_snapshot() {
        let data = status("rotated", serde_json::json!("2030-01-01T00:00:00Z"));
        match status_credentials(&data, "github.com", "old") {
            Some(ResponseResult::Ok {
                value,
                expires_at: Some(expiry),
            }) => {
                assert_eq!(value, "rotated");
                assert_eq!(expiry.unix_timestamp(), 1893456000);
            }
            _ => panic!("missing expiry"),
        }
        assert!(status_credentials(&data, "other.example", "old").is_none());
    }
    #[test]
    fn legacy_or_masked_status_falls_back_and_invalid_expiry_does_not_leak() {
        assert!(status_credentials(
            &status("dummy", serde_json::Value::Null),
            "github.com",
            "dummy"
        )
        .is_none());
        assert!(status_credentials(
            &status("***", serde_json::json!("2030-01-01T00:00:00Z")),
            "github.com",
            "dummy"
        )
        .is_none());
        match status_credentials(
            &status("PRIVATE", serde_json::json!("bad")),
            "github.com",
            "PRIVATE",
        ) {
            Some(ResponseResult::Error { error }) => assert!(!error.contains("PRIVATE")),
            _ => panic!("accepted malformed expiry"),
        }
    }
}
