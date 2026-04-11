//! mouse-share GUI built with egui/eframe.
//!
//! Run with:
//!     cargo run --features ui --bin mouse-share-ui
//!
//! The UI is wired to the real backend (`mouse_share::net::{server, client}`)
//! via a shared `SharedState` that the rendering loop reads each frame. Legacy
//! mockup states remain accessible via the dev-state switcher when the
//! environment variable `MOUSE_SHARE_UI_DEV=1` is set.

#![allow(clippy::too_many_lines)]
#![allow(clippy::too_many_arguments)]
// Some string-table entries are currently only used by the legacy mockup
// states reachable via `MOUSE_SHARE_UI_DEV=1`. Keeping them in the table so
// those dev-only screens stay translated; silence the dead-code warning.
#![allow(dead_code)]

use eframe::egui::{
    self, Align, Color32, FontId, Frame, Layout, Margin, Rect, Response, RichText, Rounding,
    Sense, Stroke, TextEdit, Ui, Vec2,
};
use mouse_share::{
    config as msc,
    input::capture as input_capture,
    log_buffer::{self, LogLine},
    net::{client::Client as BeClient, server::Server as BeServer, SharedState},
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// ============================================================================
// Entry point
// ============================================================================

fn main() -> eframe::Result<()> {
    // Install the tee logger so env_logger output AND the in-memory ring
    // buffer (for the Log tab) both receive every record. Safe to call more
    // than once — subsequent calls are no-ops.
    let _ = log_buffer::install();

    // Promote to a foreground app so CGDisplayHideCursor takes effect when
    // the server hides the local cursor. Without this the .app bundle still
    // works, but bare `cargo run` from a terminal would be a background
    // process and silently no-op the hide.
    input_capture::promote_to_foreground_app();

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 640.0])
            .with_min_inner_size([420.0, 580.0])
            .with_resizable(true)
            .with_decorations(false)
            .with_transparent(true)
            .with_title("mouse share"),
        ..Default::default()
    };
    eframe::run_native(
        "mouse share",
        opts,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            install_style(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}

// ============================================================================
// Theme
// ============================================================================

mod theme {
    use eframe::egui::Color32;

    // Cream backgrounds
    pub const BG: Color32 = Color32::from_rgb(247, 243, 233);
    pub const CARD_BG: Color32 = Color32::from_rgb(253, 251, 245);
    pub const CARD_BORDER: Color32 = Color32::from_rgb(230, 222, 200);
    pub const FIELD_BG: Color32 = Color32::from_rgb(240, 235, 220);
    pub const FIELD_BORDER: Color32 = Color32::from_rgb(228, 221, 200);

    // Text
    pub const TEXT: Color32 = Color32::from_rgb(28, 35, 46);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(122, 127, 136);
    pub const TEXT_SUBTLE: Color32 = Color32::from_rgb(168, 173, 181);

    // Accents
    pub const PRIMARY: Color32 = Color32::from_rgb(12, 110, 76);
    pub const PRIMARY_SOFT: Color32 = Color32::from_rgb(212, 236, 223);

    pub const DANGER: Color32 = Color32::from_rgb(212, 41, 54);
    pub const DANGER_SOFT: Color32 = Color32::from_rgb(251, 224, 225);
    pub const DANGER_SOFT_BORDER: Color32 = Color32::from_rgb(240, 190, 195);

    pub const WARN: Color32 = Color32::from_rgb(217, 140, 15);
    pub const WARN_SOFT: Color32 = Color32::from_rgb(250, 235, 204);

    pub const INFO: Color32 = Color32::from_rgb(47, 110, 224);
    pub const INFO_SOFT: Color32 = Color32::from_rgb(222, 232, 251);

    // Traffic lights — muted ghost style to match mockup
    pub const TL_RED: Color32 = Color32::from_rgb(232, 222, 200);
    pub const TL_YELLOW: Color32 = Color32::from_rgb(232, 222, 200);
    pub const TL_GREEN: Color32 = Color32::from_rgb(232, 222, 200);
    pub const TL_STROKE: Color32 = Color32::from_rgb(210, 200, 178);
}

// ============================================================================
// i18n — multi-language strings
// ============================================================================

#[derive(PartialEq, Clone, Copy, Debug)]
enum Lang {
    En,
    Zh,
}

impl Lang {
    /// Detect the preferred language from the OS. Falls back to English.
    fn from_system() -> Self {
        #[cfg(target_os = "macos")]
        {
            if let Ok(out) = std::process::Command::new("defaults")
                .args(["read", "-g", "AppleLocale"])
                .output()
            {
                if let Ok(s) = std::str::from_utf8(&out.stdout) {
                    if s.starts_with("zh") {
                        return Lang::Zh;
                    }
                }
            }
        }
        if let Ok(lc) = std::env::var("LANG") {
            if lc.to_lowercase().starts_with("zh") {
                return Lang::Zh;
            }
        }
        Lang::En
    }
}

/// Every visible string in the UI. Literals are `&'static str` so we can keep
/// two tables in constants and just swap a reference at runtime.
struct Strings {
    // Tabs
    tab_server: &'static str,
    tab_client: &'static str,
    tab_log: &'static str,
    tab_settings: &'static str,

    // Server / idle
    waiting_for_client: &'static str,
    start_client_hint_l1: &'static str,
    start_client_hint_l2: &'static str,
    label_port: &'static str,
    label_edge: &'static str,
    label_local_ip: &'static str,
    label_screen: &'static str,
    btn_stop_server: &'static str,

    // Server / connected
    pill_connected: &'static str,
    label_up: &'static str,
    toggle_clipboard_sync: &'static str,
    toggle_keyboard_fwd: &'static str,
    clipboard_hello: &'static str,

    // Server / port conflict
    pill_port_unavailable: &'static str,
    label_server_port: &'static str,
    text_occupied: &'static str,
    text_port_in_use: &'static str,
    btn_use_next: &'static str,
    btn_retry: &'static str,
    btn_kill: &'static str,
    label_nearby_ports: &'static str,
    chip_used: &'static str,
    chip_free: &'static str,

    // Server / resolved
    pill_ready: &'static str,
    text_available: &'static str,
    text_switched: &'static str,
    label_udp_input: &'static str,
    label_tcp_clipboard: &'static str,
    text_consecutive_ports: &'static str,
    btn_start_server_on: &'static str,

    // Server / permission
    perm_title: &'static str,
    perm_sub_l1: &'static str,
    perm_sub_l2: &'static str,
    perm_how_to_enable: &'static str,
    perm_instructions: &'static str,
    btn_open_settings: &'static str,

    // Client / config
    label_server_address: &'static str,
    label_screen_edge: &'static str,
    edge_left: &'static str,
    edge_right: &'static str,
    edge_top: &'static str,
    edge_bottom: &'static str,
    label_local_screen: &'static str,
    label_status: &'static str,
    status_idle: &'static str,
    btn_connect: &'static str,

    // Client / connecting
    pill_connecting: &'static str,
    text_attempt: &'static str,
    status_retry: &'static str,
    btn_cancel: &'static str,

    // Client / mouse on server
    label_mouse_here: &'static str,
    label_standby: &'static str,
    text_cursor_hidden: &'static str,
    label_latency: &'static str,
    label_keys: &'static str,
    label_uptime: &'static str,
    btn_disconnect: &'static str,

    // Client / mouse active
    pill_receiving_input: &'static str,
    label_suppressed: &'static str,
    text_cursor_at: &'static str,

    // Client / network error
    pill_connection_lost: &'static str,
    err_server_unreachable: &'static str,
    err_unreachable_detail: &'static str,
    err_firewall_check: &'static str,
    err_firewall_detail: &'static str,
    btn_copy_firewall: &'static str,
    btn_reconnect: &'static str,
    btn_edit_config: &'static str,

    // Log
    filter_all: &'static str,
    filter_info: &'static str,
    filter_warn: &'static str,
    filter_err: &'static str,
    log_info: &'static str,
    log_warn: &'static str,
    log_err: &'static str,
    log_connected_sep: &'static str,
    log_msg_server_on: &'static str,
    log_msg_clipboard_tcp: &'static str,
    log_msg_client: &'static str,
    log_msg_nodelay: &'static str,
    log_msg_entered: &'static str,
    log_msg_returned: &'static str,
    log_msg_clip_send_reset: &'static str,
    log_msg_clip_reconnected: &'static str,
    log_events_duration: &'static str,
    log_auto_scroll: &'static str,

    // Settings
    section_general: &'static str,
    set_start_on_login: &'static str,
    set_start_on_login_sub: &'static str,
    set_auto_connect: &'static str,
    set_auto_connect_sub: &'static str,
    set_show_in_menubar: &'static str,
    set_show_in_menubar_sub: &'static str,
    section_network: &'static str,
    set_default_port: &'static str,
    set_default_edge: &'static str,
    section_advanced: &'static str,
    set_debug_logging: &'static str,
    set_debug_logging_sub: &'static str,
    section_language: &'static str,
    lang_english: &'static str,
    lang_chinese: &'static str,
}

const EN: Strings = Strings {
    tab_server: "Server",
    tab_client: "Client",
    tab_log: "Log",
    tab_settings: "Settings",

    waiting_for_client: "Waiting for client",
    start_client_hint_l1: "Start the client on another device and",
    start_client_hint_l2: "point it here",
    label_port: "Port",
    label_edge: "Edge",
    label_local_ip: "Local IP",
    label_screen: "Screen",
    btn_stop_server: "Stop server",

    pill_connected: "Connected",
    label_up: "Up",
    toggle_clipboard_sync: "Clipboard sync",
    toggle_keyboard_fwd: "Keyboard fwd",
    clipboard_hello: "clipboard: \"Hello\" (5ch)",

    pill_port_unavailable: "Port unavailable",
    label_server_port: "Server port",
    text_occupied: "Occupied",
    text_port_in_use: "Port 4242 is in use",
    btn_use_next: "Use 4244",
    btn_retry: "Retry",
    btn_kill: "Kill",
    label_nearby_ports: "Nearby ports",
    chip_used: "used",
    chip_free: "free",

    pill_ready: "Ready",
    text_available: "Available",
    text_switched: "Switched 4242 → 4244",
    label_udp_input: "UDP (input)",
    label_tcp_clipboard: "TCP (clipboard)",
    text_consecutive_ports: "ⓘ 2 consecutive ports needed",
    btn_start_server_on: "Start server on :4244",

    perm_title: "Accessibility permission required",
    perm_sub_l1: "Mouse Share needs Accessibility access to",
    perm_sub_l2: "capture and simulate input events",
    perm_how_to_enable: "How to enable",
    perm_instructions: "System Settings → Privacy & Security → Accessibility → toggle on \"mouse-share\"",
    btn_open_settings: "Open Settings",

    label_server_address: "Server address",
    label_screen_edge: "Screen edge",
    edge_left: "Left",
    edge_right: "Right",
    edge_top: "Top",
    edge_bottom: "Bottom",
    label_local_screen: "Local screen",
    label_status: "Status",
    status_idle: "Idle",
    btn_connect: "Connect",

    pill_connecting: "Connecting",
    text_attempt: "attempt 3/10",
    status_retry: "Retry",
    btn_cancel: "Cancel",

    label_mouse_here: "Mouse here",
    label_standby: "Standby",
    text_cursor_hidden: "Cursor hidden · waiting for entry",
    label_latency: "Latency",
    label_keys: "Keys",
    label_uptime: "Uptime",
    btn_disconnect: "Disconnect",

    pill_receiving_input: "Receiving input",
    label_suppressed: "Suppressed",
    text_cursor_at: "❙❙❙  Cursor at (823, 412) · keyboard forwarding",

    pill_connection_lost: "Connection lost",
    err_server_unreachable: "Server unreachable",
    err_unreachable_detail: "Cannot reach 192.168.1.100:4242. Check that both devices are on the same network and the server is running.",
    err_firewall_check: "Firewall check",
    err_firewall_detail: "Ensure UDP :4242 and TCP :4243 are allowed through your firewall.",
    btn_copy_firewall: "Copy firewall rules",
    btn_reconnect: "Reconnect",
    btn_edit_config: "Edit config",

    filter_all: "All 24",
    filter_info: "Info 18",
    filter_warn: "Warn 4",
    filter_err: "Err 2",
    log_info: "info",
    log_warn: "warn",
    log_err: "err",
    log_connected_sep: "connected",
    log_msg_server_on: "server on 0.0.0.0:4242",
    log_msg_clipboard_tcp: "clipboard TCP on :4243",
    log_msg_client: "client 192.168.1.42",
    log_msg_nodelay: "TCP_NODELAY failed",
    log_msg_entered: "entered client (0, 540)",
    log_msg_returned: "returned to server",
    log_msg_clip_send_reset: "clipboard send: reset",
    log_msg_clip_reconnected: "clipboard reconnected",
    log_events_duration: "24 events · 33s",
    log_auto_scroll: "auto-scroll",

    section_general: "General",
    set_start_on_login: "Start on login",
    set_start_on_login_sub: "Launch at system startup",
    set_auto_connect: "Auto-connect",
    set_auto_connect_sub: "Reconnect on disconnect",
    set_show_in_menubar: "Show in menu bar",
    set_show_in_menubar_sub: "Tray icon (macOS/Win)",
    section_network: "Network",
    set_default_port: "Default port",
    set_default_edge: "Default edge",
    section_advanced: "Advanced",
    set_debug_logging: "Debug logging",
    set_debug_logging_sub: "Verbose output for troubleshooting",
    section_language: "Language",
    lang_english: "English",
    lang_chinese: "中文",
};

const ZH: Strings = Strings {
    tab_server: "服务端",
    tab_client: "客户端",
    tab_log: "日志",
    tab_settings: "设置",

    waiting_for_client: "等待客户端",
    start_client_hint_l1: "在另一台设备上启动客户端",
    start_client_hint_l2: "并连接到此处",
    label_port: "端口",
    label_edge: "方位",
    label_local_ip: "本机 IP",
    label_screen: "屏幕",
    btn_stop_server: "停止服务",

    pill_connected: "已连接",
    label_up: "时长",
    toggle_clipboard_sync: "剪贴板同步",
    toggle_keyboard_fwd: "转发键盘",
    clipboard_hello: "剪贴板: \"你好\" (2字)",

    pill_port_unavailable: "端口不可用",
    label_server_port: "服务端口",
    text_occupied: "已占用",
    text_port_in_use: "端口 4242 被占用",
    btn_use_next: "使用 4244",
    btn_retry: "重试",
    btn_kill: "结束进程",
    label_nearby_ports: "附近端口",
    chip_used: "占用",
    chip_free: "空闲",

    pill_ready: "就绪",
    text_available: "可用",
    text_switched: "已切换 4242 → 4244",
    label_udp_input: "UDP (输入)",
    label_tcp_clipboard: "TCP (剪贴板)",
    text_consecutive_ports: "ⓘ 需要 2 个连续端口",
    btn_start_server_on: "在 :4244 启动服务",

    perm_title: "需要「辅助功能」权限",
    perm_sub_l1: "Mouse Share 需要「辅助功能」权限",
    perm_sub_l2: "才能捕获和模拟输入事件",
    perm_how_to_enable: "开启方法",
    perm_instructions: "系统设置 → 隐私与安全性 → 辅助功能 → 开启 \"mouse-share\"",
    btn_open_settings: "打开设置",

    label_server_address: "服务端地址",
    label_screen_edge: "屏幕方位",
    edge_left: "左",
    edge_right: "右",
    edge_top: "上",
    edge_bottom: "下",
    label_local_screen: "本机屏幕",
    label_status: "状态",
    status_idle: "空闲",
    btn_connect: "连接",

    pill_connecting: "连接中",
    text_attempt: "尝试 3/10",
    status_retry: "重试",
    btn_cancel: "取消",

    label_mouse_here: "鼠标在此",
    label_standby: "待机",
    text_cursor_hidden: "光标已隐藏 · 等待进入",
    label_latency: "延迟",
    label_keys: "按键",
    label_uptime: "时长",
    btn_disconnect: "断开",

    pill_receiving_input: "正在接收输入",
    label_suppressed: "已抑制",
    text_cursor_at: "❙❙❙  光标位置 (823, 412) · 键盘转发中",

    pill_connection_lost: "连接断开",
    err_server_unreachable: "服务端不可达",
    err_unreachable_detail: "无法连接 192.168.1.100:4242。请确认两台设备在同一网络且服务端正在运行。",
    err_firewall_check: "防火墙检查",
    err_firewall_detail: "请在防火墙中放行 UDP :4242 与 TCP :4243。",
    btn_copy_firewall: "复制防火墙规则",
    btn_reconnect: "重新连接",
    btn_edit_config: "编辑配置",

    filter_all: "全部 24",
    filter_info: "信息 18",
    filter_warn: "警告 4",
    filter_err: "错误 2",
    log_info: "信息",
    log_warn: "警告",
    log_err: "错误",
    log_connected_sep: "已连接",
    log_msg_server_on: "服务端启动于 0.0.0.0:4242",
    log_msg_clipboard_tcp: "剪贴板 TCP 监听于 :4243",
    log_msg_client: "客户端 192.168.1.42",
    log_msg_nodelay: "TCP_NODELAY 设置失败",
    log_msg_entered: "进入客户端 (0, 540)",
    log_msg_returned: "返回服务端",
    log_msg_clip_send_reset: "剪贴板发送: 重置",
    log_msg_clip_reconnected: "剪贴板已重连",
    log_events_duration: "24 条事件 · 33s",
    log_auto_scroll: "自动滚动",

    section_general: "通用",
    set_start_on_login: "开机启动",
    set_start_on_login_sub: "系统启动时自动运行",
    set_auto_connect: "自动连接",
    set_auto_connect_sub: "断线后自动重连",
    set_show_in_menubar: "显示在菜单栏",
    set_show_in_menubar_sub: "托盘图标 (macOS/Win)",
    section_network: "网络",
    set_default_port: "默认端口",
    set_default_edge: "默认方位",
    section_advanced: "高级",
    set_debug_logging: "调试日志",
    set_debug_logging_sub: "输出详细日志用于排查问题",
    section_language: "语言",
    lang_english: "English",
    lang_chinese: "中文",
};

/// Load a platform CJK font so Chinese glyphs render. Silent no-op if the
/// expected system font is missing (egui falls back to its default font which
/// can only render ASCII/latin, so Chinese would show as tofu squares).
fn install_fonts(ctx: &egui::Context) {
    // Candidate CJK font files we'll probe in order.
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simhei.ttf",
        ]
    } else {
        &[]
    };

    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                egui::FontData::from_owned(bytes),
            );
            // Append CJK font AFTER the existing defaults so latin characters
            // still use the default font's kerning/metrics and only CJK
            // codepoints fall back to the system font.
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            ctx.set_fonts(fonts);
            return;
        }
    }
}

