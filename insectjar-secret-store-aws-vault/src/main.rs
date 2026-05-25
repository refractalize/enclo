use std::time::Duration;
use insectjar_proxy_store_common::{server, ResponseResult};
use tokio::{process::Command, time::timeout};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("insectjar-secret-store-aws-vault: {e}");
        std::process::exit(1);
    }
}

async fn handle(_id: u64, name: String) -> ResponseResult {
    let profile = match parse_profile(&name) {
        Ok(p) => p,
        Err(e) => return ResponseResult::Error { error: e },
    };

    let deadline = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    match timeout(deadline, run_aws_vault(&profile)).await {
        Ok(result) => result,
        Err(_) => ResponseResult::Error {
            error: format!(
                "aws-vault timed out after {DEFAULT_TIMEOUT_SECS}s for profile '{profile}' \
                 (SSO session may be expired)"
            ),
        },
    }
}

async fn run_aws_vault(profile: &str) -> ResponseResult {
    let output = match Command::new("aws-vault")
        .args(["export", "--format", "json", profile])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => return ResponseResult::Error { error: format!("failed to run aws-vault: {e}") },
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ResponseResult::Error {
            error: format!("aws-vault failed for profile '{profile}': {}", stderr.trim()),
        };
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(e) => return ResponseResult::Error { error: format!("aws-vault output is not valid UTF-8: {e}") },
    };

    let value = stdout.trim().to_string();
    if value.is_empty() {
        return ResponseResult::Error { error: format!("aws-vault returned empty output for profile '{profile}'") };
    }

    ResponseResult::Ok { value }
}

/// Extracts and URL-decodes the `profile` query parameter from a secret name
/// of the form `AWS_CREDENTIALS?profile=<encoded-profile>`.
fn parse_profile(name: &str) -> Result<String, String> {
    let query = name.splitn(2, '?').nth(1).ok_or_else(|| {
        format!("secret name '{name}' is missing query string (expected ?profile=<name>)")
    })?;

    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("profile=") {
            return urlencoding::decode(value)
                .map(|s| s.into_owned())
                .map_err(|e| format!("invalid percent-encoding in profile name: {e}"));
        }
    }

    Err(format!("secret name '{name}' is missing 'profile' query parameter"))
}
