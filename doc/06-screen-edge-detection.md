# 屏幕边缘检测与虚拟光标算法

本项目的核心交互逻辑：检测鼠标何时离开 Server 屏幕，以及何时从 Client 屏幕返回。

## 一、整体流程

```
状态机:
                at_edge()
  [LOCAL] ──────────────────▶ [FORWARDING]
     ▲                              │
     │      should_return           │
     └──────────────────────────────┘

LOCAL 状态:
  - 鼠标事件正常传递给本地系统
  - 每帧检查光标是否到达屏幕边缘

FORWARDING 状态:
  - 抑制本地鼠标事件
  - 通过 UDP 转发给 Client
  - 维护虚拟光标位置
  - 检查虚拟光标是否到达返回边缘
```

## 二、边缘检测

### 屏幕坐标系

macOS 和 Windows 的屏幕坐标系：

```
macOS:                    Windows:
(0,0) ─────▶ x           (0,0) ─────▶ x
  │                         │
  ▼                         ▼
  y                         y

原点在左上角，x 向右增大，y 向下增大
```

### at_edge() 算法

```rust
// src/config.rs

pub fn at_edge(&self, x: f64, y: f64) -> bool {
    let w = self.server_screen.width as f64;
    let h = self.server_screen.height as f64;
    match self.edge {
        Edge::Right  => x >= w - 1.0,   // 光标 x 坐标到达最右列
        Edge::Left   => x <= 0.0,       // 光标 x 坐标到达最左列
        Edge::Bottom => y >= h - 1.0,   // 光标 y 坐标到达最底行
        Edge::Top    => y <= 0.0,       // 光标 y 坐标到达最顶行
    }
}
```

**为什么是 `w - 1.0` 而不是 `w`？** 屏幕像素坐标从 0 到 width-1。1920 宽的屏幕，有效 x 范围是 [0, 1919]。当 x = 1919 时，光标已在最右边缘。

### 获取当前光标位置

```rust
// src/input/capture.rs

// macOS: 创建一个空事件并读取其位置
let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)?;
let event = CGEvent::new(source)?;
let pos = event.location();  // 返回当前光标的 CGPoint

// Windows: 直接调用 GetCursorPos
let mut point = POINT::default();
unsafe { GetCursorPos(&mut point)? };
```

## 三、坐标映射

### 问题

Server 屏幕 1920x1080，Client 屏幕 2560x1440。光标从 Server 右边缘 (1919, 540) 进入 Client 时，y 坐标应该映射到多少？

### 等比映射算法

```rust
// src/config.rs

pub fn entry_position(&self, x: f64, y: f64) -> (f64, f64) {
    let cw = client.width as f64;   // Client 屏幕宽
    let ch = client.height as f64;  // Client 屏幕高
    let sw = server.width as f64;   // Server 屏幕宽
    let sh = server.height as f64;  // Server 屏幕高

    match self.edge {
        // Client 在 Server 右边：从 Client 左边缘进入
        Edge::Right  => (0.0,      y / sh * ch),
        // Client 在 Server 左边：从 Client 右边缘进入
        Edge::Left   => (cw - 1.0, y / sh * ch),
        // Client 在 Server 下面：从 Client 上边缘进入
        Edge::Bottom => (x / sw * cw, 0.0),
        // Client 在 Server 上面：从 Client 下边缘进入
        Edge::Top    => (x / sw * cw, ch - 1.0),
    }
}
```

示例（Edge::Right）：
```
Server (1920×1080)         Client (2560×1440)

     y=540                      y = 540/1080 * 1440 = 720
─────────┐          ┌─────────
         │──────────│
         │          │
─────────┘          └─────────
  x=1919              x=0
```

光标从 Server 的 `(1919, 540)` 映射到 Client 的 `(0, 720)`。y 坐标按屏幕高度等比缩放。

## 四、虚拟光标追踪

### 问题

进入 FORWARDING 状态后，本地光标被抑制（无法移动）。那怎么知道光标在 Client 屏幕上的位置？

### 解决方案

Server 端维护一个**虚拟光标**，累加从 Capture 线程收到的 delta：

```rust
// src/net/server.rs

// 进入 Client 时初始化
client_cursor_x = entry_x;
client_cursor_y = entry_y;

// 每收到一个 Move 事件，累加 delta
client_cursor_x += event.dx;
client_cursor_y += event.dy;

// 边界裁剪
client_cursor_x = client_cursor_x.clamp(0.0, cw - 1.0);
client_cursor_y = client_cursor_y.clamp(0.0, ch - 1.0);
```

### 返回检测

当虚拟光标到达 Client 屏幕的**对边**时，认为用户要将鼠标移回 Server：

```rust
let should_return = match config.edge {
    Edge::Right  => client_cursor_x <= 0.0,      // 从右进入 → 碰到左边返回
    Edge::Left   => client_cursor_x >= cw - 1.0, // 从左进入 → 碰到右边返回
    Edge::Bottom => client_cursor_y <= 0.0,       // 从下进入 → 碰到上边返回
    Edge::Top    => client_cursor_y >= ch - 1.0,  // 从上进入 → 碰到下边返回
};
```

### 为什么不用 Client 反馈？

一种方案是 Client 检测到光标到达边缘后发 `Return` 消息给 Server。但这有两个问题：

1. **额外 RTT**：Client → Server 的 UDP 往返增加延迟
2. **丢包风险**：`Return` 消息丢失会导致状态不同步

Server 端虚拟追踪是零延迟、零丢包风险的方案。

## 五、看门狗机制

### 问题

如果 Client 崩溃、网络断开，Server 的抑制标志永远不会被清除，用户失去鼠标控制。

### 解决方案

```rust
// src/net/server.rs

const SUPPRESS_TIMEOUT: Duration = Duration::from_secs(5);

// 每次转发事件时更新时间戳
last_forward_time = Instant::now();

// 每帧检查超时
if forwarding && last_forward_time.elapsed() > SUPPRESS_TIMEOUT {
    log::warn!("Suppression watchdog triggered, releasing mouse");
    forwarding = false;
    suppress.store(false, Ordering::SeqCst);
}
```

5 秒无鼠标事件是不正常的（用户不可能 5 秒不动鼠标），自动释放抑制。

## 六、相对坐标 vs 绝对坐标

### 项目选择：相对 Delta

```rust
pub struct MouseEvent {
    pub dx: f64,    // 相对位移
    pub dy: f64,
    ...
}
```

**优势**：
- 不同分辨率间无需每帧缩放
- 丢包只导致微小偏移，下帧自动纠正
- DPI 敏感度天然正确（delta 已包含 DPI 信息）

**劣势**：
- 累积误差（理论上，实际上误差极小）
- 首帧需要 `Enter { x, y }` 给出绝对位置

### 首帧绝对定位

进入 Client 屏幕时发送一次绝对坐标：

```rust
// Server 端
Message::Enter { x: entry_x, y: entry_y }

// Client 端
simulator.move_to(x, y);  // 绝对定位
// 之后全部是相对移动
simulator.move_relative(event.dx, event.dy);
```
