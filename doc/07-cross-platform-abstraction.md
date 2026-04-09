# 跨平台抽象：Trait + cfg 条件编译

本项目需要在 macOS 和 Windows 上使用完全不同的 OS API。Rust 的 trait 和 `cfg` 属性提供了零开销的跨平台抽象。

## 一、核心 Trait

### InputCapture — 输入捕获

```rust
// src/input/capture.rs

pub trait InputCapture: Send {
    /// 启动捕获循环，通过 sender 发送捕获到的事件
    /// 阻塞调用线程
    fn run(&mut self, sender: Sender<MouseEvent>) -> Result<()>;

    /// 获取抑制标志的句柄
    fn suppress_handle(&self) -> Arc<AtomicBool>;
}
```

### InputSimulator — 输入模拟

```rust
// src/input/simulate.rs

pub trait InputSimulator: Send {
    fn move_to(&mut self, x: f64, y: f64) -> Result<()>;
    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()>;
    fn button_down(&mut self, button: MouseButton) -> Result<()>;
    fn button_up(&mut self, button: MouseButton) -> Result<()>;
    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()>;
}
```

### 设计原则

1. **`: Send`** — Trait 要求实现者可以跨线程传递（因为 capture 在独立线程运行）
2. **`&mut self`** — 允许实现者维护内部状态（如虚拟光标位置）
3. **`Result<()>`** — 统一用 `anyhow::Result` 传播平台特定错误

## 二、条件编译 — `#[cfg]`

### 模块级 cfg

```rust
// src/input/mod.rs

#[cfg(target_os = "macos")]
pub mod macos_capture;
#[cfg(target_os = "macos")]
pub mod macos_simulate;

#[cfg(target_os = "windows")]
pub mod win_capture;
#[cfg(target_os = "windows")]
pub mod win_simulate;
```

在 macOS 上编译时，`win_capture.rs` 和 `win_simulate.rs` **完全不参与编译**，不会产生任何 Windows API 的依赖错误。

### 工厂函数

```rust
// src/input/capture.rs

pub fn create_capture() -> Box<dyn InputCapture> {
    #[cfg(target_os = "macos")]
    {
        Box::new(super::macos_capture::MacOsCapture::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(super::win_capture::WinCapture::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        compile_error!("Unsupported platform.");
    }
}
```

`compile_error!` 在不支持的平台上产生编译错误并给出明确提示。

### Cargo.toml 中的条件依赖

```toml
[target.'cfg(target_os = "macos")'.dependencies]
core-graphics = "0.24"
core-foundation = "0.10"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = [...] }
```

macOS 上不会下载和编译 `windows` crate，反之亦然。

## 三、Trait Object 动态分发

```rust
let mut capturer: Box<dyn InputCapture> = create_capture();
capturer.run(sender)?;
```

`Box<dyn InputCapture>` 是 trait object，通过 vtable 动态分发。调用 `run()` 时有一次指针间接寻址（约 1ns）。对于一帧 8ms 的鼠标事件来说，完全可以忽略。

### 为什么不用泛型？

泛型方案：

```rust
fn run_server<C: InputCapture>(capturer: C) { ... }
```

泛型是零开销的，但会导致 `run_server` 在每个平台编译不同的版本，增加代码膨胀。对于本项目只有两个平台实现的场景，trait object 的简洁性更有价值。

## 四、平台实现对比

### 捕获

| | macOS | Windows |
|--|-------|---------|
| API | `CGEventTap::new()` | `SetWindowsHookExW(WH_MOUSE_LL)` |
| 事件循环 | `CFRunLoop::run_current()` | `GetMessageW()` 消息泵 |
| 回调形式 | 闭包 `Fn -> Option<CGEvent>` | `extern "system" fn -> LRESULT` |
| 上下文传递 | 闭包捕获 | `thread_local!` |
| 抑制方式 | 返回 `None` | 返回 `LRESULT(1)` |
| 权限 | 辅助功能权限 | 管理员权限 |

### 模拟

| | macOS | Windows |
|--|-------|---------|
| API | `CGEvent::new_mouse_event()` + `post()` | `SendInput()` |
| 坐标系 | 绝对像素坐标 | 归一化坐标 (0-65535) |
| 事件源 | `CGEventSource(HIDSystemState)` | `INPUT.mi.dwFlags` |
| 滚轮 | 手动设置 `EventField` | `MOUSEEVENTF_WHEEL` + `mouseData` |

### 屏幕信息

```rust
// src/screen.rs — 共享模块，同一函数两个 cfg 分支

#[cfg(target_os = "macos")]
{
    let display = CGDisplay::main();
    Ok(ScreenInfo {
        width: display.pixels_wide() as u32,
        height: display.pixels_high() as u32,
    })
}

#[cfg(target_os = "windows")]
{
    Ok(ScreenInfo {
        width: GetSystemMetrics(SM_CXSCREEN) as u32,
        height: GetSystemMetrics(SM_CYSCREEN) as u32,
    })
}
```

## 五、添加新平台的步骤

以 Linux 为例：

1. 创建 `src/input/linux_capture.rs`，实现 `InputCapture` trait（使用 evdev/libinput）
2. 创建 `src/input/linux_simulate.rs`，实现 `InputSimulator` trait（使用 uinput）
3. 在 `src/input/mod.rs` 添加 `#[cfg(target_os = "linux")] pub mod linux_capture;`
4. 在工厂函数中添加 `#[cfg(target_os = "linux")]` 分支
5. 在 `Cargo.toml` 添加 Linux 条件依赖
6. 在 `src/screen.rs` 添加 Linux 的屏幕信息获取

**不需要修改任何网络层或协议层代码**。这就是 trait 抽象的价值。

## 六、clap — CLI 框架

本项目使用 `clap` derive 宏构建命令行接口：

```rust
// src/main.rs

#[derive(Parser)]
#[command(name = "mouse-share")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Server {
        #[arg(short, long, default_value_t = 4242)]
        port: u16,
        #[arg(short, long, default_value = "right")]
        edge: String,
    },
    Client {
        #[arg(short, long)]
        server: String,
    },
}
```

`clap` derive 宏在编译时生成参数解析代码，包括：
- 帮助文本 (`--help`)
- 类型校验（`port` 自动解析为 `u16`）
- 默认值
- 子命令分发

单二进制 + 子命令是本项目的分发策略：同一个可执行文件既可以作为 Server 也可以作为 Client。
