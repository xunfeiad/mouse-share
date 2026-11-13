//! Versioned, per-user preferences. Loading never installs a login item.
use crate::{
    config::Edge,
    net::session::Role,
    protocol::{Features, SessionId},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub device: SessionId,
    pub language: String,
    pub port: u16,
    pub edge: Edge,
    pub server_host: String,
    pub server_port: u16,
    pub features: Features,
    pub auto_reconnect: bool,
    pub debug_logging: bool,
    pub start_on_login: bool,
    pub show_in_menubar: bool,
    pub last_role: Option<Role>,
    pub last_session: Option<crate::net::session::SessionConfig>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            device: crate::protocol::new_id(),
            language: String::new(),
            port: 4242,
            edge: Edge::Right,
            server_host: "192.168.1.100".into(),
            server_port: 4242,
            features: Features::default(),
            auto_reconnect: true,
            debug_logging: false,
            start_on_login: false,
            show_in_menubar: true,
            last_role: None,
            last_session: None,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == 1, "Unsupported settings version");
        crate::net::transport::validate_port(self.port)?;
        crate::net::transport::validate_port(self.server_port)?;
        if let Some(config) = &self.last_session {
            config.validate()?;
        }
        anyhow::ensure!(
            matches!(self.language.as_str(), "" | "en" | "zh"),
            "Invalid language"
        );
        Ok(())
    }
}
pub fn directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MOUSE_SHARE_CONFIG_DIR") {
        return Ok(path.into());
    }
    #[cfg(target_os = "macos")]
    {
        Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?)
                .join("Library/Application Support/com.xunfei.mouse-share"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        Ok(
            PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is unavailable")?)
                .join("MouseShare"),
        )
    }
}
pub fn load(path: &Path) -> Result<(Settings, Option<String>)> {
    if !path.exists() {
        return Ok((Settings::default(), None));
    }
    let bytes = fs::read(path)?;
    let parsed = serde_json::from_slice::<Settings>(&bytes)
        .map_err(anyhow::Error::from)
        .and_then(|s| {
            s.validate()?;
            Ok(s)
        });
    match parsed {
        Ok(settings) => Ok((settings, None)),
        Err(error) => {
            let backup =
                path.with_extension(format!("corrupt-{}.json", crate::net::state::now_ms()));
            fs::rename(path, &backup).context("Cannot preserve invalid settings")?;
            Ok((
                Settings::default(),
                Some(format!(
                    "Settings restored to defaults ({error}); backup: {}",
                    backup.display()
                )),
            ))
        }
    }
}
pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    settings.validate()?;
    let parent = path.parent().context("Invalid settings path")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".config-{}-{}.tmp",
        std::process::id(),
        crate::net::state::now_ms()
    ));
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        f.write_all(&serde_json::to_vec_pretty(settings)?)?;
        f.sync_all()?;
        drop(f);
        replace(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
#[cfg(not(target_os = "windows"))]
fn replace(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    Ok(())
}
#[cfg(target_os = "windows")]
fn replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(a.as_ptr()),
            PCWSTR(b.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}
