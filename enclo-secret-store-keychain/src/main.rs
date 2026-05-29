use enclo_backend_protocol::{server, ResponseResult};

const SERVICE: &str = "org.enclo";

#[tokio::main]
async fn main() {
    if let Err(e) = server::run(handle).await {
        eprintln!("enclo-secret-store-keychain: {e}");
        std::process::exit(1);
    }
}

async fn handle(_id: u64, name: String) -> ResponseResult {
    tokio::task::spawn_blocking(move || lookup(&name))
        .await
        .unwrap_or_else(|e| ResponseResult::Error { error: format!("task panicked: {e}") })
}

#[cfg(target_os = "macos")]
fn lookup(name: &str) -> ResponseResult {
    use security_framework::passwords::get_generic_password;

    match get_generic_password(SERVICE, name) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => {
                let trimmed = value.trim_end_matches('\n').to_string();
                if trimmed.is_empty() {
                    ResponseResult::Error { error: format!("keychain item '{name}' is empty") }
                } else {
                    ResponseResult::Ok { value: trimmed }
                }
            }
            Err(e) => ResponseResult::Error {
                error: format!("keychain value for '{name}' is not valid UTF-8: {e}"),
            },
        },
        Err(e) if e.code() == -25300 => ResponseResult::NotFound,
        Err(e) => ResponseResult::Error {
            error: format!("keychain lookup failed for '{name}': {e}"),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn lookup(_name: &str) -> ResponseResult {
    ResponseResult::Error {
        error: "enclo-secret-store-keychain is only supported on macOS".into(),
    }
}
