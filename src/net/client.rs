//! Compatibility constructor; GUI and CLI use SessionHandle directly.
use std::sync::Arc;
pub struct Client {
    server_addr: String,
}
impl Client {
    pub fn new(server_addr: String) -> Self {
        Self { server_addr }
    }
    pub fn run(&self, state: Arc<super::state::SharedState>) -> anyhow::Result<()> {
        let config = super::session::SessionConfig {
            role: super::session::Role::Client,
            port: 4242,
            server: self.server_addr.clone(),
            edge: crate::config::Edge::Right,
            device: crate::protocol::new_id(),
            features: *state.desired.lock().unwrap(),
            auto_reconnect: true,
        };
        config.validate()?;
        super::runtime::run(config, state, Arc::new(super::runtime::NativeBackend))
    }
}
