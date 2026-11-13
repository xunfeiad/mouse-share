use mouse_share::{
    desktop::{Instance, InstanceResult},
    net::devices::{display_id, random},
    settings::{self, SavedSession, Settings},
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mouse-share-test-{}",
            display_id(random().unwrap())
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn preferences_roundtrip_replace_and_invalid_config_backup() {
    let t = Temp::new();
    let path = t.0.join("config.json");
    let (mut s, warning) = settings::load(&path).unwrap();
    assert!(warning.is_none());
    assert!(!s.start_on_login);
    s.language = "zh".into();
    s.auto_reconnect = false;
    s.debug_logging = true;
    s.start_on_login = true;
    s.show_in_menubar = false;
    s.port = 5000;
    s.last_success = Some(SavedSession::Client { device: [3; 16] });
    settings::save(&path, &s).unwrap();
    assert_eq!(settings::load(&path).unwrap().0, s);
    s.port = 6000;
    settings::save(&path, &s).unwrap();
    assert_eq!(settings::load(&path).unwrap().0.port, 6000);
    std::fs::write(&path, b"broken").unwrap();
    let (defaults, warning) = settings::load(&path).unwrap();
    assert_eq!(defaults.port, 4242);
    assert!(warning.is_some());
    assert!(std::fs::read_dir(&t.0).unwrap().any(|f| f
        .unwrap()
        .path()
        .to_string_lossy()
        .contains("corrupt-")));
}
#[test]
fn invalid_ports_and_failed_atomic_save_leave_previous_configuration() {
    let t = Temp::new();
    let path = t.0.join("config.json");
    let s = Settings::default();
    settings::save(&path, &s).unwrap();
    let mut invalid = s.clone();
    invalid.port = 0;
    assert!(settings::save(&path, &invalid).is_err());
    assert_eq!(settings::load(&path).unwrap().0, s);
    std::fs::write(t.0.join("not-a-directory"), b"occupied").unwrap();
    assert!(settings::save(&t.0.join("not-a-directory/config.json"), &s).is_err());
    assert_eq!(settings::load(&path).unwrap().0, s);
}
#[test]
fn duplicate_instance_wakes_primary_and_lock_is_reusable_after_exit() {
    let t = Temp::new();
    let count = Arc::new(AtomicUsize::new(0));
    let notify = count.clone();
    let InstanceResult::Primary(first) = Instance::acquire(
        &t.0,
        Arc::new(move || {
            notify.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .unwrap() else {
        panic!("expected primary")
    };
    assert!(matches!(
        Instance::acquire(&t.0, Arc::new(|| {})).unwrap(),
        InstanceResult::Existing
    ));
    let start = Instant::now();
    while count.load(Ordering::SeqCst) == 0 {
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(first);
    assert!(matches!(
        Instance::acquire(&t.0, Arc::new(|| {})).unwrap(),
        InstanceResult::Primary(_)
    ));
}
#[cfg(target_os = "macos")]
#[test]
fn login_registration_uses_escaped_bundle_path_and_is_removed_in_isolation() {
    let t = Temp::new();
    let path = t.0.join("LaunchAgents/test.plist");
    let exe = std::path::Path::new("/Applications/Test & Mouse.app/Contents/MacOS/mouse-share-ui");
    mouse_share::desktop::set_login_at(&path, exe, true).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("Test &amp; Mouse.app"));
    assert!(content.contains("--autostart"));
    assert!(content.contains("<key>KeepAlive</key><false/>"));
    let status = std::process::Command::new("plutil")
        .args(["-lint"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    mouse_share::desktop::set_login_at(&path, exe, false).unwrap();
    assert!(!path.exists());
    mouse_share::desktop::set_login_at(&path, exe, false).unwrap();
}

#[cfg(target_os = "windows")]
#[test]
fn login_registration_uses_isolated_current_user_registry_and_removes_old_path() {
    struct RegistryGuard(String);
    impl Drop for RegistryGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("reg.exe")
                .args(["delete", &self.0, "/f"])
                .output();
        }
    }
    let subkey = format!(
        r"Software\MouseShareTests\{}",
        display_id(random().unwrap())
    );
    let key = RegistryGuard(format!(r"HKCU\{subkey}"));
    let exe = std::path::Path::new(r"C:\Test Path\mouse-share-ui.exe");
    mouse_share::desktop::set_login_at(&subkey, exe, true).unwrap();
    let output = std::process::Command::new("reg.exe")
        .args(["query", &key.0, "/v", "MouseShare"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains(r#""C:\Test Path\mouse-share-ui.exe" --autostart"#));
    mouse_share::desktop::set_login_at(&subkey, std::path::Path::new(r"C:\Moved\ui.exe"), false)
        .unwrap();
    let output = std::process::Command::new("reg.exe")
        .args(["query", &key.0, "/v", "MouseShare"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
