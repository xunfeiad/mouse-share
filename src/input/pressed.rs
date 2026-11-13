//! Only releases input this session successfully injected. The guard remains
//! alive until after networking has stopped, including during unwinding.
use super::simulate::InputSimulator;
use crate::protocol::{KeyEvent, MouseButton};
use std::collections::{HashMap, HashSet};

pub struct PressedInputState {
    pub simulator: Box<dyn InputSimulator>,
    buttons: HashSet<MouseButton>,
    keys: HashMap<u32, u64>,
}
impl PressedInputState {
    pub fn new(simulator: Box<dyn InputSimulator>) -> Self {
        Self {
            simulator,
            buttons: HashSet::new(),
            keys: HashMap::new(),
        }
    }
    pub fn button(&mut self, button: MouseButton, down: bool) -> anyhow::Result<()> {
        if down {
            self.simulator.button_down(button)?;
            self.buttons.insert(button);
        } else if self.buttons.contains(&button) {
            self.simulator.button_up(button)?;
            self.buttons.remove(&button);
        }
        Ok(())
    }
    pub fn key(&mut self, key: &KeyEvent) -> anyhow::Result<()> {
        if key.down {
            self.simulator.key_event(key.keycode, true, key.flags)?;
            self.keys.insert(key.keycode, key.flags);
        } else if self.keys.contains_key(&key.keycode) {
            self.simulator.key_event(key.keycode, false, key.flags)?;
            self.keys.remove(&key.keycode);
        }
        Ok(())
    }
    pub fn release_keys(&mut self) {
        // Remove successful releases only, so Drop can retry failures.
        for code in self.keys.keys().copied().collect::<Vec<_>>() {
            let flags = if cfg!(target_os = "windows") {
                self.keys[&code]
            } else {
                0
            };
            match self.simulator.key_event(code, false, flags) {
                Ok(()) => {
                    self.keys.remove(&code);
                }
                Err(e) => log::error!("Failed to release key {code}: {e}"),
            }
        }
    }
    pub fn release_all(&mut self) {
        self.release_keys();
        for button in self.buttons.iter().copied().collect::<Vec<_>>() {
            match self.simulator.button_up(button) {
                Ok(()) => {
                    self.buttons.remove(&button);
                }
                Err(e) => log::error!("Failed to release mouse button: {e}"),
            }
        }
    }
}
impl Drop for PressedInputState {
    fn drop(&mut self) {
        self.release_all();
    }
}
