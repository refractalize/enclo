#[cfg(target_os = "macos")]
use serde::{Deserialize, Serialize};

/// Commands sent from CLI → daemon over the Unix socket (macOS).
#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Start { name: String, config_path: String },
    Stop { name: String, #[serde(default)] force: bool },
    Status { name: String },
    ListContainers,
    Restart { name: String, config_path: String, #[serde(default)] force: bool },
    EnsureRunning { name: String, config_path: String },
    Reload { name: String },
    ModuleStatuses { name: String },
    UserExecCount { name: String },
}

/// Single-line JSON response written back by the daemon.
#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(target_os = "macos")]
impl Response {
    pub fn ok() -> Self {
        Self { ok: true, value: None, error: None }
    }
    pub fn ok_value(v: impl Serialize) -> Self {
        Self { ok: true, value: serde_json::to_value(v).ok(), error: None }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self { ok: false, value: None, error: Some(msg.into()) }
    }
}
