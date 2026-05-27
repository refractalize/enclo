use std::path::PathBuf;
use clap::Parser;
use enclo_backend_protocol::{unix_client, ResponseResult};

const DEFAULT_SOCKET: &str = "/run/enclo-secrets.sock";

#[derive(Parser)]
#[command(name = "enclo-aws-credentials", about = "Fetch AWS credentials from enclo-secrets")]
struct Cli {
    /// Path to the enclo-secrets Unix socket
    #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
    socket: PathBuf,

    /// AWS credentials profile to retrieve
    profile: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("enclo-aws-credentials: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let secret_name = format!(
        "AWS_CREDENTIALS?profile={}",
        urlencoding::encode(&cli.profile)
    );

    let mut responses = unix_client::request(&cli.socket, &[secret_name])
        .await
        .map_err(|e| anyhow::anyhow!("failed to query {}: {e}", cli.socket.display()))?;
    let resp = responses
        .pop()
        .ok_or_else(|| anyhow::anyhow!("connection closed without a response"))?;

    match resp.result {
        ResponseResult::Ok { value } => {
            println!("{value}");
            Ok(())
        }
        ResponseResult::NotFound => {
            anyhow::bail!("secret not found");
        }
        ResponseResult::Error { .. } => {
            std::process::exit(1);
        }
    }
}
