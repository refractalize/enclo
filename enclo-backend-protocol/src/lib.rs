pub mod client;
pub mod codec;
pub mod server;
pub mod service;
pub mod unix_client;

/// Default path to the enclo-secrets Unix socket inside the container.
pub const DEFAULT_SECRETS_SOCKET: &str = "/run/enclo/secrets/secrets.sock";

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(rename = "requestId")]
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    #[serde(rename = "requestId")]
    pub id: u64,
    #[serde(flatten)]
    pub result: ResponseResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ResponseResult {
    Ok {
        value: String,
        /// Absolute expiry supplied by the backend, independent of cache policy.
        #[serde(
            default,
            rename = "expiresAt",
            skip_serializing_if = "Option::is_none",
            with = "time::serde::rfc3339::option"
        )]
        expires_at: Option<time::OffsetDateTime>,
    },
    NotFound,
    Error {
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_expiry_is_backward_compatible_and_round_trips() {
        let legacy: Response =
            serde_json::from_str(r#"{"requestId":1,"status":"ok","value":"dummy"}"#).unwrap();
        assert!(matches!(
            legacy.result,
            ResponseResult::Ok {
                expires_at: None,
                ..
            }
        ));
        assert!(serde_json::to_value(legacy)
            .unwrap()
            .get("expiresAt")
            .is_none());
        let json = r#"{"requestId":2,"status":"ok","value":"dummy","expiresAt":"2030-01-01T12:30:00+01:00"}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        let bytes = serde_json::to_vec(&response).unwrap();
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        match decoded.result {
            ResponseResult::Ok {
                expires_at: Some(expiry),
                ..
            } => assert_eq!(expiry.unix_timestamp(), 1893497400),
            _ => panic!("missing expiry"),
        }
    }
    #[test]
    fn malformed_expiry_is_rejected() {
        for expiry in [r#""not-a-time""#, "17"] {
            let json =
                format!(r#"{{"requestId":1,"status":"ok","value":"dummy","expiresAt":{expiry}}}"#);
            assert!(serde_json::from_str::<Response>(&json).is_err());
        }
    }
}