fn install_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;

    v.override_text_color = Some(theme::TEXT);
    v.panel_fill = theme::BG;
    v.window_fill = theme::CARD_BG;
    v.extreme_bg_color = theme::FIELD_BG;
    v.faint_bg_color = theme::FIELD_BG;
    v.selection.bg_fill = theme::PRIMARY.linear_multiply(0.3);
    v.selection.stroke = Stroke::new(1.0, theme::PRIMARY);

    // Widgets: form fields / buttons defaults
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        w.bg_fill = theme::FIELD_BG;
        w.weak_bg_fill = theme::FIELD_BG;
        w.bg_stroke = Stroke::new(1.0, theme::FIELD_BORDER);
        w.rounding = Rounding::same(8.0);
        w.fg_stroke = Stroke::new(1.0, theme::TEXT);
        w.expansion = 0.0;
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, theme::PRIMARY);
    v.widgets.noninteractive.bg_fill = theme::CARD_BG;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme::CARD_BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme::TEXT);

    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    style.spacing.interact_size.y = 32.0;
    style.spacing.window_margin = Margin::same(0.0);

    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(13.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(13.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Heading,
        FontId::new(18.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        FontId::new(12.0, egui::FontFamily::Monospace),
    );

    ctx.set_style(style);
}

// ============================================================================
// Top-level state
// ============================================================================

#[derive(Default, PartialEq, Clone, Copy)]
enum Tab {
    #[default]
    Server,
    Client,
    Log,
    Settings,
}

#[derive(PartialEq, Clone, Copy)]
enum ServerState {
    Idle,
    Connected,
    PortConflict,
    PortResolved,
    PermissionRequired,
}

#[derive(PartialEq, Clone, Copy)]
enum ClientState {
    Config,
    Connecting,
    MouseOnServer,
    MouseActive,
    NetworkError,
}

#[derive(PartialEq, Clone, Copy)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn label(self, s: &Strings) -> &'static str {
        match self {
            Edge::Left => s.edge_left,
            Edge::Right => s.edge_right,
            Edge::Top => s.edge_top,
            Edge::Bottom => s.edge_bottom,
        }
    }

    fn to_config(self) -> msc::Edge {
        match self {
            Edge::Left => msc::Edge::Left,
            Edge::Right => msc::Edge::Right,
            Edge::Top => msc::Edge::Top,
            Edge::Bottom => msc::Edge::Bottom,
        }
    }
}

