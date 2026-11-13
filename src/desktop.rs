//! Native desktop integrations. Registration is per user and only changes
//! after an explicit settings command. Tests use isolated registration paths.
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use std::{
    fs::{self, File, TryLockError},
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

#[cfg(target_os = "macos")]
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[cfg(target_os = "macos")]
pub fn launch_agent(executable: &Path) -> String {
    let bundle = executable
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"));
    let args = if let Some(bundle) = bundle {
        vec![
            "/usr/bin/open".into(),
            "-a".into(),
            bundle.to_string_lossy().into_owned(),
            "--args".into(),
            "--autostart".into(),
        ]
    } else {
        vec![
            executable.to_string_lossy().into_owned(),
            "--autostart".into(),
        ]
    };
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.xunfei.mouse-share</string><key>ProgramArguments</key><array>{}</array><key>RunAtLoad</key><true/><key>KeepAlive</key><false/></dict></plist>",args.iter().map(|s|format!("<string>{}</string>",escape(s))).collect::<String>())
}
#[cfg(target_os = "macos")]
fn login_path() -> Result<std::path::PathBuf> {
    Ok(
        std::path::PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?)
            .join("Library/LaunchAgents/com.xunfei.mouse-share.plist"),
    )
}
#[cfg(target_os = "macos")]
pub fn set_login(enabled: bool) -> Result<()> {
    set_login_at(&login_path()?, &std::env::current_exe()?, enabled)
}
#[cfg(target_os = "macos")]
pub fn set_login_at(path: &Path, executable: &Path, enabled: bool) -> Result<()> {
    if enabled {
        fs::create_dir_all(path.parent().context("Invalid login item path")?)?;
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        temp.write_all(launch_agent(executable).as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}
#[cfg(target_os = "macos")]
pub fn login_enabled() -> Result<bool> {
    let path = login_path()?;
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)? == launch_agent(&std::env::current_exe()?))
}
#[cfg(target_os = "windows")]
pub use windows_login::{login_enabled, set_login, set_login_at};

#[cfg(target_os = "windows")]
mod windows_login {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::{w, PCWSTR},
        Win32::{
            Foundation::ERROR_FILE_NOT_FOUND,
            System::Registry::{
                RegCloseKey, RegCreateKeyW, RegDeleteKeyValueW, RegGetValueW, RegSetValueExW, HKEY,
                HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
            },
        },
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: PCWSTR = w!("MouseShare");

    fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain(Some(0)).collect()
    }

    fn command(exe: &Path) -> Vec<u16> {
        let mut command = std::ffi::OsString::from("\"");
        command.push(exe.as_os_str());
        command.push("\" --autostart");
        wide(command)
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }

    pub fn set_login(enabled: bool) -> Result<()> {
        set_login_at(RUN_KEY, &std::env::current_exe()?, enabled)
    }

    pub fn set_login_at(key: &str, exe: &Path, enabled: bool) -> Result<()> {
        let key = wide(key);
        unsafe {
            if !enabled {
                let result = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), VALUE);
                if result != ERROR_FILE_NOT_FOUND {
                    result.ok()?;
                }
                return Ok(());
            }
            let command = command(exe);
            anyhow::ensure!(
                command.len() <= 261,
                "Login command exceeds Windows Run length limit"
            );
            let mut handle = HKEY::default();
            RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), &mut handle).ok()?;
            let handle = Key(handle);
            let bytes: Vec<u8> = command.into_iter().flat_map(u16::to_le_bytes).collect();
            RegSetValueExW(handle.0, VALUE, 0, REG_SZ, Some(&bytes)).ok()?;
        }
        Ok(())
    }

    pub fn login_enabled() -> Result<bool> {
        let key = wide(RUN_KEY);
        let expected = command(&std::env::current_exe()?);
        let mut data = [0u16; 261];
        let mut bytes = (data.len() * 2) as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                VALUE,
                RRF_RT_REG_SZ,
                None,
                Some(data.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        result.ok()?;
        Ok(data.get(..bytes as usize / 2) == Some(expected.as_slice()))
    }
}

/// A file lock owns the instance; authenticated loopback IPC only wakes its
/// window. CLI tools are independent and do not acquire this GUI lock.
pub struct Instance {
    _lock: File,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
pub enum InstanceResult {
    Primary(Instance),
    Existing,
}
impl Instance {
    pub fn acquire(directory: &Path, wake: Arc<dyn Fn() + Send + Sync>) -> Result<InstanceResult> {
        use std::net::{TcpListener, TcpStream};
        use std::time::Duration;
        fs::create_dir_all(directory)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("instance.lock"))?;
        // Windows file locks also block readers. Keep the wake address in a
        // separate file so a second process can read it while the lock is held.
        let address_path = directory.join("instance-wake");
        let mut acquired = false;
        for _ in 0..25 {
            match lock.try_lock() {
                Ok(()) => {
                    acquired = true;
                    break;
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
            if let Ok(address) = fs::read_to_string(&address_path) {
                if let Some((port, token)) = address.trim().split_once(' ') {
                    if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
                        if let Ok(mut stream) =
                            TcpStream::connect_timeout(&addr, Duration::from_millis(25))
                        {
                            stream.set_write_timeout(Some(Duration::from_millis(50)))?;
                            stream.set_read_timeout(Some(Duration::from_millis(75)))?;
                            let mut acknowledged = [0];
                            if stream.write_all(token.as_bytes()).is_ok()
                                && stream.read_exact(&mut acknowledged).is_ok()
                                && acknowledged == [1]
                            {
                                return Ok(InstanceResult::Existing);
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::ensure!(acquired, "Existing GUI instance is not responding");
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let token = crate::net::devices::display_id(crate::net::devices::random()?);
        let mut record = tempfile::NamedTempFile::new_in(directory)?;
        write!(record, "{} {}", listener.local_addr()?.port(), token)?;
        record.flush()?;
        record.persist(address_path)?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = stop.clone();
        let thread = std::thread::Builder::new()
            .name("instance-wake".into())
            .spawn(move || {
                while !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Ok((mut s, _)) = listener.accept() {
                        let _ = s.set_read_timeout(Some(Duration::from_millis(50)));
                        let mut bytes = [0; 32];
                        if s.read_exact(&mut bytes).is_ok() && bytes == token.as_bytes() {
                            wake();
                            let _ = s.set_write_timeout(Some(Duration::from_millis(50)));
                            let _ = s.write_all(&[1]);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })?;
        Ok(InstanceResult::Primary(Self {
            _lock: lock,
            stop,
            thread: Some(thread),
        }))
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
