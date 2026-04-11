# mouse-share

[English](README.md)

通过 WiFi 局域网在多台电脑之间共享鼠标控制。支持 macOS 和 Windows 跨平台使用。

## 工作原理

```
┌─────────────────┐              ┌─────────────────┐
│  Server (控制端)  │── UDP ──────▶│  Client (被控端)  │
│  捕获鼠标事件     │   鼠标事件    │  模拟鼠标输入     │
│  边缘检测切换     │◀── UDP ─────│  心跳保活        │
└─────────────────┘              └─────────────────┘
```

- **Server 端**：运行在主控电脑上，捕获全局鼠标事件。当鼠标移动到屏幕边缘时，自动切换到 Client 端控制。
- **Client 端**：运行在被控电脑上，接收 Server 发来的鼠标事件并模拟执行。

## 快速开始

### 编译

```bash
# 需要 Rust 工具链 (https://rustup.rs/)
cargo build --release
```

CLI 可执行文件位于 `target/release/mouse-share`。

### 图形界面（macOS）

项目自带一个小型 GUI，将 server/client 统一封装到一个窗口里。在 macOS 上推荐
以 `.app` 形式运行——这样「辅助功能」权限绑定在 bundle identifier 上，重新
编译后不需要重新授权：

```bash
./scripts/build-app.sh
open "dist/mouse share.app"
```

也可以直接从源码运行而不打包：

```bash
cargo run --release --features ui --bin mouse-share-ui
```

首次启动 macOS 会弹窗索要「辅助功能」权限。在**系统设置 → 隐私与安全性 →
辅助功能**里勾选 `mouse share` 并重启应用即可。

### 命令行使用

**在控制端电脑（Server）上运行：**

```bash
# 默认端口 4242，客户端在右侧
mouse-share server

# 指定端口和客户端方位
mouse-share server --port 5000 --edge left
```

**在被控端电脑（Client）上运行：**

```bash
# 连接到 Server 的 IP 和端口
mouse-share client --server 192.168.1.100:4242
```

### 参数说明

```
Usage: mouse-share <COMMAND>

Commands:
  server  Run as server (controller side)
  client  Run as client (controlled side)

Server options:
  -p, --port <PORT>  监听端口 [默认: 4242]
  -e, --edge <EDGE>  客户端屏幕方位: left/right/top/bottom [默认: right]

Client options:
  -s, --server <ADDR>  Server 地址，如 192.168.1.100:4242
```

## 平台权限

### macOS

需要授予「辅助功能」权限：

**系统设置 → 隐私与安全性 → 辅助功能** → 添加终端应用（Terminal / iTerm2 / 你运行程序的终端）

### Windows

以**管理员身份运行**即可（低级鼠标钩子需要管理员权限）。

## 架构设计

### 项目结构

```
src/
├── main.rs                 # CLI 入口，server/client 分发
├── protocol.rs             # 网络协议定义（消息类型、序列化）
├── config.rs               # 屏幕配置、边缘检测
├── screen.rs               # 跨平台屏幕信息获取
├── net/
│   ├── server.rs           # Server 端：事件转发、边缘检测
│   └── client.rs           # Client 端：事件接收、模拟执行
└── input/
    ├── capture.rs           # InputCapture trait 定义
    ├── simulate.rs          # InputSimulator trait 定义
    ├── macos_capture.rs     # macOS: CGEventTap 捕获
    ├── macos_simulate.rs    # macOS: CGEvent 模拟
    ├── win_capture.rs       # Windows: SetWindowsHookEx 捕获
    └── win_simulate.rs      # Windows: SendInput 模拟
```

### 核心模块

#### 协议层 (`protocol.rs`)

使用 `serde` + `bincode` 二进制序列化，单个鼠标事件约 40 字节。

```rust
enum Message {
    Hello(ScreenInfo),          // Client → Server: 注册
    HelloAck(ScreenInfo),       // Server → Client: 确认
    Enter { x: f64, y: f64 },  // 鼠标进入 Client 屏幕
    Leave,                      // 鼠标离开 Client 屏幕
    Input(MouseEvent),          // 鼠标事件数据
    Heartbeat,                  // 心跳保活
}
```

#### 输入捕获 (`input/capture.rs`)

通过 trait 抽象跨平台差异：

| 平台 | 实现方式 | 事件抑制 |
|------|---------|---------|
| macOS | `CGEventTap` (HID level) | 回调返回 `None` |
| Windows | `SetWindowsHookEx(WH_MOUSE_LL)` | 返回 `LRESULT(1)` |

#### 输入模拟 (`input/simulate.rs`)

| 平台 | 实现方式 |
|------|---------|
| macOS | `CGEvent::new_mouse_event` + `CGEvent::post` |
| Windows | `SendInput` + `MOUSEINPUT` (绝对坐标 0-65535) |

### 网络设计

- **传输层**：UDP — 鼠标事件高频(60-120Hz)、每个事件独立、丢包可接受（下一帧覆盖）
- **连接管理**：Hello/HelloAck 握手，Client 自动重试 10 次
- **保活**：双向 Heartbeat（1秒间隔）
- **安全看门狗**：Server 端 5 秒无事件自动释放鼠标抑制，防止鼠标卡死

### 屏幕切换逻辑

1. Server 持续监测鼠标位置
2. 鼠标到达配置的边缘（如右边缘）→ 开启事件抑制 → 发送 `Enter` 到 Client
3. Server 跟踪虚拟光标在 Client 屏幕上的位置
4. 虚拟光标到达 Client 的返回边缘 → 关闭抑制 → 发送 `Leave`

## 跨平台编译

### macOS 上编译 Windows 版本

```bash
# 安装 Windows 交叉编译工具链
rustup target add x86_64-pc-windows-msvc
# 需要通过 cargo-xwin 或在 Windows 上直接编译
```

### Windows 上编译

```bash
cargo build --release
```

## 调试

启用详细日志：

```bash
RUST_LOG=debug mouse-share server
RUST_LOG=debug mouse-share client --server 192.168.1.100:4242
```

## 已知限制

- 当前仅支持单 Client 连接
- 不支持键盘共享（仅鼠标）
- 不支持剪贴板同步
- UDP 无加密，仅适用于可信局域网
- 不支持 Linux

## 技术依赖

| 依赖 | 用途 |
|------|------|
| `clap` | CLI 参数解析 |
| `serde` + `bincode` | 二进制序列化 |
| `crossbeam-channel` | 无锁线程间通信 |
| `core-graphics` | macOS 输入捕获/模拟 |
| `windows` | Windows Win32 API |
| `anyhow` | 错误处理 |
| `log` + `env_logger` | 日志 |

## License

MIT