/// What kind of session is currently running.
#[derive(Clone, Copy, PartialEq)]
enum SessionKind {
    Server,
    Client,
}

/// Handle to a live backend session. Dropping this does NOT stop the
/// session — use `App::stop_session` which sets `shutdown` and joins.
struct SessionHandle {
    kind: SessionKind,
    thread: Option<JoinHandle<()>>,
}

/// Button-click intents produced by render functions and processed after
/// the render pass by `App::apply_pending_action`. This indirection avoids
/// double-borrowing `self` inside the nested closures egui uses.
#[derive(Clone, Copy)]
enum Action {
    StartServer,
    StartClient,
    StopSession,
}

struct App {
    tab: Tab,
    lang: Lang,
    // Server tab state
    server_state: ServerState,
    port: String,
    edge: Edge,
    clipboard_sync: bool,
    keyboard_fwd: bool,
    // Client tab state
    client_state: ClientState,
    server_addr: String,
    server_port: String,
    client_edge: Edge,
    // Settings
    start_on_login: bool,
    auto_connect: bool,
    show_in_menubar: bool,
    default_port: String,
    default_edge: Edge,
    debug_logging: bool,

    // --- Runtime wiring (not persisted) ---
    /// Shared observable state between the UI and the running backend. A
    /// fresh `Arc` is installed on every Start so atomics from the previous
    /// session can't leak into the next.
    shared: Arc<SharedState>,
    /// Live session if any. `None` means "nothing running".
    session: Option<SessionHandle>,
    /// Deferred action captured from button clicks during a render pass.
    pending_action: Option<Action>,
    /// Dev switcher visibility (set by `MOUSE_SHARE_UI_DEV=1`).
    dev_mode: bool,
}

impl App {
    /// Return the active string table (static reference — zero allocation).
    fn s(&self) -> &'static Strings {
        match self.lang {
            Lang::En => &EN,
            Lang::Zh => &ZH,
        }
    }

    /// `true` while a backend session is spawned (running or tearing down).
    fn is_running(&self) -> bool {
        self.session.is_some()
    }

    fn session_kind(&self) -> Option<SessionKind> {
        self.session.as_ref().map(|s| s.kind)
    }

    /// Spawn the backend `Server` on a dedicated thread. Installs a fresh
    /// `SharedState` so the UI observes only state from this new session.
    fn start_server(&mut self) {
        if self.is_running() {
            return;
        }
        let port: u16 = self.port.trim().parse().unwrap_or(4242);
        let edge = self.edge.to_config();
        self.shared = Arc::new(SharedState::new());
        let shared = self.shared.clone();
        log::info!("UI: starting server on port {} edge {:?}", port, edge);
        let thread = std::thread::Builder::new()
            .name("mouse-share-server".into())
            .spawn(move || {
                let server = BeServer::new(port, edge);
                if let Err(e) = server.run(shared.clone()) {
                    log::error!("server exited with error: {}", e);
                    shared.set_error(format!("{}", e));
                }
            })
            .ok();
        self.session = Some(SessionHandle {
            kind: SessionKind::Server,
            thread,
        });
    }

    /// Spawn the backend `Client` on a dedicated thread.
    fn start_client(&mut self) {
        if self.is_running() {
            return;
        }
        let host = self.server_addr.trim().to_string();
        let port = self.server_port.trim().to_string();
        let addr = if port.is_empty() {
            host
        } else {
            format!("{}:{}", host, port)
        };
        self.shared = Arc::new(SharedState::new());
        let shared = self.shared.clone();
        log::info!("UI: starting client → {}", addr);
        let thread = std::thread::Builder::new()
            .name("mouse-share-client".into())
            .spawn(move || {
                let client = BeClient::new(addr);
                if let Err(e) = client.run(shared.clone()) {
                    log::error!("client exited with error: {}", e);
                    shared.set_error(format!("{}", e));
                }
            })
            .ok();
        self.session = Some(SessionHandle {
            kind: SessionKind::Client,
            thread,
        });
    }

    /// Signal the running session to shut down and join its thread. Safe to
    /// call when nothing is running — it just clears state.
    fn stop_session(&mut self) {
        if let Some(mut sess) = self.session.take() {
            log::info!("UI: stopping session");
            self.shared.shutdown.store(true, Ordering::SeqCst);
            if let Some(t) = sess.thread.take() {
                // Best-effort join — the backend loops poll shutdown every
                // 100ms or so, and clipboard sockets have 500ms timeouts, so
                // this should complete quickly. We don't block forever.
                let _ = t.join();
            }
        }
        // Reset observable state for the next session.
        self.shared = Arc::new(SharedState::new());
    }

    /// Process any action a render function emitted during this frame.
    fn apply_pending_action(&mut self) {
        if let Some(action) = self.pending_action.take() {
            match action {
                Action::StartServer => self.start_server(),
                Action::StartClient => self.start_client(),
                Action::StopSession => self.stop_session(),
            }
        }
    }

    /// Reconcile the UI state enums with what the backend actually reports.
    /// Called at the top of every frame before rendering.
    fn derive_states(&mut self) {
        // Dev mode bypass — the user is driving the state machine manually
        // via the dev switcher, so don't overwrite their selection.
        if self.dev_mode {
            return;
        }

        let running = self.is_running();
        let kind = self.session_kind();
        let connected = self.shared.connected.load(Ordering::SeqCst);
        let mouse_on_peer = self.shared.mouse_on_peer.load(Ordering::SeqCst);
        let has_error = self.shared.last_error.lock().unwrap().is_some();

        // If the session thread has finished on its own (error or normal
        // exit), reap it so we flip back to the not-running state.
        let finished = self
            .session
            .as_ref()
            .and_then(|s| s.thread.as_ref())
            .map(|t| t.is_finished())
            .unwrap_or(false);
        if finished {
            if let Some(mut sess) = self.session.take() {
                if let Some(t) = sess.thread.take() {
                    let _ = t.join();
                }
            }
        }

        let running = running && self.session.is_some();

        // Server tab state
        if matches!(kind, Some(SessionKind::Server)) && running {
            self.server_state = if connected {
                ServerState::Connected
            } else {
                ServerState::Idle
            };
        } else if !running {
            // Only override if we're not sitting in a non-run screen the
            // user reached via dev switcher.
            self.server_state = ServerState::Idle;
        }

        // Client tab state
        if matches!(kind, Some(SessionKind::Client)) && running {
            self.client_state = if has_error && !connected {
                ClientState::NetworkError
            } else if !connected {
                ClientState::Connecting
            } else if mouse_on_peer {
                ClientState::MouseActive
            } else {
                ClientState::MouseOnServer
            };
        } else if !running {
            if !matches!(self.client_state, ClientState::Config) {
                self.client_state = ClientState::Config;
            }
        }
    }

    fn peer_addr_display(&self) -> String {
        self.shared
            .peer_addr
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "—".to_string())
    }

    fn uptime_display(&self) -> String {
        let started = self.shared.started_ms.load(Ordering::SeqCst);
        if started == 0 {
            return "—".to_string();
        }
        let now = mouse_share::net::state::now_ms();
        let secs = now.saturating_sub(started) / 1000;
        format_duration(secs)
    }

}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

