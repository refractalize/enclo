pub mod codec;
pub mod server;
pub mod service;
pub mod client;
pub mod unix_client;

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
