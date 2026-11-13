use super::{
    state::{Phase, SessionSnapshot, SharedState},
    transport,
};
use crate::{
    config::Edge,
    protocol::{Features, SessionId},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    sync::{atomic::Ordering, Arc},
    thread::JoinHandle,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Server,
    Client,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub role: Role,
    pub port: u16,
    pub server: String,
    pub edge: Edge,
    pub device: SessionId,
    pub features: Features,
    pub auto_reconnect: bool,
}
impl SessionConfig {
    pub fn validate(&self) -> Result<()> {
        transport::validate_port(self.port)?;
        if self.role == Role::Client {
            transport::parse_address(&self.server)?;
        }
        Ok(())
    }
}
pub enum SessionCommand {
    Stop,
    SetFeatures(Features),
    SetAutoReconnect(bool),
}

pub struct SessionHandle {
    pub kind: Role,
    pub config: SessionConfig,
    pub state: Arc<SharedState>,
    thread: Option<JoinHandle<()>>,
}
impl SessionHandle {
    pub fn start(config: SessionConfig) -> Result<Self> {
        Self::start_with_backend(config, Arc::new(super::runtime::NativeBackend))
    }
    pub fn start_with_backend(
        config: SessionConfig,
        backend: Arc<dyn super::runtime::Backend>,
    ) -> Result<Self> {
        config.validate()?;
        let state = Arc::new(SharedState::new());
        *state.desired.lock().unwrap() = config.features;
        state
            .auto_reconnect
            .store(config.auto_reconnect, Ordering::SeqCst);
        state.features(config.features, true, config.edge);
        state.phase(Phase::Connecting);
        state
            .started_ms
            .store(super::state::now_ms(), Ordering::SeqCst);
        let kind = config.role;
        let saved_config = config.clone();
        let worker_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    super::runtime::run(config, worker_state.clone(), backend)
                }));
                worker_state.ownership(false);
                if worker_state.shutdown.load(Ordering::SeqCst) {
                    worker_state.phase(Phase::Idle);
                } else {
                    match outcome {
                        Ok(Ok(())) => worker_state.phase(Phase::Idle),
                        Ok(Err(e)) => {
                            worker_state.set_error(format!("{e:#}"));
                            worker_state.phase(Phase::Failed);
                        }
                        Err(_) => {
                            worker_state.set_error("Session worker panicked");
                            worker_state.phase(Phase::Failed);
                        }
                    }
                }
            })?;
        Ok(Self {
            kind,
            config: saved_config,
            state,
            thread: Some(thread),
        })
    }
    pub fn command(&self, command: SessionCommand) {
        match command {
            SessionCommand::Stop => {
                self.state.phase(Phase::Stopping);
                self.state.shutdown.store(true, Ordering::SeqCst);
            }
            SessionCommand::SetFeatures(features) => *self.state.desired.lock().unwrap() = features,
            SessionCommand::SetAutoReconnect(enabled) => {
                self.state.auto_reconnect.store(enabled, Ordering::SeqCst)
            }
        }
    }
    pub fn snapshot(&self) -> SessionSnapshot {
        self.state.snapshot()
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub fn reap(&mut self) -> bool {
        if !self.is_finished() {
            return false;
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        true
    }
}
impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        // Deliberately never block a UI destructor. Normal exit waits through
        // is_finished/reap while its event loop stays responsive.
        self.reap();
    }
}
