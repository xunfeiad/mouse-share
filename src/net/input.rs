use crate::{
    config::{Edge, ScreenConfig},
    input::{
        capture::{self, CaptureQueue, CapturedInput, Suppression},
        simulate::InputSimulator,
    },
    protocol::{Control, KeyEvent, MouseButton, MouseEvent, Point, Position, ScreenInfo},
};
use anyhow::{ensure, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};
use tokio::sync::mpsc;

pub struct Capture {
    pub events: mpsc::Receiver<CapturedInput>,
    pub suppression: Arc<Suppression>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    hidden: bool,
    #[cfg(test)]
    keep_open: Option<mpsc::Sender<CapturedInput>>,
}

impl Capture {
    pub fn start() -> Result<Self> {
        let mut hook = capture::create_capture();
        let suppression = hook.suppress_handle();
        let (tx, events) = mpsc::channel(512);
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let queue = CaptureQueue {
            tx,
            suppression: suppression.clone(),
        };
        let thread = thread::Builder::new()
            .name("input-capture".into())
            .spawn(move || {
                if let Err(e) = hook.run(queue, stop) {
                    log::warn!("Input capture stopped: {e}");
                }
            })?;
        Ok(Self {
            events,
            suppression,
            shutdown,
            thread: Some(thread),
            hidden: false,
            #[cfg(test)]
            keep_open: None,
        })
    }

    #[cfg(test)]
    pub fn mock() -> Result<Self> {
        Ok(Self::mock_with_input().0)
    }

    #[cfg(test)]
    pub fn mock_with_input() -> (Self, mpsc::Sender<CapturedInput>) {
        let (tx, events) = mpsc::channel(512);
        (
            Self {
                events,
                suppression: Default::default(),
                shutdown: Default::default(),
                thread: None,
                hidden: false,
                keep_open: Some(tx.clone()),
            },
            tx,
        )
    }

