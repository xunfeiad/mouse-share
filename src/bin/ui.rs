//! mouse-share GUI — first-pass visual mock built with egui/eframe.
//!
//! Run with:
//!     cargo run --features ui --bin mouse-share-ui
//!
//! This is NOT wired to the real server/client yet — it's a pure visual
//! layer that mirrors the mockup states so we can iterate on look & feel
//! before plugging in behaviour. States can be switched via the small
//! "dev state" dropdown at the bottom of Server/Client tabs.

#![allow(clippy::too_many_lines)]
#![allow(clippy::too_many_arguments)]

use eframe::egui::{
    self, Align, Color32, FontId, Frame, Layout, Margin, Rect, Response, RichText, Rounding,
    Sense, Stroke, TextEdit, Ui, Vec2,
};

// ============================================================================
// Entry point
// ============================================================================

fn main() -> eframe::Result<()> {
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
    label_events: &'static str,
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
    label_events: "Events",
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
    label_events: "事件数",
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
}

impl App {
    /// Return the active string table (static reference — zero allocation).
    fn s(&self) -> &'static Strings {
        match self.lang {
            Lang::En => &EN,
            Lang::Zh => &ZH,
        }
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
                            Tab::Log => log_tab(ui, s),
                            Tab::Settings => settings_tab(ui, self),
                        }
                    });
                });
            });
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
        // Centered title
        let avail = ui.available_width();
        ui.add_space((avail - ui.fonts(|f| f.layout_no_wrap(title.into(), FontId::new(13.0, egui::FontFamily::Proportional), theme::TEXT_MUTED)).size().x) / 2.0 - 26.0);
        ui.label(
            RichText::new(title)
                .size(13.0)
                .color(theme::TEXT_MUTED),
        );
    });
    ui.add_space(12.0);
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

fn field_quad(ui: &mut Ui, labels: [(&str, &str); 4]) {
    ui.columns(4, |cols| {
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
    match app.server_state {
        ServerState::Idle => server_idle(ui, app),
        ServerState::Connected => server_connected(ui, app),
        ServerState::PortConflict => server_port_conflict(ui, app),
        ServerState::PortResolved => server_port_resolved(ui, app),
        ServerState::PermissionRequired => server_permission_required(ui, app),
    }
    ui.add_space(6.0);
    dev_state_switcher_server(ui, &mut app.server_state);
}

fn server_idle(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    // Icon + "Waiting for client"
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        let (icon_rect, _) =
            ui.allocate_exact_size(Vec2::new(48.0, 44.0), Sense::hover());
        // Monitor icon: rounded screen rectangle + short stand + base line.
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
    field_pair(ui, s.label_port, &app.port, s.label_edge, app.edge.label(s));
    field_pair(ui, s.label_local_ip, "192.168.1.100", s.label_screen, "1920x1080");
    ui.add_space(6.0);
    danger_button(ui, s.btn_stop_server);
}

fn server_connected(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    ui.horizontal(|ui| {
        pill(ui, s.pill_connected, theme::PRIMARY, theme::PRIMARY_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new("192.168.1.42")
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(8.0);
    topology(
        ui,
        "1920x1080",
        "2560x1440",
        "2ms",
        Some("MacBook"),
        Some("Desktop"),
        false,
        false,
    );
    ui.add_space(18.0);
    field_quad(
        ui,
        [
            (s.label_port, app.port.as_str()),
            (s.label_edge, app.edge.label(s)),
            (s.label_events, "12k"),
            (s.label_up, "14m"),
        ],
    );
    ui.add_space(6.0);
    toggle(ui, s.toggle_clipboard_sync, &mut app.clipboard_sync);
    toggle(ui, s.toggle_keyboard_fwd, &mut app.keyboard_fwd);
    ui.add_space(4.0);
    Frame::none()
        .fill(theme::PRIMARY_SOFT)
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.painter().circle_filled(
                    ui.cursor().min + Vec2::new(0.0, 7.0),
                    3.5,
                    theme::PRIMARY,
                );
                ui.add_space(10.0);
                ui.label(
                    RichText::new(s.clipboard_hello)
                        .size(12.0)
                        .color(theme::PRIMARY),
                );
            });
        });
    ui.add_space(4.0);
    ghost_button(ui, s.btn_stop_server);
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
    match app.client_state {
        ClientState::Config => client_config(ui, app),
        ClientState::Connecting => client_connecting(ui, app),
        ClientState::MouseOnServer => client_mouse_on_server(ui, app),
        ClientState::MouseActive => client_mouse_active(ui, app),
        ClientState::NetworkError => client_network_error(ui, app),
    }
    ui.add_space(6.0);
    dev_state_switcher_client(ui, &mut app.client_state);
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
    field_pair(ui, s.label_local_screen, "2560x1440", s.label_status, s.status_idle);
    ui.add_space(4.0);
    primary_button(ui, s.btn_connect);
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
    ui.horizontal(|ui| {
        pill(ui, s.pill_connecting, theme::WARN, theme::WARN_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(s.text_attempt)
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
    ui.add_space(10.0);
    ui.columns(3, |cols| {
        field(&mut cols[0], s.label_screen, "2560x1440");
        field(&mut cols[1], s.label_edge, s.edge_right);
        Frame::none()
            .fill(theme::WARN_SOFT)
            .stroke(Stroke::new(1.0, theme::WARN))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::symmetric(14.0, 10.0))
            .show(&mut cols[2], |ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(s.label_status)
                            .size(11.0)
                            .color(theme::TEXT_MUTED),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(s.status_retry)
                            .size(15.0)
                            .color(theme::WARN)
                            .strong(),
                    );
                });
            });
    });
    ui.add_space(8.0);
    danger_button(ui, s.btn_cancel);
}

