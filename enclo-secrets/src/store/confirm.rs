use std::{sync::Arc, time::Duration};
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};

type Waiter = (String, oneshot::Sender<Result<(), String>>);

struct State {
    /// Accumulating waiters during the debounce window. None when idle or a
    /// dialog is in progress.
    batch: Option<Vec<Waiter>>,
    /// Set while a confirmation dialog is visible. Requests arriving during
    /// this window subscribe to the broadcast instead of starting a new dialog.
    inflight: Option<broadcast::Sender<Result<(), String>>>,
}

struct Inner {
    app_name: String,
    settings: RwLock<Settings>,
    state: Mutex<State>,
}

pub struct Confirm(Arc<Inner>);

struct Settings {
    enabled: bool,
    debounce: Duration,
}

impl Confirm {
    pub fn new(app_name: impl Into<String>, enabled: bool, debounce_ms: u64) -> Self {
        Self(Arc::new(Inner {
            app_name: app_name.into(),
            settings: RwLock::new(Settings {
                enabled,
                debounce: Duration::from_millis(debounce_ms),
            }),
            state: Mutex::new(State { batch: None, inflight: None }),
        }))
    }

    pub async fn reload(&self, enabled: bool, debounce_ms: u64) {
        let mut s = self.0.settings.write().await;
        s.enabled = enabled;
        s.debounce = Duration::from_millis(debounce_ms);
    }

    pub async fn approve(&self, name: String) -> Result<(), String> {
        let (enabled, debounce) = {
            let s = self.0.settings.read().await;
            (s.enabled, s.debounce)
        };

        if !enabled {
            return Ok(());
        }

        let mut state = self.0.state.lock().await;

        // A dialog is already visible — subscribe to its result instead of
        // starting a new one.
        if let Some(tx) = &state.inflight {
            let mut rx = tx.subscribe();
            drop(state);
            return rx.recv().await.unwrap_or_else(|_| Err("confirm dialog dropped".into()));
        }

        // Join the current debounce batch or start a new one.
        let (tx, rx) = oneshot::channel();
        let is_first = state.batch.is_none();
        state.batch.get_or_insert_with(Vec::new).push((name, tx));
        drop(state);

        if is_first {
            let inner = Arc::clone(&self.0);
            tokio::spawn(async move {
                tokio::time::sleep(debounce).await;

                // Take the batch and set inflight atomically.
                let (pending, itx) = {
                    let mut state = inner.state.lock().await;
                    let pending = state.batch.take().unwrap_or_default();
                    let (itx, _) = broadcast::channel(64);
                    state.inflight = Some(itx.clone());
                    (pending, itx)
                };

                let names: Vec<String> = pending.iter().map(|(n, _)| n.clone()).collect();
                tracing::info!(secrets = ?names, "confirmation requested");
                let approved = run_pinentry(&inner.app_name, &names).await;
                if approved {
                    tracing::info!(secrets = ?names, "confirmation approved");
                } else {
                    tracing::info!(secrets = ?names, "confirmation denied");
                }
                let result: Result<(), String> = if approved { Ok(()) } else { Err("denied by user".into()) };

                // Clear inflight before broadcasting so new arrivals start a
                // fresh batch rather than subscribing to a stale channel.
                inner.state.lock().await.inflight = None;

                let _ = itx.send(result.clone());
                for (_, tx) in pending {
                    let _ = tx.send(result.clone());
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
