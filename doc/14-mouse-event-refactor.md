# MouseEvent 协议类型重构

本文档记录 `MouseEvent` / `MouseEventType` 合并为单一枚举的设计动机、
具体变更和注意事项。

## 问题：旧设计语义模糊

重构前，鼠标事件由一个 **struct + enum** 组合表达：

```rust
// 旧设计
struct MouseEvent {
    dx: f64,
    dy: f64,
    event_type: MouseEventType,
}

enum MouseEventType {
    Move,
    ButtonDown(MouseButton),
    ButtonUp(MouseButton),
    Scroll { dx: f64, dy: f64 },
}
```

存在三个问题：

| 问题 | 说明 |
|------|------|
| **Move 不自带 dx/dy** | 移动量藏在外层 struct 的 `dx`/`dy` 字段里，阅读 `MouseEventType::Move` 看不出它靠外部字段传递数据 |
| **Scroll 的 dx/dy 含义冲突** | 外层 struct 的 `dx`/`dy` 表示"物理鼠标位移"，`Scroll { dx, dy }` 表示"滚轮滚动量"——同名不同义，容易混淆 |
| **Button 事件携带无用字段** | `ButtonDown`/`ButtonUp` 不需要 dx/dy，但 struct 强制它们带上，浪费 16 字节且暗示这些字段有意义 |

对新贡献者来说，看到一个 `MouseEvent { dx: 3.0, dy: -1.0, event_type: ButtonDown(Left) }` 会疑惑：
"按下按钮为什么需要 dx/dy？这个 3.0 是给谁用的？"

## 解决方案：单一 enum，各变体自带数据

```rust
// 新设计 — src/protocol.rs
pub enum MouseEvent {
    Move { dx: f64, dy: f64 },       // dx/dy = 鼠标物理位移
    ButtonDown(MouseButton),           // 只需要知道哪个按钮
    ButtonUp(MouseButton),             // 同上
    Scroll { dx: f64, dy: f64 },      // dx/dy = 滚轮滚动量
}
```

**原则：每个变体只携带自己的语义数据，不多不少。**

- `Move` 的 dx/dy 是"鼠标位移"——直接在变体里，含义清晰。
- `Scroll` 的 dx/dy 是"滚轮量"——和 Move 的 dx/dy 不会混在同一个 struct 中。
- `ButtonDown`/`ButtonUp` 不再携带冗余字段。

## bincode 序列化兼容性

> **此变更与旧版不兼容。**

bincode 是位置序列化（不编码字段名），enum 变体的标签 + 内容布局已经改变。
新旧客户端/服务端不能混用——需要同步升级两端。在当前开发阶段这是可以接受
的；如果将来需要协议版本控制，应在 `Message` 层增加版本号字段。

## 涉及文件及修改摘要

| 文件 | 变更 |
|------|------|
| `src/protocol.rs` | 删除 `MouseEventType`，`MouseEvent` 从 struct 改为 enum |
| `src/input/macos_capture.rs` | `map_event_type()` → `map_event()`，直接返回 `MouseEvent` 变体，Move 事件的 dx/dy 从参数传入 |
| `src/input/win_capture.rs` | hook 回调中直接构造 `MouseEvent::Move { dx, dy }` 等变体，删除中间 `MouseEventType` 层 |
| `src/net/server.rs` | 光标追踪只从 `MouseEvent::Move { dx, dy }` 提取位移（旧代码对所有事件类型都累加 dx/dy）；`flush_pending_move` 宏构造 `MouseEvent::Move` |
| `src/net/client.rs` | `match event` 直接解构枚举变体，`ButtonDown(btn)` / `ButtonUp(btn)` 不再需要解引用 |

## 服务端光标追踪的行为变化

旧代码在 forwarding 路径对**所有**事件执行 `client_cursor_x += event.dx`：

```rust
// 旧 — server.rs
client_cursor_x += event.dx;  // 对 ButtonDown 也累加
client_cursor_y += event.dy;
```

新代码只对 `Move` 事件累加：

```rust
// 新 — server.rs
if let MouseEvent::Move { dx, dy } = &event {
    client_cursor_x += dx;
    client_cursor_y += dy;
}
```

**影响：**几乎为零。按钮事件的物理位移通常是 0（用户点击时手不动），
即使有微小位移，它已被之前 flush 的 Move 事件覆盖（服务端先 flush
累积的 Move，再发送 Button）。这个变更使语义更准确：按钮事件不移动光标。

## 设计收益

1. **可读性** — 看到 `MouseEvent::Move { dx, dy }` 立刻知道这是移动事件以及它的数据。
2. **类型安全** — 无法构造"有位移的按钮事件"这种无意义组合，编译器会阻止。
3. **匹配更简洁** — `match event { MouseEvent::Move { dx, dy } => ... }` 一步到位，不再需要先访问 `.event_type` 再访问 `.dx`。
4. **更小的序列化体积** — `ButtonDown(Left)` 只序列化变体标签 + 按钮枚举，不再额外序列化两个 f64（节省 16 字节/事件）。
