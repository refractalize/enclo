use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    process::ChildStdin,
    sync::{oneshot, Mutex},
};
use crate::{codec, Request, ResponseResult};

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>>;

pub struct StoreClient {
    writer: Mutex<BufWriter<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
}

impl StoreClient {
    pub fn new(stdin: ChildStdin, pending: PendingMap) -> Arc<Self> {
        Arc::new(Self {
            writer: Mutex::new(BufWriter::new(stdin)),
            pending,
            next_id: AtomicU64::new(0),
        })
    }

    pub async fn get(&self, name: &str) -> Result<String, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = Request { id, name: name.to_string() };
        let bytes = codec::encode(&req).map_err(|e| e.to_string())?;

        {
            let mut w = self.writer.lock().await;
            w.write_all(&bytes).await.map_err(|e| e.to_string())?;
            w.flush().await.map_err(|e| e.to_string())?;
        }

        rx.await.unwrap_or_else(|_| Err("store disconnected".into()))
    }
}

pub fn spawn_reader(
    mut stdout: tokio::process::ChildStdout,
    pending: PendingMap,
) -> tokio::task::JoinHandle<io::Result<()>> {
    tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];

        loop {
            let n = stdout.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);

            loop {
                match codec::decode_one::<crate::Response>(&buf)? {
                    None => break,
                    Some((resp, consumed)) => {
                        buf.drain(..consumed);
                        let mut map = pending.lock().await;
                        if let Some(tx) = map.remove(&resp.id) {
                            let result = match resp.result {
                                ResponseResult::Ok { value } => Ok(value),
                                ResponseResult::Err { error } => Err(error),
                            };
                            let _ = tx.send(result);
                        }
                    }
                }
            }
        }

        let mut map = pending.lock().await;
        for (_, tx) in map.drain() {
            let _ = tx.send(Err("store process exited".into()));
        }

        Ok(())
    })
}
