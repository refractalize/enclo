use enclo_backend_protocol::{server, ResponseResult};

#[tokio::main]
async fn main() {
    if let Err(error) = server::run(|_, name| async move {
        if name != "CODEX_CREDENTIALS" && name != "CODEX_ACCOUNT_ID" {
            return ResponseResult::NotFound;
        }
        match if name == "CODEX_ACCOUNT_ID" {
            enclo_secret_store_codex::account_id()
                .await
                .map(|value| (value, None))
        } else {
            async {
                let (value, expiry) = enclo_secret_store_codex::credentials().await?;
                let expires_at = time::OffsetDateTime::from_unix_timestamp(i64::try_from(expiry)?)?;
                Ok((value, Some(expires_at)))
            }
            .await
        } {
            Ok((value, expires_at)) => ResponseResult::Ok { value, expires_at },
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
