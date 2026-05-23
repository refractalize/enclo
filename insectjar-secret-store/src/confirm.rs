use std::{sync::Arc, time::Duration};
use tokio::sync::{oneshot, Mutex, RwLock};

type Pending = Vec<(String, oneshot::Sender<Result<(), String>>)>;

pub struct Confirm {
    app_name: String,
    settings: RwLock<Settings>,
    batch: Arc<Mutex<Option<Pending>>>,
}

struct Settings {
    enabled: bool,
    debounce: Duration,
}

impl Confirm {
    pub fn new(app_name: impl Into<String>, enabled: bool, debounce_ms: u64) -> Self {
        Self {
            app_name: app_name.into(),
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
            let app_name = self.app_name.clone();
            tokio::spawn(async move {
                tokio::time::sleep(debounce).await;
                let mut lock = batch_ref.lock().await;
                let pending = lock.take().unwrap_or_default();
                drop(lock);

                let names: Vec<String> = pending.iter().map(|(n, _)| n.clone()).collect();
                tracing::info!(secrets = ?names, "confirmation requested");
                let approved = run_pinentry(&app_name, &names).await;
                if approved {
                    tracing::info!(secrets = ?names, "confirmation approved");
                } else {
                    tracing::info!(secrets = ?names, "confirmation denied");
                }

                for (_, tx) in pending {
                    let _ = tx.send(if approved { Ok(()) } else { Err("denied by user".into()) });
                }
            });
        }

        rx.await.unwrap_or_else(|_| Err("confirm task dropped".into()))
    }
}

async fn run_pinentry(app_name: &str, names: &[String]) -> bool {
    let list = names
        .iter()
        .map(|n| format!("  {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let desc = format!(
        "{app_name} wants to access the following secrets:%0A%0A{}\n\nAllow?",
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
            tracing::error!("failed to spawn pinentry: {e}");
            return false;
        }
    };

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut writer = stdin;
    let mut line = String::new();

    // Read lines from pinentry, skipping Assuan informational "S " lines,
    // until we get the first OK/ERR/other terminal response.
    async fn read_response(reader: &mut tokio::io::BufReader<tokio::process::ChildStdout>, line: &mut String) -> bool {
        loop {
            line.clear();
            if reader.read_line(line).await.is_err() {
                return false;
            }
            if !line.starts_with("S ") {
                return true;
            }
        }
    }

    // Consume the initial greeting.
    if !read_response(&mut reader, &mut line).await {
        return false;
    }

    // Send a command and read the response, skipping informational lines.
    macro_rules! send {
        ($cmd:expr) => {{
            if writer.write_all($cmd.as_bytes()).await.is_err() {
                tracing::warn!("pinentry write error");
                return false;
            }
            if !read_response(&mut reader, &mut line).await {
                tracing::warn!("pinentry read error");
                return false;
            }
        }};
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