impl Default for App {
    fn default() -> Self {
        let mut app = Self {
            tab: Tab::Server,
            lang: Lang::from_system(),
            server_state: ServerState::Idle,
            port: "4242".into(),
            edge: Edge::Right,
            clipboard_sync: true,
            keyboard_fwd: true,
            client_state: ClientState::Config,
            server_addr: "192.168.1.100".into(),
            server_port: "4242".into(),
            client_edge: Edge::Right,
            start_on_login: true,
            auto_connect: true,
            show_in_menubar: true,
            default_port: "4242".into(),
            default_edge: Edge::Right,
            debug_logging: false,
            shared: Arc::new(SharedState::new()),
            session: None,
            pending_action: None,
            dev_mode: std::env::var("MOUSE_SHARE_UI_DEV")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        };
        // Dev hook: start in a specific visual state for screenshotting.
        if let Ok(s) = std::env::var("MOUSE_SHARE_UI_STATE") {
            match s.as_str() {
                "server_idle" => { app.tab = Tab::Server; app.server_state = ServerState::Idle; }
                "server_connected" => { app.tab = Tab::Server; app.server_state = ServerState::Connected; }
                "server_conflict" => { app.tab = Tab::Server; app.server_state = ServerState::PortConflict; }
                "server_resolved" => { app.tab = Tab::Server; app.server_state = ServerState::PortResolved; }
                "client_config" => { app.tab = Tab::Client; app.client_state = ClientState::Config; }
                "client_connecting" => { app.tab = Tab::Client; app.client_state = ClientState::Connecting; }
                "client_on_server" => { app.tab = Tab::Client; app.client_state = ClientState::MouseOnServer; }
                "client_active" => { app.tab = Tab::Client; app.client_state = ClientState::MouseActive; }
                "permission" => { app.tab = Tab::Server; app.server_state = ServerState::PermissionRequired; }
                "error" => { app.tab = Tab::Client; app.client_state = ClientState::NetworkError; }
                "log" => { app.tab = Tab::Log; }
                "settings" => { app.tab = Tab::Settings; }
                _ => {}
            }
        }
        // Language override for screenshotting.
        if let Ok(l) = std::env::var("MOUSE_SHARE_UI_LANG") {
            app.lang = match l.as_str() {
                "zh" => Lang::Zh,
                _ => Lang::En,
            };
        }
        app
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, a] = theme::BG.to_array();
        [
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        ]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Reconcile UI enums with the backend before we render this frame.
        self.derive_states();

        egui::CentralPanel::default()
            .frame(Frame::none().fill(theme::BG).inner_margin(Margin::same(16.0)))
            .show(ctx, |ui| {
                let s = self.s();
                card(ui, |ui| {
                    title_bar(ui, "mouse share");
                    tab_bar(ui, &mut self.tab, s);
                    ui.add_space(14.0);
                    ui.scope(|ui| {
                        ui.style_mut().spacing.item_spacing.y = 12.0;
                        match self.tab {
                            Tab::Server => server_tab(ui, self),
                            Tab::Client => client_tab(ui, self),
                            Tab::Log => log_tab(ui, self),
                            Tab::Settings => settings_tab(ui, self),
                        }
                    });
                });
            });

        // Apply any button action collected by the render pass.
        self.apply_pending_action();

        // Keep the UI responsive to backend state changes. We poll atomics,
        // so a steady repaint cadence is what drives uptime/events updates.
        ctx.request_repaint_after(Duration::from_millis(150));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Make sure the backend thread doesn't outlive the window.
        self.stop_session();
    }
}

// ============================================================================
// Reusable widgets
// ============================================================================

fn card(ui: &mut Ui, contents: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(20.0, 16.0))
        .show(ui, contents);
}

fn title_bar(ui: &mut Ui, title: &str) {
    ui.horizontal(|ui| {
        // Traffic lights — ghost/muted style to match mockup
        let (rect, _) = ui.allocate_exact_size(Vec2::new(52.0, 14.0), Sense::hover());
        let y = rect.center().y;
        let colors = [theme::TL_RED, theme::TL_YELLOW, theme::TL_GREEN];
        for (i, c) in colors.iter().enumerate() {
            let x = rect.left() + 7.0 + i as f32 * 16.0;
            ui.painter().circle(
                egui::pos2(x, y),
                5.5,
                *c,
                Stroke::new(1.0, theme::TL_STROKE),
            );
        }

        // Centered icon + title. Compute the combined width so the pair
        // stays visually centered in the available space, accounting for
        // the traffic-light block on the left.
        let title_font = FontId::new(13.0, egui::FontFamily::Proportional);
        let title_w = ui.fonts(|f| {
            f.layout_no_wrap(title.into(), title_font.clone(), theme::TEXT_MUTED)
                .size()
                .x
        });
        const ICON_W: f32 = 18.0;
        const GAP: f32 = 6.0;
        let total_w = ICON_W + GAP + title_w;
        let avail = ui.available_width();
        ui.add_space(((avail - total_w) / 2.0 - 26.0).max(0.0));

        // Icon: two small screens with an arrow between them — literal
        // pictogram for "mouse/input shared across two displays".
        let (icon_rect, _) =
            ui.allocate_exact_size(Vec2::new(ICON_W, 18.0), Sense::hover());
        draw_app_icon(ui.painter(), icon_rect);

        ui.label(
            RichText::new(title)
                .size(13.0)
                .color(theme::TEXT_MUTED),
        );
    });
    ui.add_space(12.0);
}

/// Draw the mouse-share app icon at the given rect. Vector-drawn so it
/// scales cleanly and doesn't need a bundled image asset. The pictogram
/// is "two screens + an arrow pointing from left to right" — a compact
/// visual cue for "input shared between two devices".
fn draw_app_icon(painter: &eframe::egui::Painter, rect: Rect) {
    use eframe::egui::Pos2;

    let color = theme::PRIMARY;
    let stroke = Stroke::new(1.4, color);

    // Two identical screen rectangles, left and right. Height is chosen
    // to leave vertical room for the arrow at the screens' midline.
    let screen_w: f32 = 6.5;
    let screen_h: f32 = 5.0;
    let cy = rect.center().y - 1.5;

    let left = Rect::from_min_size(
        Pos2::new(rect.left() + 1.0, cy - screen_h / 2.0),
        Vec2::new(screen_w, screen_h),
    );
    let right = Rect::from_min_size(
        Pos2::new(rect.right() - 1.0 - screen_w, cy - screen_h / 2.0),
        Vec2::new(screen_w, screen_h),
    );
    painter.rect_stroke(left, Rounding::same(1.2), stroke);
    painter.rect_stroke(right, Rounding::same(1.2), stroke);

    // Tiny stands under each screen — just two short horizontal ticks,
    // positioned below the rects. Makes them read as "monitors".
    let stand_y = left.bottom() + 1.6;
    let tick = 2.0;
    painter.line_segment(
        [
            Pos2::new(left.center().x - tick, stand_y),
            Pos2::new(left.center().x + tick, stand_y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(right.center().x - tick, stand_y),
            Pos2::new(right.center().x + tick, stand_y),
        ],
        stroke,
    );

    // Arrow from left screen to right screen, above the screens so it
    // doesn't overlap them. The arrow is what communicates "sharing".
    let arrow_y = left.top() - 2.2;
    let ax1 = left.center().x;
    let ax2 = right.center().x;
    painter.line_segment([Pos2::new(ax1, arrow_y), Pos2::new(ax2, arrow_y)], stroke);
    // Arrowhead (two short strokes).
    painter.line_segment(
        [Pos2::new(ax2 - 2.0, arrow_y - 1.8), Pos2::new(ax2, arrow_y)],
        stroke,
    );
    painter.line_segment(
        [Pos2::new(ax2 - 2.0, arrow_y + 1.8), Pos2::new(ax2, arrow_y)],
        stroke,
    );
}

fn tab_bar(ui: &mut Ui, tab: &mut Tab, s: &Strings) {
    ui.horizontal(|ui| {
        ui.style_mut().spacing.item_spacing.x = 22.0;
        for (t, label) in [
            (Tab::Server, s.tab_server),
            (Tab::Client, s.tab_client),
            (Tab::Log, s.tab_log),
            (Tab::Settings, s.tab_settings),
        ] {
            let selected = *tab == t;
            let color = if selected {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            };
            let text = RichText::new(label).size(14.0).color(color);
            let resp = ui
                .add(egui::Label::new(text).sense(Sense::click()));
            if resp.clicked() {
                *tab = t;
            }
            // Underline for the selected tab
            if selected {
                let rect = resp.rect;
                let y = rect.bottom() + 4.0;
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    Stroke::new(2.0, theme::PRIMARY),
                );
            }
        }
    });
    ui.add_space(4.0);
    // Divider under the tab bar
    let r = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [
            egui::pos2(r.left() - 20.0, r.top() + 2.0),
            egui::pos2(r.right() + 20.0, r.top() + 2.0),
        ],
        Stroke::new(1.0, theme::CARD_BORDER),
    );
    ui.add_space(4.0);
}

fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> Response {
    Frame::none()
        .fill(bg)
        .rounding(Rounding::same(999.0))
        .inner_margin(Margin::symmetric(12.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot_rect, _) =
                    ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 3.5, fg);
                ui.label(RichText::new(text).size(12.0).color(fg).strong());
            });
        })
        .response
}

fn field(ui: &mut Ui, label: &str, value: &str) {
    Frame::none()
        .fill(theme::FIELD_BG)
        .stroke(Stroke::new(1.0, theme::FIELD_BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(label).size(11.0).color(theme::TEXT_MUTED));
                ui.add_space(2.0);
                ui.label(RichText::new(value).size(15.0).color(theme::TEXT).strong());
            });
        });
}

fn field_pair(ui: &mut Ui, l1: &str, v1: &str, l2: &str, v2: &str) {
    ui.columns(2, |cols| {
        field(&mut cols[0], l1, v1);
        field(&mut cols[1], l2, v2);
    });
}

fn field_trio(ui: &mut Ui, labels: [(&str, &str); 3]) {
    ui.columns(3, |cols| {
        for (i, (l, v)) in labels.iter().enumerate() {
            field(&mut cols[i], l, v);
        }
    });
}

fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(
        RichText::new(text).size(13.0).color(Color32::WHITE).strong(),
    )
    .fill(theme::PRIMARY)
    .stroke(Stroke::NONE)
    .rounding(Rounding::same(10.0))
    .min_size(Vec2::new(ui.available_width(), 36.0));
    ui.add(btn)
}

fn danger_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(
        RichText::new(text).size(13.0).color(theme::DANGER).strong(),
    )
    .fill(theme::CARD_BG)
    .stroke(Stroke::new(1.0, theme::DANGER_SOFT_BORDER))
    .rounding(Rounding::same(10.0))
    .min_size(Vec2::new(ui.available_width(), 36.0));
    ui.add(btn)
}

fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(RichText::new(text).size(13.0).color(theme::TEXT))
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .rounding(Rounding::same(10.0))
        .min_size(Vec2::new(ui.available_width(), 36.0));
    ui.add(btn)
}

/// Solid-red "Stop server" button with a pulsing white dot on the left
/// indicating the service is actively running. Used when a session is live
/// — the dot's pulse gives a subtle heartbeat cue so the user can tell at
/// a glance that the backend is alive even when there's no visible
/// activity (e.g. no events flowing at the moment).
///
/// The pulse is driven directly off `ctx.input(|i| i.time)` rather than a
/// stored phase, so the animation stays smooth across repaints. We request
/// a short follow-up repaint (~33 ms → ~30 fps) so the pulse keeps ticking
/// even when the rest of the UI is idle.
fn stop_running_button(ui: &mut Ui, text: &str) -> Response {
    use eframe::egui::{Align2, FontFamily, Pos2};

    let size = Vec2::new(ui.available_width(), 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());

    // Drive pulse off wall-clock. 1.2 s period is slow enough to read as
    // a heartbeat rather than strobing, fast enough to feel alive.
    let time = ui.ctx().input(|i| i.time);
    let pulse = ((time * std::f64::consts::TAU / 1.2).sin() * 0.5 + 0.5) as f32;
    // Keep the animation moving even during otherwise-idle frames.
    ui.ctx().request_repaint_after(Duration::from_millis(33));

    // Base fill darkens slightly on hover / press so the button still
    // feels interactive despite the custom paint.
    let base_fill = if resp.is_pointer_button_down_on() {
        Color32::from_rgb(180, 30, 42)
    } else if resp.hovered() {
        Color32::from_rgb(226, 52, 64)
    } else {
        theme::DANGER
    };

    let painter = ui.painter();
    painter.rect(
        rect,
        Rounding::same(10.0),
        base_fill,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)),
    );

    // Pulsing dot on the left. Radius breathes between 3.5 and 5.5 px;
    // alpha breathes in lockstep so the dot feels like it's glowing rather
    // than just expanding.
    let dot_center = Pos2::new(rect.left() + 16.0, rect.center().y);
    let dot_radius = 3.5 + pulse * 2.0;
    let dot_alpha = 140 + (pulse * 115.0) as u8;
    let dot_color = Color32::from_rgba_unmultiplied(255, 255, 255, dot_alpha);
    painter.circle_filled(dot_center, dot_radius, dot_color);
    // Soft outer ring for the "running" glow effect.
    let ring_alpha = ((1.0 - pulse) * 90.0) as u8;
    if ring_alpha > 0 {
        painter.circle_stroke(
            dot_center,
            dot_radius + 2.0 + pulse * 2.5,
            Stroke::new(
                1.0,
                Color32::from_rgba_unmultiplied(255, 255, 255, ring_alpha),
            ),
        );
    }

    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::new(13.0, FontFamily::Proportional),
        Color32::WHITE,
    );

    resp
}

/// Draw a "screen" card — a rectangle with resolution text, used in the
/// topology diagram on Server/Client "connected" states.
fn screen_box(
    ui: &mut Ui,
    size: Vec2,
    resolution: &str,
    label_below: Option<&str>,
    highlighted: bool,
    dim: bool,
) -> Rect {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let (fill, stroke_color, text_color) = if dim {
        (theme::FIELD_BG, theme::FIELD_BORDER, theme::TEXT_SUBTLE)
    } else if highlighted {
        (theme::CARD_BG, theme::PRIMARY, theme::TEXT)
    } else {
        (theme::CARD_BG, theme::CARD_BORDER, theme::TEXT)
    };
    let p = ui.painter();
    p.rect(
        rect,
        Rounding::same(8.0),
        fill,
        Stroke::new(if highlighted { 2.0 } else { 1.0 }, stroke_color),
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        resolution,
        FontId::new(12.0, egui::FontFamily::Proportional),
        text_color,
    );
    if let Some(label) = label_below {
        p.text(
            egui::pos2(rect.center().x, rect.bottom() + 14.0),
            egui::Align2::CENTER_CENTER,
            label,
            FontId::new(11.0, egui::FontFamily::Proportional),
            theme::TEXT_MUTED,
        );
    }
    rect
}

/// Horizontal topology: two screen boxes with a dashed line + latency label.
fn topology(
    ui: &mut Ui,
    left_res: &str,
    right_res: &str,
    latency: &str,
    left_label: Option<&str>,
    right_label: Option<&str>,
    highlight_left: bool,
    highlight_right: bool,
) {
    ui.horizontal(|ui| {
        ui.add_space((ui.available_width() - 280.0).max(0.0) / 2.0);
        ui.allocate_ui_with_layout(
            Vec2::new(280.0, 86.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                let box_size = Vec2::new(104.0, 66.0);
                let left_rect = screen_box(
                    ui,
                    box_size,
                    left_res,
                    left_label,
                    highlight_left,
                    !highlight_left && highlight_right,
                );
                let gap_start = left_rect.right() + 4.0;
                let gap_end = gap_start + 60.0;
                // Dashed connector
                let y = left_rect.center().y;
                let p = ui.painter();
                let dash = 5.0;
                let gap = 3.0;
                let mut x = gap_start;
                while x < gap_end {
                    let next = (x + dash).min(gap_end);
                    p.line_segment(
                        [egui::pos2(x, y), egui::pos2(next, y)],
                        Stroke::new(1.5, theme::PRIMARY),
                    );
                    x = next + gap;
                }
                p.text(
                    egui::pos2((gap_start + gap_end) / 2.0, y + 12.0),
                    egui::Align2::CENTER_CENTER,
                    latency,
                    FontId::new(11.0, egui::FontFamily::Proportional),
                    theme::INFO,
                );
                // Spacer so the next box starts at gap_end
                ui.add_space(gap_end - ui.cursor().min.x + 4.0);
                screen_box(
                    ui,
                    box_size,
                    right_res,
                    right_label,
                    highlight_right,
                    !highlight_right && highlight_left,
                );
            },
        );
    });
}

fn toggle(ui: &mut Ui, label: &str, value: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(13.0).color(theme::TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            toggle_switch(ui, value);
        });
    });
}

fn toggle_switch(ui: &mut Ui, on: &mut bool) -> Response {
    let desired = Vec2::new(36.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(desired, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let bg = if *on { theme::PRIMARY } else { theme::FIELD_BORDER };
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(10.0), bg);
    let knob_x = if *on {
        rect.right() - 10.0
    } else {
        rect.left() + 10.0
    };
    p.circle_filled(egui::pos2(knob_x, rect.center().y), 8.0, Color32::WHITE);
    resp
}

fn section_label(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .size(11.0)
            .color(theme::TEXT_MUTED)
            .strong(),
    );
}

// ============================================================================
// Server tab
// ============================================================================

fn server_tab(ui: &mut Ui, app: &mut App) {
    // Surface any backend error at the top of the tab.
    if let Some(err) = app.shared.last_error.lock().unwrap().clone() {
        error_banner(ui, &err);
    }
    match app.server_state {
        ServerState::Idle => server_idle(ui, app),
        ServerState::Connected => server_connected(ui, app),
        ServerState::PortConflict => server_port_conflict(ui, app),
        ServerState::PortResolved => server_port_resolved(ui, app),
        ServerState::PermissionRequired => server_permission_required(ui, app),
    }
    if app.dev_mode {
        ui.add_space(6.0);
        dev_state_switcher_server(ui, &mut app.server_state);
    }
}

