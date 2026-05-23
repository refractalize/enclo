use std::path::PathBuf;
use clap::Parser;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::UnixStream,
};
use insectjar_proxy_store_common::{codec, Request, Response, ResponseResult};

const DEFAULT_SOCKET: &str = "/run/insectjar-secrets.sock";

#[derive(Parser)]
#[command(name = "insectjar-aws-credentials", about = "Fetch AWS credentials from insectjar-secrets")]
struct Cli {
    /// Path to the insectjar-secrets Unix socket
    #[arg(long, short = 's', default_value = DEFAULT_SOCKET)]
    socket: PathBuf,

    /// AWS credentials profile to retrieve
    profile: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("insectjar-aws-credentials: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let secret_name = format!(
        "AWS_CREDENTIALS?profile={}",
        urlencoding::encode(&cli.profile)
    );

    let stream = UnixStream::connect(&cli.socket)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to {}: {e}", cli.socket.display()))?;

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let req = Request { id: 0, name: secret_name };
    let bytes = codec::encode(&req)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    drop(writer);

    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        anyhow::bail!("connection closed without a response");
    }

    let resp: Response = serde_json::from_str(line.trim_end())
        .map_err(|e| anyhow::anyhow!("invalid response from server: {e}"))?;

    match resp.result {
        ResponseResult::Ok { value } => {
            println!("{value}");
            Ok(())
        }
        ResponseResult::Err { error } => {
            anyhow::bail!("{error}");
        }
    }
}
