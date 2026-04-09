# 光标可见性管理

两台机器共享一个鼠标时，用户心智模型是"**同一时刻全世界只有一个光标**"。如果不做光标隐藏，Server 和 Client 会同时各自显示一个光标，割裂感强烈。本文档解释这个功能的对称设计和实现。

## 一、问题

没有隐藏逻辑时用户的感知：

```
鼠标在 Server 时（理想）：        鼠标在 Client 时（理想）：
   ┌────────┐  ┌────────┐         ┌────────┐  ┌────────┐
   │ Server │  │ Client │         │ Server │  │ Client │
   │   ●    │  │        │         │        │  │   ●    │
   │  可见   │  │  不见   │         │  不见   │  │  可见   │
   └────────┘  └────────┘         └────────┘  └────────┘

没做光标隐藏时（实际）：
   ┌────────┐  ┌────────┐
   │ Server │  │ Client │
   │   ●    │  │   ●    │   ← 两个光标同时存在，用户困惑
   │  真光标 │  │ 模拟光标│
   └────────┘  └────────┘
```

割裂感来源：
- 在 Server 操作时，Client 上也有一个（不动的）光标
- 鼠标推到边缘"进入" Client 后，Server 上的光标被 CGEventTap 抑制停在边缘不动，Client 的光标跟着手移动 —— 用户眼睛要来回扫视

真正"无感切换"必须做到：**任何时刻只有一台机器显示光标**。

## 二、对称设计

核心约束：**同一时刻全局只有一个光标可见**。

| 状态 | Server 光标 | Client 光标 |
|------|-------------|-------------|
| LOCAL（鼠标在 Server） | **可见** | **隐藏** |
| FORWARDING（鼠标在 Client） | **隐藏** | **可见** |

状态转换和两端光标的同步切换：

```
           push to edge
      ┌─────────────────────▶
LOCAL                          FORWARDING
      ◀─────────────────────┐
           move back

LOCAL 进 FORWARDING：
  Server: hide_local_cursor()  ← Server 光标消失
  send Enter msg
  Client: show_local_cursor()  ← Client 光标出现

FORWARDING 进 LOCAL：
  Server: show_local_cursor()  ← Server 光标恢复
  send Leave msg
  Client: hide_local_cursor()  ← Client 光标消失
```

Server 和 Client 是**两台独立运行的进程**，各自只能管自己这一台的光标。它们通过 Enter/Leave 消息协调：Server 在发 Enter 前隐藏自己的光标，Client 在收到 Enter 后显示自己的光标。Leave 方向对称。

状态切换时机：

```
LOCAL ──(at_edge)──▶ FORWARDING
  │                      │
  │                   hide_local_cursor()
  │                      │
  │                (send Enter msg)
  │                      │
  │    ◀──(should_return)┘
  │                      │
  │   show_local_cursor()
  │                      │
  │   (send Leave msg)
  ▼
LOCAL
```

关键点：hide 和 show 必须**严格配对**。每次进入 FORWARDING 一次 hide，每次退出一次 show，否则会出现光标永久消失或计数漂移。

## 三、跨平台实现

### macOS

`CGDisplayHideCursor` / `CGDisplayShowCursor` 是系统级 API，影响所有应用：

```rust
// src/input/capture.rs
pub fn hide_local_cursor() {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        if let Err(e) = CGDisplay::main().hide_cursor() {
            log::warn!("hide_cursor failed: {:?}", e);
        }
    }
}

pub fn show_local_cursor() {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        if let Err(e) = CGDisplay::main().show_cursor() {
            log::warn!("show_cursor failed: {:?}", e);
        }
    }
}
```

**重要：macOS 用引用计数**。每次 `hide_cursor()` 让计数 +1，每次 `show_cursor()` 让计数 -1，只有计数归 0 时光标才真的显示。调用两次 hide 就必须调用两次 show，否则光标永远隐藏。这是最常见的"用完忘记还"类 bug 来源，需要在所有退出路径（正常返回、看门狗超时、channel 断开、panic）都保证 show 被调用。

### Windows（暂未实现）

Windows 的 `ShowCursor(FALSE)` API **只影响当前线程关联的光标状态**，对控制台程序和其他进程没有效果。真正隐藏系统全局光标的方法有两种：

1. **`SetSystemCursor` + 透明光标**：用 `CreateCursor` 或加载一个空的 `.cur` 文件替换系统默认光标。侵入性大，异常退出时需要还原，否则全系统光标都没了。
2. **`WindowFromPoint` + 覆盖透明窗口**：在 Server 屏幕上显示一个全屏透明窗口吞掉鼠标事件。方案复杂。

目前 Windows 侧是 no-op，注释说明了原因。进入 FORWARDING 后 Windows Server 的本地光标仍可见，但由于 `WH_MOUSE_LL` hook 抑制了所有事件，光标不会移动，体验略差但可用。修这个坑不紧急，等有需要时再做。

## 四、生命周期与配对保证

