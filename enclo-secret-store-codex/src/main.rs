use enclo_backend_protocol::{server, ResponseResult};

#[tokio::main]
async fn main() {
    if let Err(error) = server::run(|_, name| async move {
        if name != "CODEX_CREDENTIALS" && name != "CODEX_ACCOUNT_ID" {
            return ResponseResult::NotFound;
        }
        match if name == "CODEX_ACCOUNT_ID" {
            enclo_secret_store_codex::account_id().await
        } else {
            enclo_secret_store_codex::credentials().await
        } {
            Ok(value) => ResponseResult::Ok { value },
            Err(error) => ResponseResult::Error {
                error: format!("{error:#}"),
            },
        }
    })
    .await
    {
        eprintln!("enclo-secret-store-codex: {error}");
        std::process::exit(1);
    }
}
