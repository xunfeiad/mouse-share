# UDP 网络编程基础

本项目使用 UDP 而非 TCP 传输鼠标事件。本文档解释 UDP 的基础知识以及项目中的具体用法。

## 一、TCP vs UDP

| 特性 | TCP | UDP |
|------|-----|-----|
| 连接 | 面向连接（三次握手） | 无连接 |
| 可靠性 | 保证送达、有序 | 不保证送达、无序 |
| 延迟 | 较高（重传、Nagle 算法） | 极低（无额外开销） |
| 头部 | 20 字节 | 8 字节 |
| 适用场景 | 文件传输、HTTP | 实时游戏、视频流、鼠标事件 |

## 二、为什么选择 UDP

鼠标事件有三个特点，决定了 UDP 是更优选择：

### 1. 实时性要求极高

人对鼠标延迟的感知阈值约 10ms。TCP 的 Nagle 算法会将小包合并（默认等待 40ms），即使关闭 Nagle（`TCP_NODELAY`），TCP 的重传机制仍可能引入 10-200ms 的延迟波动。

### 2. 每个事件是独立的

鼠标每帧产生一个位移 delta `(dx, dy)`，每个 delta 独立有效。丢失第 N 帧的 delta 后，第 N+1 帧仍然正确——只是光标位置差一点，用户几乎无法察觉。

### 3. "最新状态优先"

TCP 保证有序，但对鼠标事件来说，如果第 5 帧因重传延迟了 100ms，而第 6、7、8 帧已经到达，TCP 会让应用等第 5 帧收到后才能处理 6、7、8。这叫 **head-of-line blocking**，UDP 没有这个问题。

## 三、Rust 中的 UDP Socket

### 核心 API：`std::net::UdpSocket`

```rust
use std::net::UdpSocket;

// 绑定到本地地址
let socket = UdpSocket::bind("0.0.0.0:4242")?;

// 发送数据到指定地址
socket.send_to(&data, "192.168.1.100:4242")?;

// 接收数据（返回数据长度和来源地址）
let mut buf = [0u8; 4096];
let (len, src_addr) = socket.recv_from(&mut buf)?;
```

### 阻塞 vs 非阻塞

```rust
// 阻塞模式（默认）：recv_from 会一直等待直到有数据
socket.set_nonblocking(false)?;

// 非阻塞模式：recv_from 立即返回，无数据时返回 WouldBlock 错误
socket.set_nonblocking(true)?;

// 超时模式：阻塞等待，超时后返回 TimedOut 错误
socket.set_read_timeout(Some(Duration::from_millis(100)))?;
```

### 本项目中的用法

**Server 端**（`src/net/server.rs`）：

```rust
// 1. 等待 Client 阶段：使用超时模式（100ms 超时）
let socket = UdpSocket::bind("0.0.0.0:4242")?;
socket.set_read_timeout(Some(Duration::from_millis(100)))?;

// 2. 事件循环阶段：切换为非阻塞
socket.set_nonblocking(true)?;
```

切换为非阻塞是关键优化。如果事件循环中 socket 阻塞 100ms，鼠标事件就会积压在 channel 中，用户感受到明显卡顿。

**Client 端**（`src/net/client.rs`）：

```rust
// 绑定到随机端口（0 表示让系统分配）
let socket = UdpSocket::bind("0.0.0.0:0")?;
socket.set_read_timeout(Some(Duration::from_millis(50)))?;
```

## 四、UDP 数据报特性

### 消息边界保持

TCP 是字节流，发两次 `write(10 bytes)` 接收端可能一次 `read` 收到 20 bytes。

UDP 是数据报，每次 `send_to` 发送一个完整的数据报，每次 `recv_from` 恰好收到一个完整的数据报。这意味着：

- **无需自定义帧分隔**：不需要长度前缀或分隔符
- **bincode 反序列化可以直接对 `&buf[..len]` 操作**

### MTU 限制

以太网 MTU 通常为 1500 字节，减去 IP 头(20) + UDP 头(8) = **最大有效载荷 1472 字节**。

本项目的 `MouseEvent` 经 bincode 序列化后约 40 字节，远小于 MTU，不存在分片问题。

## 五、项目协议设计

### 握手流程

```
Client                          Server
  │                                │
  │──── Hello(ScreenInfo) ────────▶│  Client 发送屏幕信息
  │                                │
  │◀─── HelloAck(ScreenInfo) ─────│  Server 回复屏幕信息
  │                                │
  │◀─── Heartbeat ────────────────│  每 1 秒双向心跳
  │──── Heartbeat ────────────────▶│
  │                                │
  │◀─── Enter { x, y } ──────────│  鼠标进入 Client 屏幕
  │◀─── Input(MouseEvent) ───────│  鼠标事件流
  │◀─── Input(MouseEvent) ───────│
  │◀─── Leave ────────────────────│  鼠标离开 Client 屏幕
```

### 丢包应对

| 消息类型 | 丢包影响 | 缓解策略 |
|----------|---------|---------|
| Hello | 无法建立连接 | Client 重试 10 次，每次等待 2 秒 |
| Input(Move) | 光标微偏 | 下一帧自动纠正 |
| Input(ButtonDown) | 按键丢失 | 后续可加状态同步 |
| Enter/Leave | 切换状态不同步 | 看门狗 5 秒超时自动释放 |
| Heartbeat | 无直接影响 | 每秒发送，容忍连续丢失 |

## 六、socket2 crate

`Cargo.toml` 中引入了 `socket2`，它提供了比 `std::net` 更细粒度的 socket 控制：

```rust
use socket2::{Socket, Domain, Type, Protocol};

// 创建 socket 并设置缓冲区大小
let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
socket.set_send_buffer_size(65536)?;  // 64KB 发送缓冲区
socket.set_recv_buffer_size(65536)?;  // 64KB 接收缓冲区
```

当前项目尚未使用 `socket2` 的高级功能，预留用于后续优化（如调整缓冲区大小、设置 `SO_REUSEADDR` 等）。