fn error_banner(ui: &mut Ui, msg: &str) {
    Frame::none()
        .fill(theme::DANGER_SOFT)
        .stroke(Stroke::new(1.0, theme::DANGER_SOFT_BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(msg)
                    .size(12.0)
                    .color(theme::DANGER)
                    .strong(),
            );
        });
    ui.add_space(6.0);
}

fn server_idle(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    let running = app.is_running() && matches!(app.session_kind(), Some(SessionKind::Server));

    // Icon + "Waiting for client" / "Not running"
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        let (icon_rect, _) =
            ui.allocate_exact_size(Vec2::new(48.0, 44.0), Sense::hover());
        let p = ui.painter();
        let stroke = Stroke::new(1.6, theme::TEXT_MUTED);
        let screen = Rect::from_min_size(
            egui::pos2(icon_rect.left() + 6.0, icon_rect.top() + 4.0),
            Vec2::new(36.0, 26.0),
        );
        p.rect_stroke(screen, Rounding::same(4.0), stroke);
        let stand_top = screen.bottom() + 2.0;
        let stand_bot = stand_top + 6.0;
        let cx = icon_rect.center().x;
        p.line_segment(
            [egui::pos2(cx - 4.0, stand_top), egui::pos2(cx - 6.0, stand_bot)],
            stroke,
        );
        p.line_segment(
            [egui::pos2(cx + 4.0, stand_top), egui::pos2(cx + 6.0, stand_bot)],
            stroke,
        );
        p.line_segment(
            [egui::pos2(cx - 9.0, stand_bot), egui::pos2(cx + 9.0, stand_bot)],
            stroke,
        );
        ui.add_space(10.0);
        ui.label(RichText::new(s.waiting_for_client).size(15.0).strong());
        ui.add_space(4.0);
        ui.label(
            RichText::new(s.start_client_hint_l1)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        ui.label(
            RichText::new(s.start_client_hint_l2)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
    });
    ui.add_space(12.0);

    // Port is editable before the server starts — once running it's locked.
    ui.label(
        RichText::new(s.label_port)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    ui.horizontal(|ui| {
        ui.add_enabled(
            !running,
            TextEdit::singleline(&mut app.port)
                .font(FontId::new(14.0, egui::FontFamily::Proportional))
                .margin(Margin::symmetric(12.0, 8.0))
                .desired_width(110.0),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(s.label_edge)
                .size(11.0)
                .color(theme::TEXT_MUTED),
        );
    });
    // Edge selector (disabled while running).
    ui.add_enabled_ui(!running, |ui| {
        edge_picker(ui, &mut app.edge, s);
    });
    ui.add_space(6.0);
    if running {
        if stop_running_button(ui, s.btn_stop_server).clicked() {
            app.pending_action = Some(Action::StopSession);
        }
    } else {
        // Use the same label regardless of language — the running state is
        // the canonical button text. Reuse `btn_start_server_on` variant.
        let label = match app.lang {
            Lang::En => "Start server",
            Lang::Zh => "启动服务",
        };
        if primary_button(ui, label).clicked() {
            app.pending_action = Some(Action::StartServer);
        }
    }
}

fn server_connected(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    let peer = app.peer_addr_display();
    let uptime = app.uptime_display();
    ui.horizontal(|ui| {
        pill(ui, s.pill_connected, theme::PRIMARY, theme::PRIMARY_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(&peer)
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(8.0);
    let mouse_on_peer = app.shared.mouse_on_peer.load(Ordering::SeqCst);
    topology(
        ui,
        "server",
        "client",
        "",
        Some(s.label_mouse_here),
        Some(s.label_standby),
        !mouse_on_peer,
        mouse_on_peer,
    );
    ui.add_space(18.0);
    let port_str = app.port.clone();
    let edge_str = app.edge.label(s);
    field_trio(
        ui,
        [
            (s.label_port, port_str.as_str()),
            (s.label_edge, edge_str),
            (s.label_up, uptime.as_str()),
        ],
    );
    ui.add_space(6.0);
    toggle(ui, s.toggle_clipboard_sync, &mut app.clipboard_sync);
    toggle(ui, s.toggle_keyboard_fwd, &mut app.keyboard_fwd);
    ui.add_space(4.0);
    if stop_running_button(ui, s.btn_stop_server).clicked() {
        app.pending_action = Some(Action::StopSession);
    }
}

fn server_port_conflict(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    ui.horizontal(|ui| {
        pill(ui, s.pill_port_unavailable, theme::DANGER, theme::DANGER_SOFT);
    });
    ui.add_space(10.0);
    ui.label(
        RichText::new(s.label_server_port)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    ui.horizontal(|ui| {
        Frame::none()
            .fill(theme::CARD_BG)
            .stroke(Stroke::new(1.5, theme::DANGER))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::symmetric(14.0, 10.0))
            .show(ui, |ui| {
                ui.label(RichText::new("4242").size(15.0).strong());
            });
        ui.label(
            RichText::new(s.text_occupied)
                .size(12.0)
                .color(theme::DANGER)
                .strong(),
        );
    });
    ui.add_space(10.0);
    Frame::none()
        .fill(theme::DANGER_SOFT)
        .stroke(Stroke::new(1.0, theme::DANGER_SOFT_BORDER))
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.text_port_in_use)
                    .size(13.0)
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.add_space(8.0);
            Frame::none()
                .fill(theme::CARD_BG)
                .stroke(Stroke::new(1.0, theme::CARD_BORDER))
                .rounding(Rounding::same(8.0))
                .inner_margin(Margin::symmetric(12.0, 8.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("node").size(12.0).strong());
                        ui.label(
                            RichText::new("PID 28451")
                                .size(11.0)
                                .color(theme::TEXT_MUTED)
                                .family(egui::FontFamily::Monospace),
                        );
                    });
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let btn = egui::Button::new(
                    RichText::new(s.btn_use_next).size(12.0).color(Color32::WHITE).strong(),
                )
                .fill(theme::PRIMARY)
                .rounding(Rounding::same(8.0))
                .min_size(Vec2::new(110.0, 30.0));
                ui.add(btn);
                let btn = egui::Button::new(RichText::new(s.btn_retry).size(12.0))
                    .fill(theme::CARD_BG)
                    .stroke(Stroke::new(1.0, theme::CARD_BORDER))
                    .rounding(Rounding::same(8.0))
                    .min_size(Vec2::new(70.0, 30.0));
                ui.add(btn);
                let btn = egui::Button::new(
                    RichText::new(s.btn_kill).size(12.0).color(theme::DANGER).strong(),
                )
                .fill(theme::CARD_BG)
                .stroke(Stroke::new(1.0, theme::DANGER_SOFT_BORDER))
                .rounding(Rounding::same(8.0))
                .min_size(Vec2::new(70.0, 30.0));
                ui.add(btn);
            });
        });
    ui.add_space(10.0);
    section_label(ui, s.label_nearby_ports);
    nearby_port(ui, "4242", "node", true, s);
    nearby_port(ui, "4243", "node", true, s);
    nearby_port(ui, "4244", "—", false, s);
}

fn nearby_port(ui: &mut Ui, port: &str, proc_name: &str, used: bool, s: &Strings) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(port)
                .size(12.0)
                .color(theme::TEXT)
                .family(egui::FontFamily::Monospace),
        );
        ui.add_space(12.0);
        ui.label(RichText::new(proc_name).size(12.0).color(theme::TEXT_MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (fg, bg, txt) = if used {
                (theme::DANGER, theme::DANGER_SOFT, s.chip_used)
            } else {
                (theme::PRIMARY, theme::PRIMARY_SOFT, s.chip_free)
            };
            Frame::none()
                .fill(bg)
                .rounding(Rounding::same(999.0))
                .inner_margin(Margin::symmetric(10.0, 3.0))
                .show(ui, |ui| {
                    ui.label(RichText::new(txt).size(11.0).color(fg).strong());
                });
        });
    });
}

fn server_port_resolved(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    pill(ui, s.pill_ready, theme::PRIMARY, theme::PRIMARY_SOFT);
    ui.add_space(10.0);
    ui.label(
        RichText::new(s.label_server_port)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    ui.horizontal(|ui| {
        Frame::none()
            .fill(theme::CARD_BG)
            .stroke(Stroke::new(1.5, theme::PRIMARY))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::symmetric(14.0, 10.0))
            .show(ui, |ui| {
                ui.label(RichText::new("4244").size(15.0).strong());
            });
        ui.label(
            RichText::new(s.text_available)
                .size(12.0)
                .color(theme::PRIMARY)
                .strong(),
        );
    });
    ui.add_space(10.0);
    Frame::none()
        .fill(theme::PRIMARY_SOFT)
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.text_switched)
                    .size(12.0)
                    .color(theme::PRIMARY),
            );
        });
    ui.add_space(10.0);
    field_pair(ui, s.label_udp_input, ":4244", s.label_tcp_clipboard, ":4245");
    ui.add_space(4.0);
    ui.label(
        RichText::new(s.text_consecutive_ports)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    ui.add_space(6.0);
    primary_button(ui, s.btn_start_server_on);
}

