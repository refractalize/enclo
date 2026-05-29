use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use clap::Parser;
use enclo_backend_protocol::{codec, Request, Response};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, Mutex},
};

#[derive(Parser)]
#[command(name = "enclo-container-bridge", about = "Container-side secrets socket bridge")]
struct Args {
    /// Unix socket path to listen on inside the container.
    #[arg(long)]
    socket: PathBuf,
}

// channel_id -> (original_request_id, response_sender)
type PendingMap = Arc<Mutex<HashMap<u64, (u64, oneshot::Sender<Response>)>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    if args.socket.exists() {
        tokio::fs::remove_file(&args.socket).await?;
    }
    if let Some(parent) = args.socket.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let next_id = Arc::new(AtomicU64::new(1));
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    let (req_tx, req_rx) = mpsc::channel::<Request>(128);

    tokio::spawn(stdout_writer(req_rx));
    tokio::spawn(stdin_reader(Arc::clone(&pending)));

    let listener = UnixListener::bind(&args.socket)?;
    tracing::info!("listening on {}", args.socket.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let next_id = Arc::clone(&next_id);
        let pending = Arc::clone(&pending);
        let req_tx = req_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, next_id, pending, req_tx).await {
                tracing::warn!("connection error: {e}");
            }
        });
    }
}

/// Serialises all outbound requests to stdout; must be the sole stdout writer.
async fn stdout_writer(mut req_rx: mpsc::Receiver<Request>) {
    let mut writer = BufWriter::new(tokio::io::stdout());
    while let Some(req) = req_rx.recv().await {
        let bytes = match codec::encode(&req) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("encode error: {e}");
                continue;
            }
        };
        if writer.write_all(&bytes).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

/// Reads responses from stdin and dispatches them to the waiting connection handler.
async fn stdin_reader(pending: PendingMap) {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let resp: Response = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("bad response from stdin: {e}");
                continue;
            }
        };
        let entry = pending.lock().await.remove(&resp.id);
        if let Some((original_id, tx)) = entry {
            let _ = tx.send(Response { id: original_id, result: resp.result });
        } else {
            tracing::warn!("no pending entry for channel_id {}", resp.id);
        }
    }
}

/// Handles a single socket connection: reads requests sequentially, assigns
/// unique channel IDs, forwards over stdout, and writes responses back.
async fn handle_connection(
    stream: UnixStream,
    next_id: Arc<AtomicU64>,
    pending: PendingMap,
    req_tx: mpsc::Sender<Request>,
) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let req: Request = serde_json::from_str(line.trim_end())
            .map_err(|e| anyhow::anyhow!("invalid request: {e}"))?;

        let original_id = req.id;
        let channel_id = next_id.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(channel_id, (original_id, tx));

        req_tx
            .send(Request { id: channel_id, name: req.name })
            .await
            .map_err(|_| anyhow::anyhow!("stdout writer closed"))?;

        let resp = rx.await.map_err(|_| anyhow::anyhow!("stdin reader closed"))?;
        let bytes = codec::encode(&resp)?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
    }

    Ok(())
}