Server 和 Client 各自独立管理自己的光标状态。两端都用显式 `cursor_hidden: bool` 变量追踪当前是否已隐藏 —— 不依赖 macOS 引用计数做幂等，每次切换前先检查状态，保证任何执行路径上都严格配对。

### Server 端（[src/net/server.rs](../src/net/server.rs)）

默认状态 LOCAL，光标可见。进入 FORWARDING 时隐藏，三条退出路径上恢复：

```rust
let mut cursor_hidden = false;

// 进入 FORWARDING：唯一的 hide 点
if config.at
        cursor_hidden = true;
    }
    // ... send Enter ...
}

// 路径 1: 正常返回（用户把光标拉回入口边缘）
if should_return {
    forwarding = false;
    suppress.store(false, Ordering::SeqCst);
    if cursor_hidden {
        capture::show_local_cursor();
        cursor_hidden = false;
    }
    // ... send Leave ...
}

// 路径 2: 看门狗超时（5 秒无事件）
if forwarding && last_forward_time.elapsed() > SUPPRESS_TIMEOUT {
    forwarding = false;
    suppress.store(false, Ordering::SeqCst);
    if cursor_hidden {
        capture::show_local_cursor();
        cursor_hidden = false;
    }
    // ... send Leave ...
}

// 路径 3: capture 线程崩溃（channel 断开）
Err(Disconnected) => {
    suppress.store(false, Ordering::SeqCst);
    if cursor_hidden {
        capture::show_local_cursor();
    }
    break;
}
```

`if cursor_hidden` 是关键保护：即使代码重构后在 LOCAL 状态误入某条退出分支，也不会让引用计数跑到负数。不依赖 `CGDisplayShowCursor` 对负计数的幂等性 —— 靠显式状态机保证正确。

### Client 端（[src/net/client.rs](../src/net/client.rs)）

Client 的默认状态是 LOCAL（对 Client 而言 = 鼠标在 Server，光标应该隐藏）。所以**启动时就调 `hide_local_cursor()`**，然后在 Enter/Leave 消息到达时翻转：

```rust
// 启动：鼠标默认在 Server，隐藏 Client 光标
capture::hide_local_cursor();
let mut cursor_hidden = true;

loop {
    match msg {
        Message::Enter { x, y } => {
            // 鼠标进入 Client，显示光标
            if cursor_hidden {
                capture::show_local_cursor();
                cursor_hidden = false;
            }
            // ... move simulator to (x, y) ...
        }
        Message::Leave => {
            // 鼠标离开 Client，隐藏光标
            if !cursor_hidden {
                capture::hide_local_cursor();
                cursor_hidden = true;
            }
        }
        // ...
    }
}
```

Client 的配对检查比 Server 更重要：UDP 消息可能丢失或乱序，重复的 Enter 或 Leave 到达时，`cursor_hidden` 状态保护让第二次调用变成 no-op，避免引用计数漂移。

### 异常退出的盲区

还有一些情况**不在代码控制中**：

1. **Ctrl+C / SIGINT**：Rust 默认不捕获，进程直接退出，hide 状态残留。用户会发现鼠标消失，必须重启 WindowServer 才能恢复（或者重启系统 UI）。
2. **`panic!`**：panic unwind 时不会执行我们的 show 逻辑。
3. **`kill -9` / SIGKILL**：进程被强杀，没有任何清理机会。

完全防御这三种情况需要：

- 注册 signal handler（`libc::signal` 或 `signal-hook` crate）→ SIGINT/SIGTERM 触发清理
- `std::panic::set_hook` → panic 前恢复光标
- SIGKILL 无解，但可以用外部 supervisor 监测进程死亡后执行 `CGDisplayShowCursor`（比如 launchd 的 `OnDemand`）

当前版本没做这些，属于已知缺陷。一旦 dev 阶段遇到鼠标消失的情况，可以执行：

```bash
# 在另一个终端执行，把光标强制 show 回来
osascript -e 'tell application "System Events" to key code 123'
```

或者临时写一个小工具：

```rust
// recover-cursor.rs
fn main() {
    use core_graphics::display::CGDisplay;
    // show 一大堆次，把计数推回到 0 以上
    for _ in 0..100 {
        let _ = CGDisplay::main().show_cursor();
    }
}
```

## 五、和其他模块的交互

### 与 suppress 标志的关系

注意：`suppress` 只在 Server 端存在，因为只有 Server 有 capture 线程需要吞事件。Client 没有 capture，只有 simulate，所以它只管光标可见性，不管 suppress。

`suppress` 是原子布尔，告诉 capture 线程"把事件吞掉，不要传给本地 OS"。它和光标可见性是**两个独立**的机制：

| | suppress | hide_cursor |
|--|----------|-------------|
| 作用对象 | 事件流 | 视觉呈现 |
| 作用范围 | 本进程的 tap callback | 整个系统的光标渲染 |
| 状态存储 | `Arc<AtomicBool>` | macOS 内部引用计数 |

