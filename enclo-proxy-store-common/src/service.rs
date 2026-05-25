use std::sync::Arc;

use tokio::{
    signal::unix::{signal, SignalKind},
    sync::Notify,
    task::JoinHandle,
};

pub fn spawn_shutdown_listener(shutdown: Arc<Notify>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        shutdown.notify_waiters();
    })
}