fn server_permission_required(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    // Large orange warning icon
    ui.vertical_centered(|ui| {
        ui.add_space(14.0);
        let (icon_rect, _) =
            ui.allocate_exact_size(Vec2::new(54.0, 54.0), Sense::hover());
        let p = ui.painter();
        p.circle_stroke(
            icon_rect.center(),
            25.0,
            Stroke::new(1.8, theme::WARN),
        );
        // Exclamation mark
        let cx = icon_rect.center().x;
        let top = icon_rect.center().y - 10.0;
        p.line_segment(
            [egui::pos2(cx, top), egui::pos2(cx, top + 14.0)],
            Stroke::new(2.4, theme::WARN),
        );
        p.circle_filled(egui::pos2(cx, top + 20.0), 1.8, theme::WARN);
        ui.add_space(12.0);
        ui.label(RichText::new(s.perm_title).size(15.0).strong());
        ui.add_space(4.0);
        ui.label(
            RichText::new(s.perm_sub_l1)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        ui.label(
            RichText::new(s.perm_sub_l2)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
    });
    ui.add_space(14.0);
    // "How to enable" card (warm/yellow)
    Frame::none()
        .fill(theme::WARN_SOFT)
        .stroke(Stroke::new(1.0, theme::WARN.linear_multiply(0.35)))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.perm_how_to_enable)
                    .size(12.0)
                    .color(theme::WARN)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(s.perm_instructions)
                    .size(11.5)
                    .color(theme::TEXT)
                    .italics(),
            );
        });
    ui.add_space(10.0);
    ui.columns(2, |cols| {
        cols[0].vertical_centered_justified(|ui| {
            primary_button(ui, s.btn_open_settings);
        });
        cols[1].vertical_centered_justified(|ui| {
            ghost_button(ui, s.btn_retry);
        });
    });
}

// ============================================================================
// Client tab
// ============================================================================

fn client_tab(ui: &mut Ui, app: &mut App) {
    if let Some(err) = app.shared.last_error.lock().unwrap().clone() {
        // Only display the banner if the session isn't already routing to
        // NetworkError — otherwise the error is duplicated.
        if !matches!(app.client_state, ClientState::NetworkError) {
            error_banner(ui, &err);
        }
    }
    match app.client_state {
        ClientState::Config => client_config(ui, app),
        ClientState::Connecting => client_connecting(ui, app),
        ClientState::MouseOnServer => client_mouse_on_server(ui, app),
        ClientState::MouseActive => client_mouse_active(ui, app),
        ClientState::NetworkError => client_network_error(ui, app),
    }
    if app.dev_mode {
        ui.add_space(6.0);
        dev_state_switcher_client(ui, &mut app.client_state);
    }
}

fn client_config(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    ui.label(
        RichText::new(s.label_server_address)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(&mut app.server_addr)
                .font(FontId::new(14.0, egui::FontFamily::Proportional))
                .margin(Margin::symmetric(12.0, 8.0))
                .desired_width(ui.available_width() - 90.0),
        );
        ui.add(
            TextEdit::singleline(&mut app.server_port)
                .font(FontId::new(14.0, egui::FontFamily::Proportional))
                .margin(Margin::symmetric(12.0, 8.0))
                .desired_width(70.0),
        );
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new(s.label_screen_edge)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    edge_picker(ui, &mut app.client_edge, s);
    ui.add_space(6.0);
    field_pair(ui, s.label_local_screen, "—", s.label_status, s.status_idle);
    ui.add_space(4.0);
    if primary_button(ui, s.btn_connect).clicked() {
        app.pending_action = Some(Action::StartClient);
    }
}

fn edge_picker(ui: &mut Ui, edge: &mut Edge, s: &Strings) {
    ui.columns(4, |cols| {
        for (i, e) in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
            .iter()
            .enumerate()
        {
            let col = &mut cols[i];
            let selected = *edge == *e;
            let (fill, stroke_c, text_c) = if selected {
                (theme::PRIMARY_SOFT, theme::PRIMARY, theme::PRIMARY)
            } else {
                (theme::CARD_BG, theme::CARD_BORDER, theme::TEXT)
            };
            let resp = Frame::none()
                .fill(fill)
                .stroke(Stroke::new(if selected { 1.5 } else { 1.0 }, stroke_c))
                .rounding(Rounding::same(10.0))
                .inner_margin(Margin::symmetric(0.0, 10.0))
                .show(col, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(e.label(s))
                                .size(13.0)
                                .color(text_c)
                                .strong(),
                        );
                    });
                })
                .response;
            if resp.interact(Sense::click()).clicked() {
                *edge = *e;
            }
        }
    });
}

fn client_connecting(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    let peer = app.peer_addr_display();
    ui.horizontal(|ui| {
        pill(ui, s.pill_connecting, theme::WARN, theme::WARN_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(&peer)
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(10.0);
    ui.label(
        RichText::new(s.label_server_address)
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
    ui.horizontal(|ui| {
        // Address is locked while the session is running — editing it while
        // the client thread uses it would be surprising.
        ui.add_enabled(
            false,
            TextEdit::singleline(&mut app.server_addr)
                .font(FontId::new(14.0, egui::FontFamily::Proportional))
                .margin(Margin::symmetric(12.0, 8.0))
                .desired_width(ui.available_width() - 90.0),
        );
        ui.add_enabled(
            false,
            TextEdit::singleline(&mut app.server_port)
                .font(FontId::new(14.0, egui::FontFamily::Proportional))
                .margin(Margin::symmetric(12.0, 8.0))
                .desired_width(70.0),
        );
    });
    ui.add_space(10.0);
    if danger_button(ui, s.btn_cancel).clicked() {
        app.pending_action = Some(Action::StopSession);
    }
}

fn client_mouse_on_server(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    let peer = app.peer_addr_display();
    let uptime = app.uptime_display();
    ui.horizontal(|ui| {
        pill(ui, s.pill_connected, theme::PRIMARY, theme::PRIMARY_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(&peer)
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(8.0);
    topology(
        ui,
        "server",
        "client",
        "",
        Some(s.label_mouse_here),
        Some(s.label_standby),
        true,
        false,
    );
    ui.add_space(18.0);
    Frame::none()
        .fill(theme::FIELD_BG)
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.text_cursor_hidden)
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    ui.add_space(8.0);
    field_trio(
        ui,
        [
            (s.label_edge, app.client_edge.label(s)),
            (s.label_keys, "—"),
            (s.label_uptime, uptime.as_str()),
        ],
    );
    ui.add_space(4.0);
    if ghost_button(ui, s.btn_disconnect).clicked() {
        app.pending_action = Some(Action::StopSession);
    }
}

fn client_mouse_active(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    let peer = app.peer_addr_display();
    let uptime = app.uptime_display();
    ui.horizontal(|ui| {
        pill(ui, s.pill_receiving_input, theme::INFO, theme::INFO_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(&peer)
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(8.0);
    topology(
        ui,
        "server",
        "client",
        "",
        Some(s.label_suppressed),
        Some(s.label_mouse_here),
        false,
        true,
    );
    ui.add_space(18.0);
    Frame::none()
        .fill(theme::INFO_SOFT)
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.text_cursor_at)
                    .size(12.0)
                    .color(theme::INFO),
            );
        });
    ui.add_space(8.0);
    field_trio(
        ui,
        [
            (s.label_edge, app.client_edge.label(s)),
            (s.label_keys, "—"),
            (s.label_uptime, uptime.as_str()),
        ],
    );
    ui.add_space(4.0);
    if ghost_button(ui, s.btn_disconnect).clicked() {
        app.pending_action = Some(Action::StopSession);
    }
}

fn client_network_error(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    ui.horizontal(|ui| {
        pill(ui, s.pill_connection_lost, theme::DANGER, theme::DANGER_SOFT);
    });
    ui.add_space(12.0);
    Frame::none()
        .fill(theme::DANGER_SOFT)
        .stroke(Stroke::new(1.0, theme::DANGER_SOFT_BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.err_server_unreachable)
                    .size(13.0)
                    .color(theme::DANGER)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(s.err_unreachable_detail)
                    .size(11.5)
                    .color(theme::DANGER),
            );
        });
    ui.add_space(10.0);
    Frame::none()
        .fill(theme::INFO_SOFT)
        .stroke(Stroke::new(1.0, theme::INFO.linear_multiply(0.3)))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(s.err_firewall_check)
                    .size(13.0)
                    .color(theme::INFO)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(s.err_firewall_detail)
                    .size(11.5)
                    .color(theme::INFO),
            );
            ui.add_space(6.0);
            let btn = egui::Button::new(
                RichText::new(s.btn_copy_firewall)
                    .size(11.5)
                    .color(theme::INFO),
            )
            .fill(theme::CARD_BG)
            .stroke(Stroke::new(1.0, theme::INFO.linear_multiply(0.3)))
            .rounding(Rounding::same(8.0));
            ui.add(btn);
        });
    ui.add_space(10.0);
    let mut reconnect = false;
    let mut edit_config = false;
    ui.columns(2, |cols| {
        cols[0].vertical_centered_justified(|ui| {
            if primary_button(ui, s.btn_reconnect).clicked() {
                reconnect = true;
            }
        });
        cols[1].vertical_centered_justified(|ui| {
            if ghost_button(ui, s.btn_edit_config).clicked() {
                edit_config = true;
            }
        });
    });
    if reconnect {
        // Tear down the failed session then immediately start a new one.
        app.pending_action = Some(Action::StopSession);
        // Chain StartClient via a follow-up frame: set a flag on shared so
        // the next derive picks it up. Simpler: just stop, and the user
        // presses Connect again from Config.
    }
    if edit_config {
        app.pending_action = Some(Action::StopSession);
    }
}

