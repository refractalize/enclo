use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use enclo_backend_protocol::{client as proto_client, codec, Request, ResponseResult};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{oneshot, Mutex, Notify},
};

const MAX_RESTARTS: usize = 10;
const RESTART_WINDOW: Duration = Duration::from_secs(60);

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>;

struct Inner {
    child: Child,
    writer: BufWriter<ChildStdin>,
}

pub struct BackendClient {
    binary: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    inner: Mutex<Option<Inner>>,
    pending: PendingMap,
    next_id: AtomicU64,
    restart_times: Mutex<VecDeque<Instant>>,
    shutdown: Arc<Notify>,
    stopping: AtomicBool,
}

impl BackendClient {
    pub async fn spawn(
        binary: &str,
        args: &[String],
        env: &HashMap<String, String>,
        shutdown: Arc<Notify>,
    ) -> anyhow::Result<Arc<Self>> {
        let (child, writer, stdout) = spawn_process(binary, args, env).await?;
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        let client = Arc::new(Self {
            binary: binary.to_string(),
            args: args.to_vec(),
            env: env.clone(),
            inner: Mutex::new(Some(Inner { child, writer })),
            pending,
            next_id: AtomicU64::new(0),
            restart_times: Mutex::new(VecDeque::new()),
            shutdown,
            stopping: AtomicBool::new(false),
        });

        start_watching(Arc::clone(&client), stdout);
        Ok(client)
    }

    pub async fn get(&self, name: &str) -> ResponseResult {
        if self.stopping.load(Ordering::Acquire) {
            return ResponseResult::Error {
                error: format!("backend '{}' is shutting down", self.binary),
            };
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = Request {
            id,
            name: name.to_string(),
        };
        let bytes = match codec::encode(&req) {
            Ok(b) => b,
            Err(e) => return ResponseResult::Error { error: e.to_string() },
        };

        let send_result = {
            let mut inner = self.inner.lock().await;
            match inner.as_mut() {
                Some(inner) => {
                    let write_ok = inner.writer.write_all(&bytes).await.is_ok()
                        && inner.writer.flush().await.is_ok();
                    if write_ok { Ok(()) } else { Err("failed to send request to backend process".to_string()) }
                }
                None => Err("backend process is not running".to_string()),
            }
        };

        if let Err(error) = send_result {
            self.pending.lock().await.remove(&id);
            return ResponseResult::Error { error };
        }

        rx.await.unwrap_or(ResponseResult::Error {
            error: "backend disconnected".into(),
        })
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);

        let mut inner = self.inner.lock().await;
        if let Some(mut inner) = inner.take() {
            let _ = inner.child.kill().await;
            let _ = inner.child.wait().await;
        }
    }
}

fn start_watching(client: Arc<BackendClient>, stdout: ChildStdout) {
    tokio::spawn(reader_loop(client, stdout));
}

async fn reader_loop(client: Arc<BackendClient>, stdout: ChildStdout) {
    proto_client::spawn_reader(stdout, Arc::clone(&client.pending))
        .await
        .ok();
    on_exit(client).await;
}

async fn on_exit(client: Arc<BackendClient>) {
    if client.stopping.load(Ordering::Acquire) {
        client.inner.lock().await.take();
        return;
    }

    {
        let mut times = client.restart_times.lock().await;
        let now = Instant::now();
        times.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        times.push_back(now);
        if times.len() > MAX_RESTARTS {
            tracing::error!(
                binary = client.binary.as_str(),
                "backend restarted >{MAX_RESTARTS} times in {RESTART_WINDOW:?}, shutting down"
            );
            client.shutdown.notify_waiters();
            return;
        }
    }

    tracing::warn!(binary = client.binary.as_str(), "backend exited, restarting");

    match spawn_process(&client.binary, &client.args, &client.env).await {
        Err(e) => {
            tracing::error!(binary = client.binary.as_str(), "failed to restart backend: {e}");
            client.shutdown.notify_waiters();
        }
        Ok((child, writer, stdout)) => {
            {
                let mut inner = client.inner.lock().await;
                if client.stopping.load(Ordering::Acquire) {
                    let mut child = child;
                    drop(writer);
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return;
                }
                *inner = Some(Inner { child, writer });
            }
            start_watching(client, stdout);
        }
    }
}

async fn spawn_process(
    binary: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> anyhow::Result<(Child, BufWriter<ChildStdin>, ChildStdout)> {
    tracing::debug!(binary, "spawning backend process");
    let mut child = Command::new(binary)
        .args(args)
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn backend '{}': {e}", binary))?;

    let stdin = child.stdin.take().expect("stdin not captured");
    let stdout = child.stdout.take().expect("stdout not captured");
    Ok((child, BufWriter::new(stdin), stdout))
}
