//! Three independent lifetimes: machine, forwarded services, and shell sessions.
//!
//! Drivers own the actual processes. This coordinator owns their ordering and
//! never stops a machine implicitly when a session or services stop.
use anyhow::{ensure, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineOwnership {
    Managed,
    External,
}

#[async_trait]
pub trait Machine: Send {
    fn ownership(&self) -> MachineOwnership;
    /// Start a managed machine if necessary, or check an external SSH target.
    /// Return only when the target is accessible. Do not provision external hosts.
    async fn ensure_available(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
}

#[async_trait]
pub trait Services: Send {
    /// Establish services and their transport, returning only when ready.
    /// Must be idempotent and check/recover an existing transport on every call.
    /// On error, roll back resources created by this call before returning.
    async fn ensure_ready(&mut self) -> Result<()>;
    /// Stop all owned services and tunnels; must support retries after failure.
    async fn stop(&mut self) -> Result<()>;
    /// Wait for transport loss. Used by foreground supervisors.
    async fn wait(&mut self) -> Result<()>;
}

#[async_trait]
pub trait Sessions: Send {
    type Handle: Send;
    /// Open one shell connection; do not own the services transport.
    /// On failure, clean up any partially created session.
    async fn open(&mut self) -> Result<Self::Handle>;
    /// Close and reap a shell connection. Also succeeds if it already exited.
    /// A failed close retains the handle so callers can retry.
    async fn close(&mut self, handle: &mut Self::Handle) -> Result<()>;
    /// Wait for natural exit and return the shell's exit code.
    async fn wait(&mut self, handle: &mut Self::Handle) -> Result<i32>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId(u64);

/// Serializes lifecycle operations. Session handles remain tracked until closed,
/// including sessions that have exited naturally and still need to be reaped.
/// Explicit shutdown is required; dropping this object does not stop a machine.
pub struct Environment<M, V, S: Sessions> {
    machine: M,
    services: V,
    sessions: S,
    active: BTreeMap<SessionId, S::Handle>,
    next_id: u64,
}

impl<M: Machine, V: Services, S: Sessions> Environment<M, V, S> {
    pub fn new(machine: M, services: V, sessions: S) -> Self {
        Self {
            machine,
            services,
            sessions,
            active: BTreeMap::new(),
            next_id: 0,
        }
    }

    pub fn session_count(&self) -> usize {
        self.active.len()
    }

    /// Establish only the upper lifetime; no credential services are exposed.
    pub async fn start_machine(&mut self) -> Result<()> {
        self.machine.ensure_available().await
    }

    pub async fn start_services(&mut self) -> Result<()> {
        self.start_machine().await?;
        self.services.ensure_ready().await
    }

    pub async fn wait_services(&mut self) -> Result<()> {
        self.services.wait().await
    }

    pub async fn wait_session(&mut self, id: SessionId) -> Result<i32> {
        let handle = self
            .active
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown session"))?;
        let code = self.sessions.wait(handle).await?;
        self.active.remove(&id);
        Ok(code)
    }

    pub async fn open_session(&mut self) -> Result<SessionId> {
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("session IDs exhausted"))?;
        self.start_services().await?;
        let handle = self.sessions.open().await?;
        let id = SessionId(self.next_id);
        self.next_id = next_id;
        self.active.insert(id, handle);
        Ok(id)
    }

    /// Closing the last shell leaves the services and machine running.
    pub async fn close_session(&mut self, id: SessionId) -> Result<()> {
        if let Some(handle) = self.active.get_mut(&id) {
            self.sessions.close(handle).await?;
            self.active.remove(&id);
        }
        Ok(())
    }

    /// Services may stop while shells keep running without their capabilities.
    pub async fn stop_services(&mut self) -> Result<()> {
        self.services.stop().await
    }

    /// Explicitly close all tracked shells, retaining failed handles for retry.
    pub async fn close_sessions(&mut self) -> Result<()> {
        let mut first_error = None;
        for id in self.active.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = self.close_session(id).await {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    /// Stop dependencies in reverse order. External machines are never stopped,
    /// and rejecting that request does not disturb their services or sessions.
    pub async fn stop_machine(&mut self, force: bool) -> Result<()> {
        ensure!(
            self.machine.ownership() == MachineOwnership::Managed,
            "machine lifecycle is externally managed; disconnect services instead"
        );
        ensure!(
            force || self.active.is_empty(),
            "shell sessions are still active"
        );
        self.close_sessions().await?;
        self.stop_services().await?;
        self.machine.stop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct State {
        events: Vec<&'static str>,
        fail: Option<&'static str>,
    }

    #[derive(Clone)]
    struct Driver(Arc<Mutex<State>>, MachineOwnership);

    impl Driver {
        fn event(&self, event: &'static str) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.events.push(event);
            ensure!(state.fail != Some(event), "injected failure: {event}");
            Ok(())
        }
    }

    #[async_trait]
    impl Machine for Driver {
        fn ownership(&self) -> MachineOwnership {
            self.1
        }
        async fn ensure_available(&mut self) -> Result<()> {
            self.event("machine ready")
        }
        async fn stop(&mut self) -> Result<()> {
            self.event("machine stop")
        }
    }

    #[async_trait]
    impl Services for Driver {
        async fn wait(&mut self) -> Result<()> {
            std::future::pending().await
        }
        async fn ensure_ready(&mut self) -> Result<()> {
            self.event("services ready")
        }
        async fn stop(&mut self) -> Result<()> {
            self.event("services stop")
        }
    }

    #[async_trait]
    impl Sessions for Driver {
        type Handle = ();
        async fn wait(&mut self, _: &mut ()) -> Result<i32> {
            self.event("shell wait")?;
            Ok(0)
        }
        async fn open(&mut self) -> Result<()> {
            self.event("shell open")
        }
        async fn close(&mut self, _: &mut ()) -> Result<()> {
            self.event("shell close")
        }
    }

    fn fixture(
        ownership: MachineOwnership,
    ) -> (Environment<Driver, Driver, Driver>, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State::default()));
        let driver = Driver(state.clone(), ownership);
        (
            Environment::new(driver.clone(), driver.clone(), driver),
            state,
        )
    }

    fn take_events(state: &Arc<Mutex<State>>) -> Vec<&'static str> {
        std::mem::take(&mut state.lock().unwrap().events)
    }

    #[tokio::test]
    async fn shell_exit_leaves_upper_lifetimes_running() {
        let (mut env, state) = fixture(MachineOwnership::Managed);
        let first = env.open_session().await.unwrap();
        assert_eq!(
            take_events(&state),
            ["machine ready", "services ready", "shell open"]
        );
        let second = env.open_session().await.unwrap();
        assert_ne!(first, second);
        assert_eq!(env.session_count(), 2);
        take_events(&state);
        env.close_session(first).await.unwrap();
        env.close_session(second).await.unwrap();
        env.close_session(second).await.unwrap();
        assert_eq!(env.session_count(), 0);
        assert_eq!(take_events(&state), ["shell close", "shell close"]);
        env.stop_services().await.unwrap();
        assert_eq!(take_events(&state), ["services stop"]);
    }

    #[tokio::test]
    async fn natural_exit_reaps_only_that_session() {
        let (mut env, state) = fixture(MachineOwnership::Managed);
        let session = env.open_session().await.unwrap();
        take_events(&state);
        assert_eq!(env.wait_session(session).await.unwrap(), 0);
        assert_eq!(env.session_count(), 0);
        assert_eq!(take_events(&state), ["shell wait"]);
    }

    #[tokio::test]
    async fn services_can_stop_and_restart_without_closing_shells() {
        let (mut env, state) = fixture(MachineOwnership::Managed);
        let session = env.open_session().await.unwrap();
        take_events(&state);
        env.stop_services().await.unwrap();
        assert_eq!(env.session_count(), 1);
        assert_eq!(take_events(&state), ["services stop"]);
        env.start_services().await.unwrap();
        assert_eq!(env.session_count(), 1);
        assert_eq!(take_events(&state), ["machine ready", "services ready"]);
        env.close_session(session).await.unwrap();
        assert_eq!(take_events(&state), ["shell close"]);
    }

    #[tokio::test]
    async fn shutdown_requires_force_with_shells_and_runs_in_reverse_order() {
        let (mut env, state) = fixture(MachineOwnership::Managed);
        env.open_session().await.unwrap();
        take_events(&state);
        assert!(env.stop_machine(false).await.is_err());
        assert!(take_events(&state).is_empty());
        env.stop_machine(true).await.unwrap();
        assert_eq!(
            take_events(&state),
            ["shell close", "services stop", "machine stop"]
        );
    }

    #[tokio::test]
    async fn external_machine_stop_is_rejected_without_side_effects() {
        let (mut env, state) = fixture(MachineOwnership::External);
        env.open_session().await.unwrap();
        take_events(&state);
        assert!(env.stop_machine(true).await.is_err());
        assert!(take_events(&state).is_empty());
        env.stop_services().await.unwrap();
        assert_eq!(take_events(&state), ["services stop"]);
        assert_eq!(env.session_count(), 1);
    }

    #[tokio::test]
    async fn startup_failure_does_not_open_shell_or_stop_machine() {
        for (failure, expected) in [
            ("machine ready", vec!["machine ready"]),
            ("services ready", vec!["machine ready", "services ready"]),
            (
                "shell open",
                vec!["machine ready", "services ready", "shell open"],
            ),
        ] {
            let (mut env, state) = fixture(MachineOwnership::Managed);
            state.lock().unwrap().fail = Some(failure);
            assert!(env.open_session().await.is_err());
            assert_eq!(env.session_count(), 0);
            assert_eq!(take_events(&state), expected);
            state.lock().unwrap().fail = None;
            env.open_session().await.unwrap();
            assert_eq!(env.session_count(), 1);
        }
    }

    #[tokio::test]
    async fn failed_shell_cleanup_preserves_handles_and_dependencies_for_retry() {
        let (mut env, state) = fixture(MachineOwnership::Managed);
        env.open_session().await.unwrap();
        env.open_session().await.unwrap();
        take_events(&state);
        state.lock().unwrap().fail = Some("shell close");
        assert!(env.stop_machine(true).await.is_err());
        assert_eq!(take_events(&state), ["shell close", "shell close"]);
        assert_eq!(env.session_count(), 2);
        state.lock().unwrap().fail = None;
        env.stop_machine(true).await.unwrap();
        assert_eq!(env.session_count(), 0);
        assert_eq!(
            take_events(&state),
            [
                "shell close",
                "shell close",
                "services stop",
                "machine stop"
            ]
        );
    }

    #[tokio::test]
    async fn services_failure_blocks_machine_shutdown_and_can_be_retried() {
        let (mut env, state) = fixture(MachineOwnership::Managed);
        env.start_services().await.unwrap();
        take_events(&state);
        state.lock().unwrap().fail = Some("services stop");
        assert!(env.stop_machine(false).await.is_err());
        assert_eq!(take_events(&state), ["services stop"]);
        state.lock().unwrap().fail = None;
        env.stop_machine(false).await.unwrap();
        assert_eq!(take_events(&state), ["services stop", "machine stop"]);
    }
}