fn client_mouse_on_server(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    ui.horizontal(|ui| {
        pill(ui, s.pill_connected, theme::PRIMARY, theme::PRIMARY_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new("192.168.1.100")
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(8.0);
    topology(
        ui,
        "1920x1080",
        "2560x1440",
        "2ms",
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
    field_quad(ui, [(s.label_latency, "2ms"), (s.label_events, "0"), (s.label_keys, "0"), (s.label_uptime, "3m")]);
    ui.add_space(4.0);
    ghost_button(ui, s.btn_disconnect);
}

fn client_mouse_active(ui: &mut Ui, app: &mut App) {
    let s = app.s();
    ui.horizontal(|ui| {
        pill(ui, s.pill_receiving_input, theme::INFO, theme::INFO_SOFT);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new("192.168.1.100")
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
    ui.add_space(8.0);
    topology(
        ui,
        "1920x1080",
        "2560x1440",
        "2ms",
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
    field_quad(ui, [(s.label_latency, "2ms"), (s.label_events, "1.2k"), (s.label_keys, "84"), (s.label_uptime, "5m")]);
    ui.add_space(4.0);
    ghost_button(ui, s.btn_disconnect);
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
    ui.columns(2, |cols| {
        cols[0].vertical_centered_justified(|ui| {
            primary_button(ui, s.btn_reconnect);
        });
        cols[1].vertical_centered_justified(|ui| {
            ghost_button(ui, s.btn_edit_config);
        });
    });
}

// ============================================================================
// Log tab
// ============================================================================

fn log_tab(ui: &mut Ui, s: &Strings) {
    ui.horizontal(|ui| {
        log_chip(ui, s.filter_all, theme::TEXT, theme::FIELD_BG, true);
        log_chip(ui, s.filter_info, theme::PRIMARY, theme::PRIMARY_SOFT, false);
        log_chip(ui, s.filter_warn, theme::WARN, theme::WARN_SOFT, false);
        log_chip(ui, s.filter_err, theme::DANGER, theme::DANGER_SOFT, false);
    });
    ui.add_space(10.0);
    Frame::none()
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    log_entry(ui, "14:32:01", LogLevel::Info, s.log_msg_server_on, s);
                    log_entry(ui, "14:32:01", LogLevel::Info, s.log_msg_clipboard_tcp, s);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let r = ui.available_rect_before_wrap();
                        ui.painter().line_segment(
                            [
                                egui::pos2(r.left(), r.top() + 6.0),
                                egui::pos2(r.left() + 120.0, r.top() + 6.0),
                            ],
                            Stroke::new(1.0, theme::FIELD_BORDER),
                        );
                        ui.add_space(128.0);
                        ui.label(
                            RichText::new(s.log_connected_sep)
                                .size(10.0)
                                .color(theme::TEXT_MUTED),
                        );
                        let r2 = ui.available_rect_before_wrap();
                        ui.painter().line_segment(
                            [
                                egui::pos2(r2.left() + 6.0, r2.top() + 6.0),
                                egui::pos2(r2.right(), r2.top() + 6.0),
                            ],
                            Stroke::new(1.0, theme::FIELD_BORDER),
                        );
                    });
                    ui.add_space(4.0);
                    log_entry(ui, "14:32:08", LogLevel::Info, s.log_msg_client, s);
                    log_entry(ui, "14:32:09", LogLevel::Warn, s.log_msg_nodelay, s);
                    log_entry(ui, "14:32:15", LogLevel::Info, s.log_msg_entered, s);
                    log_entry(ui, "14:32:18", LogLevel::Info, s.log_msg_returned, s);
                    log_entry(ui, "14:33:01", LogLevel::Err, s.log_msg_clip_send_reset, s);
                    log_entry(ui, "14:33:05", LogLevel::Info, s.log_msg_clip_reconnected, s);
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
                    RichText::new(s.log_events_duration)
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
