use std::{sync::Arc, time::Duration};
use tokio::sync::{oneshot, Mutex, RwLock};

type Pending = Vec<(String, oneshot::Sender<Result<(), String>>)>;

pub struct Confirm {
    settings: RwLock<Settings>,
    batch: Arc<Mutex<Option<Pending>>>,
}

struct Settings {
    enabled: bool,
    debounce: Duration,
}

impl Confirm {
    pub fn new(enabled: bool, debounce_ms: u64) -> Self {
        Self {
            settings: RwLock::new(Settings {
                enabled,
                debounce: Duration::from_millis(debounce_ms),
            }),
            batch: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn reload(&self, enabled: bool, debounce_ms: u64) {
        let mut s = self.settings.write().await;
        s.enabled = enabled;
        s.debounce = Duration::from_millis(debounce_ms);
    }

    pub async fn approve(&self, name: String) -> Result<(), String> {
        let (enabled, debounce) = {
            let s = self.settings.read().await;
            (s.enabled, s.debounce)
        };

        if !enabled {
            return Ok(());
        }

        let (tx, rx) = oneshot::channel();

        let mut batch_lock = self.batch.lock().await;
        let is_first = batch_lock.is_none();
        batch_lock.get_or_insert_with(Vec::new).push((name, tx));
        drop(batch_lock);

        if is_first {
            let batch_ref = Arc::clone(&self.batch);
            tokio::spawn(async move {
                tokio::time::sleep(debounce).await;
                let mut lock = batch_ref.lock().await;
                let pending = lock.take().unwrap_or_default();
                drop(lock);

                let names: Vec<String> = pending.iter().map(|(n, _)| n.clone()).collect();
                let approved = run_pinentry(&names).await;

                for (_, tx) in pending {
                    let _ = tx.send(if approved { Ok(()) } else { Err("denied by user".into()) });
                }
            });
        }

        rx.await.unwrap_or_else(|_| Err("confirm task dropped".into()))
    }
}

async fn run_pinentry(names: &[String]) -> bool {
    let list = names
        .iter()
        .map(|n| format!("  {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let desc = format!(
        "insectjar-proxy wants to access the following secrets:%0A%0A{}\n\nAllow?",
        list.replace(' ', "%20").replace('\n', "%0A")
    );

    let mut child = match tokio::process::Command::new("pinentry")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("insectjar-proxy: failed to spawn pinentry: {e}");
            return false;
        }
    };

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut writer = stdin;
    let mut line = String::new();

    macro_rules! send {
        ($cmd:expr) => {{
            if let Err(e) = writer.write_all($cmd.as_bytes()).await {
                eprintln!("insectjar-proxy: pinentry write error: {e}");
                return false;
            }
            line.clear();
            if let Err(e) = reader.read_line(&mut line).await {
                eprintln!("insectjar-proxy: pinentry read error: {e}");
                return false;
            }
        }};
    }

    line.clear();
    if reader.read_line(&mut line).await.is_err() {
        return false;
    }

    send!(format!("SETDESC {desc}\n"));
    send!("SETOK Allow\n");
    send!("SETCANCEL Deny\n");
    send!("CONFIRM\n");

    let approved = line.starts_with("OK");

    send!("BYE\n");
    let _ = child.wait().await;

    approved
}
