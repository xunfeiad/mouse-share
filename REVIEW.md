# Code Review Report

## 审查结果总览

| 严重性 | 数量 | 状态 |
|--------|------|------|
| 严重 (Critical) | 4 | 已修复 3，保留 1（设计决策） |
| 中等 (Moderate) | 5 | 已修复 4，保留 1 |
| 轻微 (Minor) | 3 | 已修复 1 |

---

## 已修复的问题

### [Critical] 1. 鼠标返回逻辑错误
**问题**：原实现仅根据单次 delta 判断是否返回 Server 屏幕，在 Client 屏幕中央快速移动也会误触发返回。

**修复**：Server 端维护虚拟光标位置，跟踪光标在 Client 屏幕的实际坐标，仅当虚拟光标到达 Client 屏幕的返回边缘时才触发返回。

**文件**：`src/net/server.rs`

### [Critical] 2. 事件抑制无超时保护
**问题**：如果 Client 崩溃或网络中断，Server 端鼠标抑制永远不会释放，用户失去鼠标控制。

**修复**：添加 5 秒看门狗 (SUPPRESS_TIMEOUT)，无事件时自动释放抑制。同时在 channel 断开时也释放抑制。

**文件**：`src/net/server.rs`

### [Critical] 3. Server 端 socket 阻塞导致高延迟
**问题**：event loop 中 UDP socket 设置为 blocking 模式，100ms 超时。每次循环最多等待 110ms，导致事件转发延迟高。

**修复**：进入 event loop 前将 socket 切换为 non-blocking 模式，channel recv_timeout 降至 1ms。

**文件**：`src/net/server.rs`

### [Moderate] 4. 初始 delta 值异常
**问题**：macOS capture 的 last_x/last_y 初始化为 0.0，第一个事件会产生从 (0,0) 到实际光标位置的巨大 delta。

**修复**：初始化时调用 `get_cursor_position()` 获取当前光标位置。

**文件**：`src/input/macos_capture.rs`

### [Moderate] 5. `get_screen_info()` 代码重复
**问题**：server.rs 和 client.rs 各有一份相同的 `get_screen_info()` 函数。

**修复**：提取到 `src/screen.rs` 共享模块。

### [Moderate] 6. macOS simulator 不限制上界
**问题**：`move_relative` 只 clamp 下界为 0，上界无限制。

**修复**：上界 clamp 到 16384（覆盖最大 16K 分辨率）。

**文件**：`src/input/macos_simulate.rs`

### [Moderate] 7. Client 连接不重试
**问题**：Client 仅发送一次 Hello，UDP 丢包则直接失败。

**修复**：改为 10 次重试，每次等待 2 秒。

**文件**：`src/net/client.rs`

### [Minor] 8. 不支持平台的编译提示
**问题**：非 macOS/Windows 平台编译会产生难以理解的错误。

**修复**：添加 `compile_error!` 明确提示。

**文件**：`src/input/capture.rs`, `src/input/simulate.rs`

---

## 保留的问题（设计决策/后续优化）

### [Critical] UDP 无可靠性保证
鼠标 button_down 可能丢包导致按键卡住。当前阶段保留 UDP 设计（低延迟优先），后续可在协议层添加 button 状态同步机制。

### [Moderate] 水平滚轮被忽略
`scroll()` 实现丢弃了 `dx` 参数。需要在 macOS 上设置 `SCROLL_WHEEL_EVENT_DELTA_AXIS_2`，Windows 上处理 `WM_MOUSEHWHEEL`。留作后续迭代。

### [Minor] 无优雅退出
Ctrl+C 会直接杀进程，Windows 上 `UnhookWindowsHookEx` 不会被调用。后续添加 signal handler。

---

## 代码质量评估

- **编译**：macOS 零 warning 通过 ✅
- **跨平台**：trait 抽象清晰，平台代码隔离 ✅
- **线程安全**：capture 线程通过 crossbeam channel 与主线程通信，atomic bool 控制抑制状态 ✅
- **错误处理**：使用 anyhow，无生产代码中的 unwrap ✅
- **序列化**：bincode 二进制格式，低开销 ✅
