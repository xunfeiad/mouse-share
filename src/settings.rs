//! Versioned desktop preferences. Paired device identities, credentials and
//! sharing preferences remain in the QUIC device store.
use crate::{config::Edge, net::devices::DeviceId};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SavedSession {
    Server { port: u16, edge: Edge },
    Client { device: DeviceId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub language: String,
    pub port: u16,
    pub edge: Edge,
    pub auto_reconnect: bool,
    pub debug_logging: bool,
    pub start_on_login: bool,
    pub show_in_menubar: bool,
    pub last_success: Option<SavedSession>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            language: String::new(),
            port: 4242,
            edge: Edge::Right,
            auto_reconnect: true,
            debug_logging: false,
            start_on_login: false,
            show_in_menubar: true,
            last_success: None,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == 1, "Unsupported preferences version");
        anyhow::ensure!(
            matches!(self.language.as_str(), "" | "en" | "zh"),
            "Invalid language"
        );
        anyhow::ensure!(
            self.port != 0
                && !matches!(
                    self.last_success,
                    Some(SavedSession::Server { port: 0, .. })
                ),
            "UDP port must be between 1 and 65535"
        );
        Ok(())
    }
}

pub fn directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MOUSE_SHARE_CONFIG_DIR") {
        return Ok(path.into());
    }
    #[cfg(target_os = "macos")]
    let dir = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?)
        .join("Library/Application Support/com.xunfei.mouse-share");
    #[cfg(target_os = "windows")]
    let dir = PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is unavailable")?)
        .join("MouseShare");
    Ok(dir)
}

pub fn load(path: &Path) -> Result<(Settings, Option<String>)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Settings::default(), None))
        }
        Err(e) => return Err(e).context("Cannot read preferences"),
    };
    match serde_json::from_slice::<Settings>(&bytes)
        .map_err(anyhow::Error::from)
        .and_then(|s| {
            s.validate()?;
            Ok(s)
        }) {
        Ok(s) => Ok((s, None)),
        Err(error) => {
            let suffix = crate::net::devices::display_id(crate::net::devices::random()?);
            let backup = path.with_extension(format!("corrupt-{suffix}.json"));
            fs::rename(path, &backup).context("Cannot preserve invalid preferences")?;
            Ok((
                Settings::default(),
                Some(format!(
                    "Restored default preferences ({error}). Backup: {}",
                    backup.display()
                )),
            ))
        }
    }
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    settings.validate()?;
    let parent = path.parent().context("Invalid preferences path")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, settings)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}
