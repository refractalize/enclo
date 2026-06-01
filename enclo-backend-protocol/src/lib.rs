pub mod codec;
pub mod server;
pub mod service;
pub mod client;
pub mod unix_client;

/// Default path to the enclo-secrets Unix socket inside the container.
pub const DEFAULT_SECRETS_SOCKET: &str = "/run/enclo/secrets.sock";

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
    Ok { value: String },
    NotFound,
    Error { error: String },
}