两者必须同步切换：suppress = true 时光标必须隐藏（否则冻结的光标还在屏幕上），suppress = false 时光标必须显示（否则本地操作没反馈）。同步是通过在 server 事件循环里把两个动作写在相邻行完成的：

```rust
forwarding = true;
suppress.store(true, Ordering::SeqCst);
capture::hide_local_cursor();
```

```rust
forwarding = false;
suppress.store(false, Ordering::SeqCst);
capture::show_local_cursor();
```

### 与 return_armed 的关系

[Bug #3](09-bugfix-log.md) 修复引入的 `return_armed` 状态和光标隐藏是**独立正交**的。return_armed 防止状态机瞬间 Enter→Return 抽搐，即使没有光标隐藏也该修。但两者叠加后用户体验才完整：

- 没有 return_armed：光标会一闪一闪（每次 hide/show 都是可见的跳变）
- 没有 hide_local_cursor：光标不会跳变，但 Server 上永远有一个静止光标陪着

两者都必要。

## 六、未来改进

### 1. 自动恢复 —— signal handler

最高优先级。装一个 SIGINT/SIGTERM handler，无论怎么退出都先 show 一次：

```rust
use signal_hook::{consts::SIGINT, iterator::Signals};

fn install_cleanup() {
    let mut signals = Signals::new(&[SIGINT, SIGTERM]).unwrap();
    std::thread::spawn(move || {
        for _ in signals.forever() {
            for _ in 0..10 {
                let _ = CGDisplay::main().show_cursor();
            }
            std::process::exit(0);
        }
    });
}
```

同时配上 `std::panic::set_hook`：

```rust
std::panic::set_hook(Box::new(|info| {
    for _ in 0..10 {
        let _ = CGDisplay::main().show_cursor();
    }
    eprintln!("panic: {}", info);
}));
```

这样用户手动 Ctrl+C 或者代码 panic 都能保证光标恢复。

### 2. Windows 侧实现

优先级较低，但完整跨平台需要做。推荐方案：透明窗口覆盖 + `SetCursor(NULL)`，避开 `SetSystemCursor` 的全局副作用。

### 3. 显式光标状态追踪

当前通过"调用次数配对"保证正确性，比较脆弱。可以改为在 server 结构体里加一个 `cursor_hidden: bool`，每次切换前检查状态：

```rust
if !self.cursor_hidden {
    capture::hide_local_cursor();
    self.cursor_hidden = true;
}
```

这样即使代码写错多调了一次 hide，也不会让引用计数失衡。代价是多一个状态变量和一点复杂度。

### 4. 淡入淡出动画

锦上添花类改进。Server 光标消失前缓慢透明化，Client 光标出现时渐显。macOS 的 `NSCursor` 不直接支持，需要用 Core Animation。对交互舒适度有一定提升，但实现成本不低。

## 七、调试技巧

如果 hide/show 逻辑出问题，先判断是 Server 还是 Client 的光标出问题 —— 它们是各自独立的状态机：

1. **Server 光标消失且不再出现**：某条退出路径忘了调 `show_local_cursor()`。grep `forwarding = false` 看每一处后面有没有配对的 show（带 `if cursor_hidden` 保护）。

2. **Client 光标消失且不再出现**：检查 Enter 消息处理分支，确认 `show_local_cursor()` 被调用了。也可能 Enter 消息丢失 —— 看 Client 日志里是否有 `Mouse entered at ...`。

3. **Client 启动时光标一直可见**：Client 启动时第一行就应该 `hide_local_cursor()`。如果没起作用，可能是 Client Mac 没给 Accessibility 权限，macOS 在无权限时悄悄失败。

4. **光标一直可见即使在 forwarding**：检查 `hide_cursor()` 的返回值，可能失败（macOS 某些场景需要辅助功能权限）。我们已经在 `hide_local_cursor` 里 log 了失败原因。

5. **光标闪烁**：说明 hide/show 被高频调用。说明状态机在抖动（比如 [Bug #3](09-bugfix-log.md) 描述的 Enter→Return 循环）。先修状态机，再谈光标。

6. **Ctrl+C 后光标消失**：这是已知缺陷，加 signal handler 即可。临时恢复用上面的 `recover-cursor.rs` 或从 Activity Monitor 强制重启 Dock / WindowServer。Server 和 Client 都会中招，都需要恢复。

## 八、小结

| 关注点 | 做法 |
|--------|------|
| 语义 | FORWARDING 时 Server 光标隐藏，LOCAL 时可见 |
| macOS API | `CGDisplay::hide_cursor` / `show_cursor`，内部引用计数 |
| Windows API | 暂无（`ShowCursor` 不够强） |
| 配对保证 | 正常返回、看门狗、channel 断开三处都调 show |
| 异常退出 | 当前未防御 SIGINT/panic/SIGKILL，属已知缺陷 |
| 与 suppress 的关系 | 两个独立机制，必须同步切换 |

光标可见性是那种"不做用户肯定不满意，做了用户完全感觉不到"的功能 —— 真正符合心智模型时，用户根本意识不到曾经可能有两个光标的问题。这就是体验细节的价值。
