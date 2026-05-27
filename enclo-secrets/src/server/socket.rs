use std::{path::PathBuf, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream},
    sync::{Notify, RwLock},
};
use enclo_backend_protocol::{codec, Request, Response, ResponseResult};
use crate::access::AccessPolicy;
use crate::config::Config;
use crate::store::StoreStack;

pub async fn run(
    socket_path: PathBuf,
    store: Arc<StoreStack>,
    config: Arc<RwLock<Arc<Config>>>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    let _ = tokio::fs::remove_file(&socket_path).await;

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", socket_path.display()))?;

    tracing::info!("socket listening on {}", socket_path.display());

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let store = Arc::clone(&store);
                        let config = Arc::clone(&config);
                        tokio::spawn(handle_connection(stream, store, config));
                    }
                    Err(e) => {
                        tracing::warn!("socket accept error: {e}");
                        continue;
                    }
                }
            }

            _ = shutdown.notified() => {
                break;
            }
        }
    }

    let _ = tokio::fs::remove_file(&socket_path).await;
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    store: Arc<StoreStack>,
    config: Arc<RwLock<Arc<Config>>>,
) {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let req: Request = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(_) => break,
        };

        let policy = {
            let cfg = config.read().await;
            AccessPolicy::new(&cfg.secrets.access)
        };

        let permitted = policy.is_permitted(&req.name);
        let result = if permitted {
            let r = store.get(&req.name).await;
            match &r {
                ResponseResult::Ok { .. } => tracing::info!(secret = req.name.as_str(), "secret served via socket"),
                ResponseResult::NotFound => tracing::warn!(secret = req.name.as_str(), "secret not found"),
                ResponseResult::Error { error } => tracing::error!(secret = req.name.as_str(), error, "error fetching secret"),
            }
            r
        } else {
            tracing::warn!(secret = req.name.as_str(), "socket secret request denied by access policy");
            ResponseResult::NotFound
        };

        let container_result = match result {
            ResponseResult::Ok { value } => ResponseResult::Ok { value },
            ResponseResult::NotFound | ResponseResult::Error { .. } => ResponseResult::NotFound,
        };

        let resp = Response { id: req.id, result: container_result };

        let bytes = match codec::encode(&resp) {
            Ok(b) => b,
            Err(_) => break,
        };

        if writer.write_all(&bytes).await.is_err() { break; }
        if writer.flush().await.is_err() { break; }
    }
}
