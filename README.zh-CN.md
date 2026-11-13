# mouse-share

[English](README.md)

通过 WiFi 局域网在多台电脑之间共享鼠标控制。支持 macOS 和 Windows 跨平台使用。

## 工作原理

```text
Server (controller) ── UDP P ────────▶ Client (target): absolute cursor
                    ◀─ TCP P ───────▶ handshake / heartbeat / reliable input
                    ◀─ TCP P+1 ─────▶ session-bound clipboard
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

## 设置、托盘与登录启动

设置自动保存到版本化 `config.json`（macOS：`~/Library/Application Support/com.xunfei.mouse-share/`；Windows：`%APPDATA%\MouseShare\`）。写入失败回退并显示错误；损坏文件保留 `config.corrupt-*.json` 备份。`MOUSE_SHARE_CONFIG_DIR` 可指定隔离的测试配置目录。

- 键盘、剪贴板、断线自动重连、托盘默认开启；调试日志和开机启动默认关闭。
- 控制端连接页的键盘、剪贴板开关会实时同步；关闭键盘先释放远端按键，关闭剪贴板取消双向传输。被控端的 CLI 关闭选项也参与协商。
- 端口和屏幕方向保存后用于下次启动，当前会话不会暗中重启；屏幕方向由控制端决定。
- 语言即时切换；调试日志动态生效，不记录剪贴板正文。
- 开启菜单栏／托盘：关窗隐藏并继续共享，菜单提供打开、启动、停止、退出；关闭托盘会先恢复窗口，此后关窗退出。
- 开机启动须主动开启：macOS 使用当前用户 LaunchAgent，Windows 使用 HKCU Run。`--autostart` 恢复保存的成功运行角色和连接配置，普通启动只显示窗口。
- GUI 单实例，重复启动唤起原窗口。停止和退出在后台释放输入及回收线程，UI 保持响应。

CLI 另支持 `--no-keyboard`、`--no-clipboard`、`--no-reconnect`、`--debug`。详细设计和验证边界见 [第一轮优化记录](doc/16-session-reliability.md)。

## 平台权限

### macOS

需要授予「辅助功能」权限：

**系统设置 → 隐私与安全性 → 辅助功能** → 添加终端应用（Terminal / iTerm2 / 你运行程序的终端）

### Windows

输入注入受目标应用完整性级别限制；普通权限进程无法控制更高权限的应用，失败会明确记录。见 [Microsoft SendInput 文档](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)。

## 架构设计

### 项目结构

```
src/
├── main.rs                 # CLI 入口，server/client 分发
├── protocol.rs             # 网络协议定义（消息类型、序列化）
├── config.rs               # 屏幕配置、边缘检测
├── screen.rs               # 跨平台屏幕信息获取
├── net/
│   ├── session.rs          # GUI/CLI 共用配置、命令与句柄
│   ├── runtime.rs          # 会话、转发、重连、输入清理
│   ├── transport.rs        # 可取消的 TCP 帧传输
│   └── state.rs            # 统一状态快照
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

协议 v2 使用 `serde` / `bincode`。TCP 控制帧为 4 字节大端长度加消息体；UDP 仅承载带会话标识、控制权代次、移动序号和控制序号的绝对位置。两端必须同步升级，旧协议仅用于识别并提示升级，不参与输入转发。

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

- 基础端口 `P` 范围为 `1…65534`，默认 `4242`。服务端需放行 **UDP P、TCP P 和 TCP P+1**。
- TCP 控制连接启用 `TCP_NODELAY`；点击、滚轮、键盘和设置可靠有序传输。点击前定位，旧 UDP 移动不会覆盖点击位置。
- 心跳间隔 1 秒，单调时钟计算 5 秒对端失联超时；TCP 关闭立即处理。静止不失联，持续 UDP 输入不能掩盖控制连接失联。
- 自动重连按 1、2、4、8 秒退避并加入小幅抖动；新连接完整替换旧会话和实际 UDP 地址。主动停止、退出、取消或关闭重连后不再重试。
- 离开、失联、停止和会话替换统一释放本会话注入的按钮、键及修饰键，并恢复本地输入与可见光标。

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
mouse-share --debug server
mouse-share --debug client --server 192.168.1.100:4242
```

## 已知限制

- 当前仅支持单 Client 连接
- 键盘转发仅支持相同操作系统，跨系统禁用并显示原因
- 本轮不支持多显示器和按住按钮跨屏拖拽
- 网络连接没有加密或身份认证；会话标识用于隔离旧消息，仅适用于可信局域网
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
