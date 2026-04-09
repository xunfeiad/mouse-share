# Serde 与 Bincode 序列化

本项目使用 `serde` + `bincode` 将鼠标事件序列化为紧凑的二进制格式，通过 UDP 传输。

## 一、Serde 框架

[serde](https://serde.rs/) 是 Rust 生态中最核心的序列化框架，它将序列化分为两层：

```
数据结构 ──(Serialize trait)──▶ 通用数据模型 ──(Serializer)──▶ 目标格式
数据结构 ◀──(Deserialize trait)── 通用数据模型 ◀──(Deserializer)── 目标格式
```

### derive 宏

通过 `#[derive(Serialize, Deserialize)]` 自动生成序列化代码，无需手写：

```rust
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

#[derive(Serialize, Deserialize)]
pub struct MouseEvent {
    pub dx: f64,
    pub dy: f64,
    pub event_type: MouseEventType,
    pub timestamp_us: u64,
}
```

Serde 支持大量目标格式：JSON、TOML、YAML、MessagePack、bincode 等。更换格式只需更换 Serializer，数据结构代码不变。

## 二、Bincode 格式

[bincode](https://github.com/bincode-org/bincode) 是一种紧凑的二进制序列化格式，专为 Rust 设计。

### 为什么不用 JSON？

| | JSON | bincode |
|--|------|---------|
| 格式 | 文本（UTF-8） | 二进制 |
| 可读性 | 人类可读 | 不可读 |
| 大小 | 较大（字段名、引号） | 极小（无字段名） |
| 速度 | 需要解析文本 | 直接内存映射 |
| 适用 | API、配置文件 | 进程间通信、网络协议 |

一个 `MouseEvent` 的大小对比：
- JSON: `{"dx":1.5,"dy":-0.3,"event_type":"Move","timestamp_us":1234567890}` ≈ 70 字节
- bincode: 约 **25-30 字节**（直接编码数值，enum 用 u32 索引）

鼠标事件频率 60-120Hz，bincode 的紧凑性和零开销解析直接降低了网络延迟。

### 编码规则

bincode 的编码方式非常直观：

```
f64      → 8 字节小端序
u64      → 8 字节小端序
u32      → 4 字节小端序
u8       → 1 字节
enum     → 4 字节变体索引 + 变体数据
struct   → 各字段依次拼接（无字段名）
String   → 8 字节长度(u64) + UTF-8 内容
```

### 项目中的用法

```rust
// src/protocol.rs

// 序列化：Message → Vec<u8>
pub fn serialize(msg: &Message) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::serialize(msg)?)
}

// 反序列化：&[u8] → Message
pub fn deserialize(data: &[u8]) -> anyhow::Result<Message> {
    Ok(bincode::deserialize(data)?)
}
```

调用点：
- Server 端序列化后通过 `socket.send_to()` 发送
- Client 端 `socket.recv_from()` 后反序列化

## 三、Message 枚举的序列化示例

```rust
pub enum Message {
    Hello(ScreenInfo),           // 变体 0
    HelloAck(ScreenInfo),        // 变体 1
    Enter { x: f64, y: f64 },   // 变体 2
    Leave,                       // 变体 3
    Input(MouseEvent),           // 变体 4
    Heartbeat,                   // 变体 5
}
```

序列化 `Message::Heartbeat`：
```
[05, 00, 00, 00]  // enum 变体索引 5，4 字节
```

序列化 `Message::Enter { x: 100.0, y: 200.0 }`：
```
[02, 00, 00, 00]                          // 变体索引 2
[00, 00, 00, 00, 00, 00, 59, 40]          // x = 100.0 (f64 小端序)
[00, 00, 00, 00, 00, 00, 69, 40]          // y = 200.0 (f64 小端序)
```

总计 20 字节，完全在单个 UDP 数据报内。

## 四、错误处理

bincode 反序列化可能失败的场景：

1. **数据截断**：UDP 数据报不完整（极少发生，UDP 要么完整收到，要么丢弃）
2. **版本不匹配**：Server 和 Client 的 `Message` 枚举不一致
3. **恶意数据**：非法的 enum 变体索引

项目中对反序列化错误统一处理为 `continue`（跳过无法解析的包）：

```rust
// src/net/client.rs
let msg = match protocol::deserialize(&buf[..len]) {
    Ok(m) => m,
    Err(_) => continue,  // 跳过无法解析的数据报
};
```

## 五、性能特点

bincode 几乎是零拷贝的：
- 序列化时直接将内存中的值按小端序写入 `Vec<u8>`
- 反序列化时直接从 `&[u8]` 读取并构建结构体
- 无需分配临时字符串、无需解析 JSON 语法树

在 120Hz 鼠标事件频率下，序列化+反序列化的总开销通常 < 1μs（微秒）。
