use insectjar_proxy_store_common::server;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("insectjar-secret-store-pass: {e}");
        std::process::exit(1);
    }
}

async fn handle(_id: u64, name: String) -> Result<String, String> {
    let output = Command::new("pass")
        .arg("show")
        .arg(&name)
        .output()
        .await
        .map_err(|e| format!("failed to run pass: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pass exited with error: {}", stderr.trim()));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("pass output is not valid UTF-8: {e}"))?;

    let value = stdout.lines().next().unwrap_or("").to_string();
    if value.is_empty() {
        return Err(format!("pass returned empty value for {name}"));
    }

    Ok(value)
}
