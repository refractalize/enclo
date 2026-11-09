use crate::{codec, Request, ResponseResult};
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

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>;

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

    pub async fn get(&self, name: &str) -> ResponseResult {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = Request {
            id,
            name: name.to_string(),
        };
        let bytes = match codec::encode(&req) {
            Ok(b) => b,
            Err(e) => {
                return ResponseResult::Error {
                    error: e.to_string(),
                }
            }
        };

        {
            let mut w = self.writer.lock().await;
            let ok = w.write_all(&bytes).await.is_ok() && w.flush().await.is_ok();
            if !ok {
                self.pending.lock().await.remove(&id);
                return ResponseResult::Error {
                    error: "failed to send request to store".into(),
                };
            }
        }

        rx.await.unwrap_or(ResponseResult::Error {
            error: "store disconnected".into(),
        })
    }
}

pub fn spawn_reader(
    mut stdout: tokio::process::ChildStdout,
    pending: PendingMap,
) -> tokio::task::JoinHandle<io::Result<()>> {
    tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];

        let result: io::Result<()> = async {
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
                                let _ = tx.send(resp.result);
                            }
                        }
                    }
                }
            }

            Ok(())
        }
        .await;

        let mut map = pending.lock().await;
        for (_, tx) in map.drain() {
            let _ = tx.send(ResponseResult::Error {
                error: if result.is_err() {
                    "invalid backend response"
                } else {
                    "store process exited"
                }
                .into(),
            });
        }

        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn invalid_expiry_response_releases_waiting_requests() {
        let mut child=tokio::process::Command::new("/bin/sh")
            .args(["-c",r#"printf '%s\n' '{"requestId":1,"status":"ok","value":"dummy","expiresAt":"invalid"}'"#])
            .stdout(std::process::Stdio::piped()).spawn().unwrap();
        let (tx, rx) = oneshot::channel();
        let pending = Arc::new(Mutex::new(HashMap::from([(1, tx)])));
        let reader = spawn_reader(child.stdout.take().unwrap(), pending);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, ResponseResult::Error { .. }));
        assert!(reader.await.unwrap().is_err());
        child.wait().await.unwrap();
    }
}
