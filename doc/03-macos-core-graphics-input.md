# macOS 输入系统：Core Graphics 事件机制

macOS 通过 Core Graphics (Quartz) 框架管理全局输入事件。本项目使用它实现鼠标事件的捕获和模拟。

## 一、macOS 输入事件架构

```
硬件 (鼠标/触控板)
       │
       ▼
┌─────────────┐
│  IOKit HID   │  ← 硬件抽象层
└──────┬──────┘
       ▼
┌─────────────┐
│ WindowServer │  ← 系统级事件分发
└──────┬──────┘
       │
       ├──▶ Event Tap (我们在这里拦截) ←── CGEventTap
       │
       ▼
┌─────────────┐
│  Application │  ← 最终接收事件的应用
└─────────────┘
```

**Event Tap** 是 macOS 提供的事件拦截机制，可以在事件到达应用之前截获、修改或吞掉事件。

## 二、CGEventTap — 事件捕获

### 概念

`CGEventTap` 是一个可以插入到事件流中的"监听点"。它可以配置在不同层级：

| 位置 | 含义 | 用途 |
|------|------|------|
| `CGEventTapLocation::HID` | 最底层，硬件事件级别 | 全局捕获所有输入 |
| `CGEventTapLocation::Session` | 用户会话级别 | 当前登录用户的事件 |
| `CGEventTapLocation::AnnotatedSession` | 带注解的会话级别 | 已被标记的事件 |

### 项目中的实现

```rust
// src/input/macos_capture.rs

let tap = CGEventTap::new(
    CGEventTapLocation::HID,                    // 最底层拦截
    CGEventTapPlacement::HeadInsertEventTap,    // 插入到事件流最前面
    CGEventTapOptions::Default,                 // 可以修改/抑制事件
    events_of_interest,                         // 关注的事件类型列表
    |_proxy, event_type, event| {               // 回调函数
        // 处理事件...
        if suppressing {
            None            // 返回 None → 吞掉事件（不传递给下游）
        } else {
            Some(event.clone())  // 返回 Some → 事件继续传递
        }
    },
)?;
```

### 回调函数签名

```rust
Fn(CGEventTapProxy, CGEventType, &CGEvent) -> Option<CGEvent>
```

- **`CGEventTapProxy`**：代理对象，可以用来向事件流注入新事件
- **`CGEventType`**：事件类型枚举（移动、点击、滚轮等）
- **`&CGEvent`**：事件数据（坐标、按键、修饰键等）
- **返回值**：`Some(event)` 传递事件，`None` 吞掉事件

### 事件类型 (CGEventType)

```rust
pub enum CGEventType {
    MouseMoved = 5,           // 鼠标移动
    LeftMouseDown = 1,        // 左键按下
    LeftMouseUp = 2,          // 左键释放
    RightMouseDown = 3,       // 右键按下
    RightMouseUp = 4,         // 右键释放
    OtherMouseDown = 25,      // 中键/侧键按下
    OtherMouseUp = 26,        // 中键/侧键释放
    ScrollWheel = 22,         // 滚轮
    LeftMouseDragged = 6,     // 左键拖拽（按住移动）
    RightMouseDragged = 7,    // 右键拖拽
    OtherMouseDragged = 27,   // 中键拖拽
    TapDisabledByTimeout = 0xFFFFFFFE,  // 系统禁用了 tap
    TapDisabledByUserInput = 0xFFFFFFFF,
}
```

注意 `Dragged` 系列：macOS 中按住鼠标按钮移动时，产生的是 `Dragged` 而非 `MouseMoved`。必须同时监听两者。

### EventField — 事件字段

```rust
// 读取事件的额外信息
let button_number = event.get_integer_value_field(
    EventField::MOUSE_EVENT_BUTTON_NUMBER  // 哪个按钮（中键=2，侧键=3,4...）
);

let scroll_y = event.get_integer_value_field(
    EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1  // 垂直滚轮 delta
);

let scroll_x = event.get_integer_value_field(
    EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2  // 水平滚轮 delta
);
```

## 三、CFRunLoop — 事件循环

macOS 的事件系统基于 Core Foundation 的 RunLoop 机制。`CGEventTap` 必须绑定到一个 RunLoop 才能接收事件。

```rust
// 1. 将 event tap 的 mach port 转为 RunLoop source
let loop_source = tap.mach_port
    .create_runloop_source(0)?;

// 2. 添加到当前线程的 RunLoop
let run_loop = CFRunLoop::get_current();
run_loop.add_source(&loop_source, kCFRunLoopCommonModes);

// 3. 启用 tap
tap.enable();

// 4. 阻塞运行 RunLoop（永不返回，除非显式停止）
CFRunLoop::run_current();
```

**关键点**：`CFRunLoop::run_current()` 会阻塞当前线程。所以项目中在独立线程运行 capture：

```rust
// src/net/server.rs
std::thread::Builder::new()
    .name("input-capture".into())
    .spawn(move || {
        capturer.run(sender)  // 内部会调用 CFRunLoop::run_current()
    })?;
```

### Mach Port

`CGEventTap` 底层是一个 Mach Port（macOS 内核的 IPC 机制）。事件通过 Mach 消息从 WindowServer 进程发送到我们的进程。`create_runloop_source()` 将这个 Mach Port 包装为 RunLoop 可以监听的事件源。

## 四、CGEvent — 事件模拟

### 创建并发送鼠标事件

```rust
// src/input/macos_simulate.rs

// 1. 创建事件源
let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)?;

// 2. 创建鼠标移动事件
let event = CGEvent::new_mouse_event(
    source,
    CGEventType::MouseMoved,           // 事件类型
    CGPoint::new(100.0, 200.0),        // 目标坐标（绝对位置）
    CGMouseButton::Left,               // 关联按钮（移动事件中通常无关）
)?;

// 3. 发送到事件系统
event.post(CGEventTapLocation::HID);
```

### CGEventSourceStateID

```rust
pub enum CGEventSourceStateID {
    Private,          // 独立状态表（不影响全局键盘/鼠标状态）
    CombinedSession,  // 合并所有事件源的状态
    HIDSystemState,   // 物理硬件的真实状态
}
```

本项目使用 `HIDSystemState`，这样模拟的事件就像来自真实硬件一样。

### 滚轮事件的特殊处理

`core-graphics` crate 的 `new_scroll_event` 需要 `highsierra` feature flag。项目使用替代方案：

```rust
fn scroll(&mut self, _dx: f64, dy: f64) -> Result<()> {
    let source = self.source()?;
    // 创建空事件，手动设置类型和字段
    let event = CGEvent::new(source)?;
    event.set_type(CGEventType::ScrollWheel);
    event.set_integer_value_field(
        EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
        dy as i64,
    );
    event.post(CGEventTapLocation::HID);
    Ok(())
}
```

## 五、权限要求

macOS 从 10.14 (Mojave) 开始，CGEventTap 需要**辅助功能权限**：

```
系统设置 → 隐私与安全性 → 辅助功能 → 添加你的终端应用
```

如果没有权限，`CGEventTap::new()` 返回 `Err`，项目会输出：

```
Failed to create event tap. Please grant Accessibility permission in
System Preferences > Privacy & Security > Accessibility
```

### Rust Crate 对应关系

| macOS 框架 | Rust crate | 用途 |
|-----------|------------|------|
| Core Graphics (Quartz) | `core-graphics` 0.24 | CGEvent, CGEventTap |
| Core Foundation | `core-foundation` 0.10 | CFRunLoop, CFMachPort |

这两个 crate 是 Core Graphics C API 的薄封装，函数和类型几乎一一对应。
