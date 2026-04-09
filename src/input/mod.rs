pub mod capture;
pub mod simulate;

#[cfg(target_os = "macos")]
pub mod macos_capture;
#[cfg(target_os = "macos")]
pub mod macos_simulate;

#[cfg(target_os = "windows")]
pub mod win_capture;
#[cfg(target_os = "windows")]
pub mod win_simulate;
