use enclo_backend_protocol::{server, ResponseResult};
use tokio::process::Command;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("enclo-secret-store-pass: {e}");
        std::process::exit(1);
    }
}

async fn handle(_id: u64, name: String) -> ResponseResult {
    let output = match Command::new("pass").arg("show").arg(&name).output().await {
        Ok(o) => o,
        Err(e) => return ResponseResult::Error { error: format!("failed to run pass: {e}") },
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        if msg.contains("is not in the password store") {
            return ResponseResult::NotFound;
        }
        return ResponseResult::Error { error: format!("pass failed: {msg}") };
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(e) => return ResponseResult::Error { error: format!("pass output is not valid UTF-8: {e}") },
    };

    let value = stdout.lines().next().unwrap_or("").to_string();
    if value.is_empty() {
        return ResponseResult::Error { error: format!("pass returned empty value for '{name}'") };
    }

    ResponseResult::Ok { value }
}
