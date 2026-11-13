//! Native desktop integrations. Registration is per user and only changes
//! after an explicit settings command. Tests use isolated registration paths.
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use fs2::FileExt;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
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
        let temp = path.with_extension("plist.tmp");
        fs::write(&temp, launch_agent(executable))?;
        fs::rename(temp, path)?;
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
fn reg(args: &[&str]) -> Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    Ok(std::process::Command::new("reg.exe")
        .args(args)
        .creation_flags(0x08000000)
        .output()?)
}
#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
fn login_command() -> Result<String> {
    Ok(login_command_at(&std::env::current_exe()?))
}
#[cfg(target_os = "windows")]
fn login_command_at(executable: &Path) -> String {
    format!("\"{}\" --autostart", executable.display())
}
#[cfg(target_os = "windows")]
pub fn set_login(enabled: bool) -> Result<()> {
    set_login_at(RUN_KEY, &std::env::current_exe()?, enabled)
}
#[cfg(target_os = "windows")]
pub fn set_login_at(key: &str, executable: &Path, enabled: bool) -> Result<()> {
    let output = if enabled {
        let command = login_command_at(executable);
        anyhow::ensure!(
            command.encode_utf16().count() <= 260,
            "Login command exceeds Windows Run length limit"
        );
        reg(&[
            "add",
            key,
            "/v",
            "MouseShare",
            "/t",
            "REG_SZ",
            "/d",
            &command,
            "/f",
        ])?
    } else {
        if !reg(&["query", key, "/v", "MouseShare"])?.status.success() {
            return Ok(());
        }
        reg(&["delete", key, "/v", "MouseShare", "/f"])?
    };
    anyhow::ensure!(
        output.status.success(),
        "Login item update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
#[cfg(target_os = "windows")]
pub fn login_enabled() -> Result<bool> {
    let o = reg(&["query", RUN_KEY, "/v", "MouseShare"])?;
    Ok(o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&login_command()?))
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
        let path = directory.join("instance.lock");
        let mut lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if let Err(e) = lock.try_lock_exclusive() {
            anyhow::ensure!(
                e.kind() == std::io::ErrorKind::WouldBlock,
                "Cannot lock GUI instance: {e}"
            );
            for _ in 0..20 {
                let mut text = String::new();
                lock.seek(SeekFrom::Start(0))?;
                lock.read_to_string(&mut text)?;
                if let Some((port, token)) = text.trim().split_once(' ') {
                    if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
                        if let Ok(mut s) =
                            TcpStream::connect_timeout(&addr, Duration::from_millis(50))
                        {
                            s.set_write_timeout(Some(Duration::from_millis(50)))?;
                            s.write_all(token.as_bytes())?;
                            return Ok(InstanceResult::Existing);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            anyhow::bail!("Existing GUI instance is not responding");
        }
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let token = crate::protocol::new_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        lock.set_len(0)?;
        write!(lock, "{} {}", listener.local_addr()?.port(), token)?;
        lock.sync_all()?;
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
