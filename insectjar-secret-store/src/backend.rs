use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use insectjar_proxy_store_common::{client as proto_client, codec, Request};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{oneshot, Mutex, Notify},
};

const MAX_RESTARTS: usize = 10;
const RESTART_WINDOW: Duration = Duration::from_secs(60);

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>>;

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

    pub async fn get(&self, name: &str) -> Result<String, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = Request { id, name: name.to_string() };
        let bytes = codec::encode(&req).map_err(|e| e.to_string())?;

        {
            let mut inner = self.inner.lock().await;
            if inner.writer.write_all(&bytes).await.is_err()
                || inner.writer.flush().await.is_err()
            {
                // Error surfaced via the pending channel when the reader task exits.
            }
        }

        rx.await.unwrap_or_else(|_| Err("store disconnected".into()))
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
            eprintln!(
                "insectjar: backend '{}' restarted >{} times in {:?}, shutting down",
                client.binary, MAX_RESTARTS, RESTART_WINDOW
            );
            client.shutdown.notify_waiters();
            return;
        }
    }

    eprintln!("insectjar: backend '{}' exited, restarting", client.binary);

    match spawn_process(&client.binary, &client.args, &client.env).await {
        Err(e) => {
            eprintln!("insectjar: failed to restart '{}': {e}", client.binary);
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
