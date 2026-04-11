//! Library crate exposing the shared backend (networking, input capture,
//! configuration) so both the CLI binary (`src/main.rs`) and the GUI
//! binary (`src/bin/ui.rs`) can consume it.

pub mod clipboard;
pub mod config;
pub mod input;
pub mod log_buffer;
pub mod net;
pub mod protocol;
pub mod screen;
