use enclo_backend_protocol::{server, ResponseResult};
use std::time::Duration;
use tokio::{process::Command, time::timeout};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("enclo-secret-store-aws-vault: {e}");
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
        Err(e) => {
            return ResponseResult::Error {
                error: format!("failed to run aws-vault: {e}"),
            }
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ResponseResult::Error {
            error: format!(
                "aws-vault failed for profile '{profile}': {}",
                stderr.trim()
            ),
        };
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(e) => {
            return ResponseResult::Error {
                error: format!("aws-vault output is not valid UTF-8: {e}"),
            }
        }
    };

    let value = stdout.trim().to_string();
    if value.is_empty() {
        return ResponseResult::Error {
            error: format!("aws-vault returned empty output for profile '{profile}'"),
        };
    }

    credential_response(value)
}

fn credential_response(value: String) -> ResponseResult {
    let parsed = match serde_json::from_str::<serde_json::Value>(&value) {
        Ok(parsed) if parsed.is_object() => parsed,
        _ => {
            return ResponseResult::Error {
                error: "aws-vault returned invalid credential JSON".into(),
            }
        }
    };
    let expires_at = match parsed.get("Expiration") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(expiry)) if expiry.is_empty() => None,
        Some(serde_json::Value::String(expiry)) => match time::OffsetDateTime::parse(
            expiry,
            &time::format_description::well_known::Rfc3339,
        ) {
            Ok(expiry) => Some(expiry),
            Err(_) => {
                return ResponseResult::Error {
                    error: "aws-vault returned invalid credential expiration".into(),
                }
            }
        },
        _ => {
            return ResponseResult::Error {
                error: "aws-vault returned invalid credential expiration".into(),
            }
        }
    };
    ResponseResult::Ok { value, expires_at }
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

    Err(format!(
        "secret name '{name}' is missing 'profile' query parameter"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aws_expiration_is_extracted_without_changing_the_credential_document() {
        let value = r#"{"Version":1,"AccessKeyId":"dummy","Expiration":"2030-01-01T00:00:00Z"}"#;
        match credential_response(value.into()) {
            ResponseResult::Ok {
                value: result,
                expires_at: Some(expiry),
            } => {
                assert_eq!(result, value);
                assert_eq!(expiry.unix_timestamp(), 1893456000);
            }
            _ => panic!("missing expiry"),
        }
        assert!(matches!(
            credential_response(r#"{"Version":1,"AccessKeyId":"dummy"}"#.into()),
            ResponseResult::Ok {
                expires_at: None,
                ..
            }
        ));
    }
    #[test]
    fn invalid_expiration_fails_without_leaking_credential_contents() {
        for value in [
            r#"{"SecretAccessKey":"PRIVATE","Expiration":"bad"}"#,
            r#"{"Expiration":12}"#,
            "PRIVATE",
        ] {
            match credential_response(value.into()) {
                ResponseResult::Error { error } => assert!(!error.contains("PRIVATE")),
                _ => panic!("accepted bad expiry"),
            }
        }
    }
}
