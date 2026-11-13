//! Compatibility constructor; GUI and CLI use SessionHandle directly.
use std::sync::Arc;
pub struct Server {
    port: u16,
    edge: crate::config::Edge,
}
impl Server {
    pub fn new(port: u16, edge: crate::config::Edge) -> Self {
        Self { port, edge }
    }
    pub fn run(&self, state: Arc<super::state::SharedState>) -> anyhow::Result<()> {
        let config = super::session::SessionConfig {
            role: super::session::Role::Server,
            port: self.port,
            server: String::new(),
            edge: self.edge,
            device: crate::protocol::new_id(),
            features: *state.desired.lock().unwrap(),
            auto_reconnect: true,
        };
        config.validate()?;
        super::runtime::run(config, state, Arc::new(super::runtime::NativeBackend))
    }
}
