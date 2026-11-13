use anyhow::Result;
use eframe::egui;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIcon, TrayIconBuilder, TrayIconEvent,
};
#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    Open,
    Start,
    Stop,
    Exit,
}
pub struct Tray {
    _icon: TrayIcon,
    status: MenuItem,
    open: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    exit: MenuItem,
    items: [(tray_icon::menu::MenuId, TrayAction); 4],
    last_label: String,
}
impl Tray {
    pub fn new(ctx: &egui::Context) -> Result<Self> {
        let menu = Menu::new();
        let status = MenuItem::new("mouse share", false, None);
        let open = MenuItem::new("Open", true, None);
        let start = MenuItem::new("Start", true, None);
        let stop = MenuItem::new("Stop", false, None);
        let exit = MenuItem::new("Quit", true, None);
        menu.append_items(&[&status, &open, &start, &stop, &exit])?;
        let mut rgba = vec![0; 24 * 24 * 4];
        for y in 2..22 {
            for x in 5..19 {
                let inside = if y < 8 {
                    let dx = x as i32 - 12;
                    let dy = y as i32 - 8;
                    dx * dx + dy * dy < 49
                } else {
                    true
                };
                if inside {
                    let i = (y * 24 + x) * 4;
                    rgba[i..i + 4].copy_from_slice(&[12, 110, 76, 255]);
                }
            }
        }
        let icon = TrayIconBuilder::new()
            .with_tooltip("mouse share")
            .with_menu(Box::new(menu))
            .with_icon(tray_icon::Icon::from_rgba(rgba, 24, 24)?)
            .build()?;
        let items = [
            (open.id().clone(), TrayAction::Open),
            (start.id().clone(), TrayAction::Start),
            (stop.id().clone(), TrayAction::Stop),
            (exit.id().clone(), TrayAction::Exit),
        ];
        // The library installs handlers only once. Keep the native IDs
        // on each Tray and wake egui for the channel events.
        static HANDLERS: std::sync::Once = std::sync::Once::new();
        HANDLERS.call_once(|| {
            let wake = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let _ = events().0.send(Event::Menu(event.id));
                wake.request_repaint();
            }));
            let wake = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                if let TrayIconEvent::Click {
                    id,
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } = event
                {
                    let _ = events().0.send(Event::Icon(id));
                    wake.request_repaint();
                }
            }));
        });
        Ok(Self {
            _icon: icon,
            status,
            open,
            start,
            stop,
            exit,
            items,
            last_label: String::new(),
        })
    }
    pub fn update(&mut self, phase: crate::net::state::Phase, running: bool, zh: bool) {
        use crate::net::state::Phase;
        let label = match (zh, phase) {
            (true, Phase::Idle) => "待机",
            (true, Phase::Listening) => "等待连接",
            (true, Phase::Connecting) => "连接中",
            (true, Phase::Connected) => "已连接",
            (true, Phase::Reconnecting) => "重连中",
            (true, Phase::Stopping) => "停止中",
            (true, Phase::Failed) => "连接失败",
            (false, Phase::Idle) => "Idle",
            (false, Phase::Listening) => "Listening",
            (false, Phase::Connecting) => "Connecting",
            (false, Phase::Connected) => "Connected",
            (false, Phase::Reconnecting) => "Reconnecting",
            (false, Phase::Stopping) => "Stopping",
            (false, Phase::Failed) => "Failed",
        };
        let signature = format!("{label}:{running}:{zh}");
        if self.last_label == signature {
            return;
        }
        self.last_label = signature;
        self.status.set_text(format!("mouse share — {label}"));
        self.open
            .set_text(if zh { "打开窗口" } else { "Open window" });
        self.start
            .set_text(if zh { "启动共享" } else { "Start sharing" });
        self.stop
            .set_text(if zh { "停止共享" } else { "Stop sharing" });
        self.exit.set_text(if zh { "退出" } else { "Quit" });
        self.start.set_enabled(!running);
        self.stop.set_enabled(running && phase != Phase::Stopping);
    }
}

impl Tray {
    pub fn actions(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();
        for event in events().1.lock().unwrap().try_iter() {
            match event {
                Event::Menu(id) => {
                    if let Some((_, action)) = self.items.iter().find(|(item, _)| *item == id) {
                        actions.push(*action);
                    }
                }
                Event::Icon(id) if id == *self._icon.id() => actions.push(TrayAction::Open),
                Event::Icon(_) => {}
            }
        }
        actions
    }
}

enum Event {
    Menu(tray_icon::menu::MenuId),
    Icon(tray_icon::TrayIconId),
}

type Events = (
    std::sync::mpsc::Sender<Event>,
    std::sync::Mutex<std::sync::mpsc::Receiver<Event>>,
);

fn events() -> &'static Events {
    static EVENTS: std::sync::OnceLock<Events> = std::sync::OnceLock::new();
    EVENTS.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        (tx, std::sync::Mutex::new(rx))
    })
}
