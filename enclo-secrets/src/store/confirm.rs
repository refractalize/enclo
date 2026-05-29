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

#[cfg(target_os = "macos")]
async fn run_pinentry(app_name: &str, names: &[String]) -> bool {
    let app_name = app_name.to_string();
    let names = names.to_vec();
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("enclo-secret-approve-macos");
        cmd.arg(&app_name);
        for name in &names {
            cmd.arg(name);
        }
        match cmd.status() {
            Ok(s) => match s.code() {
                Some(0) => { tracing::info!("enclo-secret-approve-macos: approved"); true }
                Some(1) => { tracing::info!("enclo-secret-approve-macos: denied"); false }
                code => {
                    tracing::error!("enclo-secret-approve-macos: unexpected exit {:?}", code);
                    false
                }
            },
            Err(e) => {
                tracing::error!("enclo-secret-approve-macos: failed to run: {e}");
                false
            }
        }
    })
    .await
    .unwrap_or_else(|_| { tracing::error!("enclo-secret-approve-macos task panicked"); false })
}

#[cfg(not(target_os = "macos"))]
async fn run_pinentry(app_name: &str, names: &[String]) -> bool {
    let list = names.iter().map(|n| format!("  {n}")).collect::<Vec<_>>().join("\n");
    let query = format!("{app_name} wants to access the following secrets:\n\n{list}\n\nAllow?");
    tokio::task::spawn_blocking(move || {
        let mut dialog = match pinentry::ConfirmationDialog::with_default_binary() {
            Some(d) => d,
            None => {
                tracing::error!("pinentry binary not found");
                return false;
            }
        };
        dialog.with_ok("Allow").with_cancel("Deny");
        match dialog.confirm(&query) {
            Ok(true) => { tracing::info!("pinentry: approved"); true }
            Ok(false) => { tracing::info!("pinentry: denied (cancelled)"); false }
            Err(e) => { tracing::info!("pinentry: denied ({})", e); false }
        }
    })
    .await
    .unwrap_or_else(|_| { tracing::error!("pinentry task panicked"); false })
}
