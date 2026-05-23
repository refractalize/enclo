use std::time::Duration;
use insectjar_proxy_store_common::server;
use tokio::{process::Command, time::timeout};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("insectjar-secret-store-aws-vault: {e}");
        std::process::exit(1);
    }
}

async fn handle(_id: u64, name: String) -> Result<String, String> {
    let profile = parse_profile(&name)?;

    let deadline = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let result = timeout(deadline, run_aws_vault(&profile)).await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(format!(
            "aws-vault timed out after {DEFAULT_TIMEOUT_SECS}s for profile '{profile}' \
             (SSO session may be expired)"
        )),
    }
}

async fn run_aws_vault(profile: &str) -> Result<String, String> {
    let output = Command::new("aws-vault")
        .args(["export", "--format", "json", profile])
        .output()
        .await
        .map_err(|e| format!("failed to run aws-vault: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("aws-vault failed for profile '{profile}': {}", stderr.trim()));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("aws-vault output is not valid UTF-8: {e}"))?;

    let value = stdout.trim().to_string();
    if value.is_empty() {
        return Err(format!("aws-vault returned empty output for profile '{profile}'"));
    }

    Ok(value)
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