    pub fn suppress(&mut self, mouse: bool, keyboard: bool) {
        self.suppression.mouse.store(mouse, Ordering::Release);
        self.suppression
            .keyboard
            .store(mouse && keyboard, Ordering::Release);
        #[cfg(test)]
        if self.keep_open.is_some() {
            return;
        }
        if mouse != self.hidden {
            if mouse {
                capture::hide_local_cursor();
            } else {
                capture::show_local_cursor();
            }
            self.hidden = mouse;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.suppress(false, false);
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
pub struct Held {
    keys: BTreeMap<u32, KeyEvent>,
    buttons: BTreeSet<MouseButton>,
}

pub struct Forward {
    pub geometry: ScreenConfig,
    pub position: Position,
    pub pending: Option<tokio::time::Instant>,
    pub active: bool,
    pub buffered: Vec<CapturedInput>,
    pub motion_dirty: bool,
    pub armed: bool,
    held: Held,
}

impl Forward {
    pub fn new(server: ScreenInfo, client: ScreenInfo, edge: Edge) -> Self {
        let mut geometry = ScreenConfig::new(server, edge);
        geometry.client_screen = Some(client);
        Self {
            geometry,
            position: Position {
                epoch: 0,
                seq: 0,
                control: 0,
                point: (0.0, 0.0),
            },
            pending: None,
            active: false,
            buffered: Vec::new(),
            motion_dirty: false,
            armed: false,
            held: Held::default(),
        }
    }

    pub fn entry(&mut self, dx: f64, dy: f64, x: f64, y: f64) -> Option<Control> {
        let screen = self.geometry.server_screen;
        let outward = match self.geometry.edge {
            Edge::Left => dx < 0.0,
            Edge::Right => dx > 0.0,
            Edge::Top => dy < 0.0,
            Edge::Bottom => dy > 0.0,
        };
        if !outward
            || !self.geometry.at_edge(x, y)
            || !(0.0..f64::from(screen.width)).contains(&x)
            || !(0.0..f64::from(screen.height)).contains(&y)
        {
            return None;
        }
        let point = self
            .geometry
            .client_screen
            .unwrap()
            .clamp(self.geometry.entry_position(x, y));
        self.position = Position {
            epoch: self.position.epoch + 1,
            seq: 0,
            control: 0,
            point,
        };
        self.pending = Some(tokio::time::Instant::now());
        Some(Control::Enter(self.position))
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.pending = None;
        self.motion_dirty = false;
        self.armed = false;
        self.buffered.clear();
        self.held = Held::default();
    }

    fn barrier(&mut self) -> Position {
        self.position.seq += 1;
        self.position.control += 1;
        self.motion_dirty = false;
        self.position
    }

    pub fn snapshot(&mut self) -> Control {
        let position = self.barrier();
        Control::Snapshot(
            position,
            self.held.keys.values().cloned().collect(),
            self.held.buttons.iter().copied().collect(),
        )
    }

    pub fn disable_keyboard(&mut self) {
        self.held.keys.clear();
        self.buffered
            .retain(|event| !matches!(event, CapturedInput::Key(_)));
    }

    pub fn input(&mut self, event: CapturedInput, keyboard: bool) -> Option<Control> {
        match event {
            CapturedInput::Key(key) if keyboard => {
                if key.down {
                    self.held.keys.insert(key.keycode, key.clone());
                } else if self.held.keys.remove(&key.keycode).is_none() {
                    return None;
                }
                Some(Control::Key(self.barrier(), key))
            }
            CapturedInput::Key(_) => None,
            CapturedInput::Mouse { event, .. } => match event {
                MouseEvent::Move { dx, dy } => {
                    let (x, y) = self.position.point;
                    let screen = self.geometry.client_screen.unwrap();
                    let next = (x + dx, y + dy);
                    let (distance, outward) = match self.geometry.edge {
                        Edge::Right => (next.0, dx < 0.0),
                        Edge::Left => (f64::from(screen.width - 1) - next.0, dx > 0.0),
                        Edge::Bottom => (next.1, dy < 0.0),
                        Edge::Top => (f64::from(screen.height - 1) - next.1, dy > 0.0),
                    };
                    if distance >= 20.0 {
                        self.armed = true;
                    }
                    if self.armed && distance <= 0.0 && outward {
                        let epoch = self.position.epoch;
                        self.clear();
                        return Some(Control::Leave(epoch));
                    }
                    self.position.point = screen.clamp(next);
                    self.position.seq += 1;
                    self.motion_dirty = true;
                    None
                }
                MouseEvent::ButtonDown(btn) if self.held.buttons.insert(btn) => {
                    Some(Control::Mouse(self.barrier(), event))
                }
                MouseEvent::ButtonUp(btn) if self.held.buttons.remove(&btn) => {
                    Some(Control::Mouse(self.barrier(), event))
                }
                MouseEvent::Scroll { .. } => Some(Control::Mouse(self.barrier(), event)),
                _ => None,
            },
        }
    }
}

pub struct RemoteInput {
    simulator: Box<dyn InputSimulator>,
    screen: ScreenInfo,
    pub epoch: u64,
    pub active: bool,
    seq: u64,
    control: u64,
    pending: Option<Position>,
    point: Option<Point>,
    held: Held,
}

impl RemoteInput {
    pub fn new(simulator: Box<dyn InputSimulator>, screen: ScreenInfo) -> Self {
        Self {
            simulator,
            screen,
            epoch: 0,
            active: false,
            seq: 0,
            control: 0,
            pending: None,
            point: None,
            held: Held::default(),
        }
    }

    fn move_to(&mut self, point: Point) -> Result<()> {
        ensure!(
            point.0.is_finite() && point.1.is_finite(),
            "Invalid pointer position"
        );
        let point = self.screen.clamp(point);
        if self.point != Some(point) {
            self.simulator.move_to(point.0, point.1)?;
            self.point = Some(point);
        }
        Ok(())
    }

    pub fn enter(&mut self, position: Position) -> Result<()> {
        ensure!(
            position.epoch > self.epoch && position.seq == 0 && position.control == 0,
            "Invalid Enter sequence"
        );
        self.leave()?;
        self.epoch = position.epoch;
        self.seq = 0;
        self.control = 0;
        self.active = true;
        self.move_to(position.point)
    }

    pub fn leave(&mut self) -> Result<()> {
        self.active = false;
        self.pending = None;
        self.point = None;
        let mut result = self.release_keys();
        for button in self.held.buttons.clone() {
            match self.simulator.button_up(button) {
                Ok(()) => {
                    self.held.buttons.remove(&button);
                }
                Err(e) => result = Err(e),
            }
        }
        result
    }

    pub fn release_keys(&mut self) -> Result<()> {
        let mut result = Ok(());
        for key in self.held.keys.values().cloned().collect::<Vec<_>>() {
            match self.simulator.key_event(
                key.keycode,
                false,
                if cfg!(target_os = "macos") {
                    0
                } else {
                    key.flags
                },
            ) {
                Ok(()) => {
                    self.held.keys.remove(&key.keycode);
                }
                Err(e) => result = Err(e),
            }
        }
        result
    }

    pub fn datagram(&mut self, position: Position) {
        if self.active
            && position.epoch == self.epoch
            && position.seq > self.seq
            && position.control >= self.control
            && self
                .pending
                .is_none_or(|pending| position.seq > pending.seq)
        {
            self.pending = Some(position);
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some(position) = self.pending.filter(|p| p.control == self.control) {
            self.pending = None;
            if position.seq > self.seq {
                self.move_to(position.point)?;
                self.seq = position.seq;
            }
        }
        Ok(())
    }

    fn barrier(&mut self, position: Position) -> Result<bool> {
        if !self.active || position.epoch != self.epoch {
            return Ok(false);
        }
        ensure!(
            position.control == self.control + 1 && position.seq > self.seq,
            "Invalid input control sequence"
        );
        self.move_to(position.point)?;
        self.seq = position.seq;
        self.control = position.control;
        if self
            .pending
            .is_some_and(|p| p.seq <= self.seq || p.control < self.control)
        {
            self.pending = None;
        }
        Ok(true)
    }

    fn key(&mut self, key: KeyEvent) -> Result<()> {
        if !key.down && !self.held.keys.contains_key(&key.keycode) {
            return Ok(());
        }
        self.simulator.key_event(key.keycode, key.down, key.flags)?;
        if key.down {
            self.held.keys.insert(key.keycode, key);
        } else {
            self.held.keys.remove(&key.keycode);
        }
        Ok(())
    }

    pub fn apply(&mut self, message: Control, keyboard: bool) -> Result<()> {
        match message {
            Control::Key(position, key) => {
                if self.barrier(position)? && keyboard {
                    self.key(key)?;
                }
            }
            Control::Mouse(position, event) => {
                if self.barrier(position)? {
                    match event {
                        MouseEvent::ButtonDown(b) if !self.held.buttons.contains(&b) => {
                            self.simulator.button_down(b)?;
                            self.held.buttons.insert(b);
                        }
                        MouseEvent::ButtonUp(b) if self.held.buttons.contains(&b) => {
                            self.simulator.button_up(b)?;
                            self.held.buttons.remove(&b);
                        }
                        MouseEvent::Scroll { dx, dy } if dx.is_finite() && dy.is_finite() => {
                            self.simulator.scroll(dx, dy)?
                        }
                        _ => {}
                    }
                }
            }
            Control::Snapshot(position, keys, buttons) if self.barrier(position)? => {
                for old in self.held.keys.values().cloned().collect::<Vec<_>>() {
                    if !keyboard || !keys.iter().any(|k| k.keycode == old.keycode) {
                        let flags = if cfg!(target_os = "macos") {
                            keys.last().map_or(0, |k| k.flags)
                        } else {
                            old.flags
                        };
                        self.key(KeyEvent {
                            down: false,
                            flags,
                            ..old
                        })?;
                    }
                }
                for key in keys {
                    if keyboard && key.down && !self.held.keys.contains_key(&key.keycode) {
                        self.key(key)?;
                    }
                }
                for b in self.held.buttons.clone() {
                    if !buttons.contains(&b) {
                        self.simulator.button_up(b)?;
                        self.held.buttons.remove(&b);
                    }
                }
                for b in buttons {
                    if !self.held.buttons.contains(&b) {
                        self.simulator.button_down(b)?;
                        self.held.buttons.insert(b);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Drop for RemoteInput {
    fn drop(&mut self) {
        if let Err(e) = self.leave() {
            log::warn!("Could not release injected input: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Simulator(Arc<Mutex<Vec<String>>>);
    impl InputSimulator for Simulator {
        fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
            self.0.lock().unwrap().push(format!("{x},{y}"));
            Ok(())
        }
        fn move_relative(&mut self, _: f64, _: f64) -> Result<()> {
            unreachable!()
        }
        fn button_down(&mut self, _: MouseButton) -> Result<()> {
            self.0.lock().unwrap().push("down".into());
            Ok(())
        }
        fn button_up(&mut self, _: MouseButton) -> Result<()> {
            self.0.lock().unwrap().push("up".into());
            Ok(())
        }
        fn scroll(&mut self, _: f64, _: f64) -> Result<()> {
            Ok(())
        }
        fn key_event(&mut self, _: u32, down: bool, _: u64) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(if down { "key down" } else { "key up" }.into());
            Ok(())
        }
    }

    fn pointer(epoch: u64, seq: u64, control: u64, x: f64) -> Position {
        Position {
            epoch,
            seq,
            control,
            point: (x, 10.0),
        }
    }

    #[test]
    fn movement_waits_for_click_and_old_epoch_cannot_restore_input() -> Result<()> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut input = RemoteInput::new(
            Box::new(Simulator(events.clone())),
            ScreenInfo {
                width: 1920,
                height: 1080,
            },
        );
        input.enter(pointer(1, 0, 0, 0.0))?;
        input.datagram(pointer(1, 3, 1, 300.0));
        input.datagram(pointer(1, 1, 0, 100.0));
        input.flush()?;
        assert_eq!(*events.lock().unwrap(), ["0,10"]);
        input.apply(
            Control::Mouse(
                pointer(1, 2, 1, 200.0),
                MouseEvent::ButtonDown(MouseButton::Left),
            ),
            true,
        )?;
        input.flush()?;
        input.apply(
            Control::Key(
                pointer(1, 4, 2, 300.0),
                KeyEvent {
                    keycode: 4,
                    down: true,
                    flags: 0,
                },
            ),
            true,
        )?;
        input.leave()?;
        input.leave()?;
        input.enter(pointer(2, 0, 0, 0.0))?;
        input.datagram(pointer(1, 9, 4, 500.0));
        input.apply(
            Control::Snapshot(pointer(1, 8, 4, 500.0), Vec::new(), vec![MouseButton::Left]),
            true,
        )?;
        input.flush()?;
        assert_eq!(
            *events.lock().unwrap(),
            ["0,10", "200,10", "down", "300,10", "key down", "key up", "up", "0,10"]
        );
        Ok(())
    }

    fn movement(dx: f64, dy: f64) -> CapturedInput {
        CapturedInput::Mouse {
            event: MouseEvent::Move { dx, dy },
            abs_x: 0.0,
            abs_y: 0.0,
        }
    }

    #[test]
    fn edges_and_clamp_then_reverse_are_consistent() {
        let server = ScreenInfo {
            width: 800,
            height: 600,
        };
        let client = ScreenInfo {
            width: 1920,
            height: 1080,
        };
        for (edge, (dx, dy), (x, y), expected) in [
            (Edge::Right, (1.0, 0.0), (799.0, 300.0), (0.0, 540.0)),
            (Edge::Left, (-1.0, 0.0), (0.0, 300.0), (1919.0, 540.0)),
            (Edge::Top, (0.0, -1.0), (400.0, 0.0), (960.0, 1079.0)),
            (Edge::Bottom, (0.0, 1.0), (400.0, 599.0), (960.0, 0.0)),
        ] {
            let mut forward = Forward::new(server, client, edge);
            assert!(forward.entry(-dx, -dy, x, y).is_none());
            assert!(
                matches!(forward.entry(dx, dy, x, y), Some(Control::Enter(p)) if p.point == expected)
            );
            forward.input(movement(dx * 2500.0, dy * 2500.0), true);
            let boundary = forward.position.point;
            forward.input(movement(-dx * 5.0, -dy * 5.0), true);
            assert_eq!(
                forward.position.point,
                (boundary.0 - dx * 5.0, boundary.1 - dy * 5.0)
            );
            assert!(matches!(
                forward.input(movement(-dx * 2500.0, -dy * 2500.0), true),
                Some(Control::Leave(1))
            ));
        }
    }

    #[test]
    fn disabling_keyboard_releases_keys_but_keeps_drag_state() -> Result<()> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut input = RemoteInput::new(
            Box::new(Simulator(events.clone())),
            ScreenInfo {
                width: 800,
                height: 600,
            },
        );
        input.enter(pointer(1, 0, 0, 0.0))?;
        let key = KeyEvent {
            keycode: 56,
            down: true,
            flags: 0,
        };
        input.apply(Control::Key(pointer(1, 1, 1, 0.0), key.clone()), true)?;
        input.apply(
            Control::Mouse(
                pointer(1, 2, 2, 10.0),
                MouseEvent::ButtonDown(MouseButton::Left),
            ),
            true,
        )?;
        input.release_keys()?;
        input.apply(
            Control::Snapshot(pointer(1, 3, 3, 20.0), vec![key], vec![MouseButton::Left]),
            false,
        )?;
        assert!(input.active);
        assert_eq!(
            *events.lock().unwrap(),
            ["0,10", "key down", "10,10", "down", "key up", "20,10"]
        );
        input.leave()?;
        assert_eq!(events.lock().unwrap().last().unwrap(), "up");
        Ok(())
    }

    #[test]
    fn capture_queue_overflow_releases_both_hooks() {
        let suppression = Arc::new(Suppression::default());
        suppression.mouse.store(true, Ordering::Release);
        suppression.keyboard.store(true, Ordering::Release);
        let (tx, _rx) = mpsc::channel(1);
        let queue = CaptureQueue {
            tx,
            suppression: suppression.clone(),
        };
        queue.send(movement(1.0, 0.0));
        queue.send(CapturedInput::Key(KeyEvent {
            keycode: 4,
            down: false,
            flags: 0,
        }));
        assert!(suppression.failed.load(Ordering::Acquire));
        assert!(!suppression.mouse.load(Ordering::Acquire));
        assert!(!suppression.keyboard.load(Ordering::Acquire));
    }
}