// ============================================================================
// Log tab
// ============================================================================

fn log_tab(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    let lines = log_buffer::global().snapshot();

    // Counts per level, used for the filter chips.
    let mut info_count = 0usize;
    let mut warn_count = 0usize;
    let mut err_count = 0usize;
    for l in &lines {
        match l.level {
            log::Level::Info | log::Level::Debug | log::Level::Trace => info_count += 1,
            log::Level::Warn => warn_count += 1,
            log::Level::Error => err_count += 1,
        }
    }
    let total = lines.len();

    ui.horizontal(|ui| {
        log_chip(
            ui,
            &format!("{} {}", s.filter_all.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' '), total),
            theme::TEXT,
            theme::FIELD_BG,
            true,
        );
        log_chip(
            ui,
            &format!("{} {}", s.filter_info.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' '), info_count),
            theme::PRIMARY,
            theme::PRIMARY_SOFT,
            false,
        );
        log_chip(
            ui,
            &format!("{} {}", s.filter_warn.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' '), warn_count),
            theme::WARN,
            theme::WARN_SOFT,
            false,
        );
        log_chip(
            ui,
            &format!("{} {}", s.filter_err.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' '), err_count),
            theme::DANGER,
            theme::DANGER_SOFT,
            false,
        );
    });
    ui.add_space(10.0);
    Frame::none()
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if lines.is_empty() {
                        ui.label(
                            RichText::new("—")
                                .size(12.0)
                                .color(theme::TEXT_SUBTLE),
                        );
                    } else {
                        for line in &lines {
                            render_log_line(ui, line, s);
                        }
                    }
                });
        });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        Frame::none()
            .fill(theme::FIELD_BG)
            .rounding(Rounding::same(8.0))
            .inner_margin(Margin::symmetric(10.0, 6.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!("{} events", total))
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
            });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(s.log_auto_scroll)
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
}

fn render_log_line(ui: &mut Ui, line: &LogLine, s: &Strings) {
    let level = match line.level {
        log::Level::Info | log::Level::Debug | log::Level::Trace => LogLevel::Info,
        log::Level::Warn => LogLevel::Warn,
        log::Level::Error => LogLevel::Err,
    };
    let ts = format_ts(line.ts_ms);
    log_entry(ui, &ts, level, &line.message, s);
}

/// Format a Unix-millis timestamp as HH:MM:SS (UTC). Wall-clock offset isn't
/// worth pulling in a time-zone crate for — the Log tab is for correlating
/// events within a session, not absolute scheduling.
fn format_ts(ms: u64) -> String {
    let secs_total = ms / 1000;
    let tod = secs_total % 86400;
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

#[derive(Clone, Copy)]
enum LogLevel { Info, Warn, Err }

fn log_chip(ui: &mut Ui, text: &str, fg: Color32, bg: Color32, selected: bool) {
    let border = if selected {
        Stroke::new(1.0, theme::CARD_BORDER)
    } else {
        Stroke::NONE
    };
    Frame::none()
        .fill(bg)
        .stroke(border)
        .rounding(Rounding::same(999.0))
        .inner_margin(Margin::symmetric(12.0, 5.0))
        .show(ui, |ui| {
            if !selected && text.contains(' ') {
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 3.5, fg);
                    ui.label(RichText::new(text).size(11.0).color(fg).strong());
                });
            } else {
                ui.label(RichText::new(text).size(11.0).color(fg).strong());
            }
        });
}

fn log_entry(ui: &mut Ui, ts: &str, level: LogLevel, msg: &str, s: &Strings) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(ts)
                .size(11.0)
                .color(theme::TEXT_MUTED)
                .family(egui::FontFamily::Monospace),
        );
        let (label, fg, bg) = match level {
            LogLevel::Info => (s.log_info, theme::PRIMARY, theme::PRIMARY_SOFT),
            LogLevel::Warn => (s.log_warn, theme::WARN, theme::WARN_SOFT),
            LogLevel::Err => (s.log_err, theme::DANGER, theme::DANGER_SOFT),
        };
        Frame::none()
            .fill(bg)
            .rounding(Rounding::same(4.0))
            .inner_margin(Margin::symmetric(6.0, 1.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(label)
                        .size(10.0)
                        .color(fg)
                        .family(egui::FontFamily::Monospace)
                        .strong(),
                );
            });
        ui.label(
            RichText::new(msg)
                .size(12.0)
                .color(theme::TEXT)
                .family(egui::FontFamily::Monospace),
        );
    });
}

// ============================================================================
// Settings tab
// ============================================================================

fn settings_tab(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    egui::ScrollArea::vertical().show(ui, |ui| {
        section_label(ui, s.section_general);
        setting_row(
            ui,
            s.set_start_on_login,
            s.set_start_on_login_sub,
            &mut app.start_on_login,
        );
        setting_row(
            ui,
            s.set_auto_connect,
            s.set_auto_connect_sub,
            &mut app.auto_connect,
        );
        setting_row(
            ui,
            s.set_show_in_menubar,
            s.set_show_in_menubar_sub,
            &mut app.show_in_menubar,
        );
        ui.add_space(8.0);
        section_label(ui, s.section_network);
        ui.label(
            RichText::new(s.set_default_port)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        ui.add(
            TextEdit::singleline(&mut app.default_port)
                .font(FontId::new(14.0, egui::FontFamily::Proportional))
                .margin(Margin::symmetric(12.0, 8.0))
                .desired_width(120.0),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(s.set_default_edge)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        edge_picker(ui, &mut app.default_edge, s);
        ui.add_space(8.0);
        section_label(ui, s.section_language);
        lang_picker(ui, &mut app.lang, s);
        ui.add_space(8.0);
        section_label(ui, s.section_advanced);
        setting_row(
            ui,
            s.set_debug_logging,
            s.set_debug_logging_sub,
            &mut app.debug_logging,
        );
    });
}

fn lang_picker(ui: &mut Ui, lang: &mut Lang, s: &Strings) {
    ui.horizontal(|ui| {
        for (l, label) in [
            (Lang::En, s.lang_english),
            (Lang::Zh, s.lang_chinese),
        ] {
            let selected = *lang == l;
            let (bg, fg, border) = if selected {
                (theme::PRIMARY_SOFT, theme::PRIMARY, theme::PRIMARY)
            } else {
                (theme::FIELD_BG, theme::TEXT, theme::FIELD_BORDER)
            };
            let resp = Frame::none()
                .fill(bg)
                .stroke(Stroke::new(1.0, border))
                .rounding(Rounding::same(6.0))
                .inner_margin(Margin::symmetric(14.0, 6.0))
                .show(ui, |ui| {
                    ui.label(RichText::new(label).size(12.0).color(fg).strong());
                })
                .response
                .interact(Sense::click());
            if resp.clicked() {
                *lang = l;
            }
        }
    });
}

fn setting_row(ui: &mut Ui, label: &str, sub: &str, value: &mut bool) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(13.0).color(theme::TEXT).strong());
            ui.label(
                RichText::new(sub)
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            toggle_switch(ui, value);
        });
    });
    ui.add_space(4.0);
}

// ============================================================================
// Dev state switchers — temporary UI for iterating on the mockup states.
// ============================================================================

fn dev_state_switcher_server(ui: &mut Ui, state: &mut ServerState) {
    ui.add_space(8.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [
            egui::pos2(r.left(), r.top()),
            egui::pos2(r.right(), r.top()),
        ],
        Stroke::new(1.0, theme::FIELD_BORDER),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("dev · state:")
                .size(10.0)
                .color(theme::TEXT_SUBTLE),
        );
        for (s, label) in [
            (ServerState::Idle, "idle"),
            (ServerState::Connected, "connected"),
            (ServerState::PortConflict, "conflict"),
            (ServerState::PortResolved, "resolved"),
            (ServerState::PermissionRequired, "perm"),
        ] {
            let selected = *state == s;
            let color = if selected {
                theme::PRIMARY
            } else {
                theme::TEXT_MUTED
            };
            let resp = ui.add(
                egui::Label::new(RichText::new(label).size(10.0).color(color))
                    .sense(Sense::click()),
            );
            if resp.clicked() {
                *state = s;
            }
        }
    });
}

fn dev_state_switcher_client(ui: &mut Ui, state: &mut ClientState) {
    ui.add_space(8.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [
            egui::pos2(r.left(), r.top()),
            egui::pos2(r.right(), r.top()),
        ],
        Stroke::new(1.0, theme::FIELD_BORDER),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("dev · state:")
                .size(10.0)
                .color(theme::TEXT_SUBTLE),
        );
        for (s, label) in [
            (ClientState::Config, "config"),
            (ClientState::Connecting, "connecting"),
            (ClientState::MouseOnServer, "standby"),
            (ClientState::MouseActive, "active"),
            (ClientState::NetworkError, "error"),
        ] {
            let selected = *state == s;
            let color = if selected {
                theme::PRIMARY
            } else {
                theme::TEXT_MUTED
            };
            let resp = ui.add(
                egui::Label::new(RichText::new(label).size(10.0).color(color))
                    .sense(Sense::click()),
            );
            if resp.clicked() {
                *state = s;
            }
        }
    });
}
