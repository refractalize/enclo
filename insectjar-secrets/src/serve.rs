use std::{path::PathBuf, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream},
    signal::unix::{signal, SignalKind},
    sync::{Notify, RwLock},
};
use insectjar_proxy_store_common::{codec, Request, Response, ResponseResult};
use insectjar_secret_store::StoreStack;
use crate::access::AccessPolicy;
use crate::config::Config;

pub async fn run(socket_path: PathBuf, config_path: PathBuf, config: Config) -> anyhow::Result<()> {
    let shutdown = Arc::new(Notify::new());

    let store = Arc::new(
        StoreStack::build(&config.store, Arc::clone(&shutdown), "insectjar-secrets")
            .await
            .map_err(|e| anyhow::anyhow!("failed to start store: {e}"))?,
    );
    let policy = Arc::new(RwLock::new(AccessPolicy::new(&config.access)));

    let _ = tokio::fs::remove_file(&socket_path).await;

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", socket_path.display()))?;

    tracing::info!("listening on {}", socket_path.display());

    let shutdown_clone = Arc::clone(&shutdown);
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        shutdown_clone.notify_one();
    });

    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let store = Arc::clone(&store);
                        let policy = Arc::clone(&policy);
                        tokio::spawn(handle_connection(stream, store, policy));
                    }
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                        continue;
                    }
                }
            }

            _ = sighup.recv() => {
                match reload_config(&config_path, &store, &policy).await {
                    Ok(()) => tracing::info!("config reloaded"),
                    Err(e) => tracing::warn!("config reload failed: {e:#}"),
                }
            }

            _ = shutdown.notified() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    let _ = tokio::fs::remove_file(&socket_path).await;
    Ok(())
}

async fn reload_config(
    config_path: &PathBuf,
    store: &Arc<StoreStack>,
    policy: &Arc<RwLock<AccessPolicy>>,
) -> anyhow::Result<()> {
    let new_config = crate::config::Config::from_file(config_path)?;
    store.reload(&new_config.store).await;
    *policy.write().await = AccessPolicy::new(&new_config.access);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    store: Arc<StoreStack>,
    policy: Arc<RwLock<AccessPolicy>>,
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

        let permitted = policy.read().await.is_permitted(&req.name);
        let result = if permitted {
            let r = store.get(&req.name).await;
            match &r {
                ResponseResult::Ok { .. }    => tracing::info!(secret = req.name.as_str(), "secret served"),
                ResponseResult::NotFound     => tracing::warn!(secret = req.name.as_str(), "secret not found"),
                ResponseResult::Error { error } => tracing::error!(secret = req.name.as_str(), error, "error fetching secret"),
            }
            r
        } else {
            tracing::warn!(secret = req.name.as_str(), "secret access denied by policy");
            ResponseResult::NotFound
        };

        // Collapse any error to notFound before sending to the container.
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
