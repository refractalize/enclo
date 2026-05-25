use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use insectjar_proxy_store_common::{client as proto_client, codec, Request, ResponseResult};
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
    inner: Mutex<Inner>,
    pending: PendingMap,
    next_id: AtomicU64,
    restart_times: Mutex<VecDeque<Instant>>,
    shutdown: Arc<Notify>,
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
            inner: Mutex::new(Inner { child, writer }),
            pending,
            next_id: AtomicU64::new(0),
            restart_times: Mutex::new(VecDeque::new()),
            shutdown,
        });

        start_watching(Arc::clone(&client), stdout);
        Ok(client)
    }

    pub async fn get(&self, name: &str) -> ResponseResult {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = Request { id, name: name.to_string() };
        let bytes = match codec::encode(&req) {
            Ok(b) => b,
            Err(e) => return ResponseResult::Error { error: e.to_string() },
        };

        {
            let mut inner = self.inner.lock().await;
            let write_ok = inner.writer.write_all(&bytes).await.is_ok()
                && inner.writer.flush().await.is_ok();
            if !write_ok {
                self.pending.lock().await.remove(&id);
                return ResponseResult::Error { error: "failed to send request to backend process".into() };
            }
        }

        rx.await.unwrap_or(ResponseResult::Error { error: "backend disconnected".into() })
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
                inner.child = child;
                inner.writer = writer;
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
    let mut child = Command::new(binary)
        .args(args)
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let stdin = child.stdin.take().expect("stdin not captured");
    let stdout = child.stdout.take().expect("stdout not captured");
    Ok((child, BufWriter::new(stdin), stdout))
}
