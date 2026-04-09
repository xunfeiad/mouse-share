# 剪贴板同步：UDP + TCP 混合架构

本项目在 UDP 鼠标转发之外，还通过 TCP 通道实现了双向剪贴板同步。本文档解释为什么要引入第二种传输协议、如何防止同步循环、以及关键的竞态条件分析。

## 一、为什么 UDP 不适合传输剪贴板？

鼠标事件和剪贴板内容是两种截然不同的数据：

| | 鼠标事件 | 剪贴板内容 |
|--|----------|------------|
| 数据大小 | ~40 字节 | 几字节 ~ 几十 MB |
| 频率 | 100+ Hz | 人工触发，很稀疏 |
| 丢包容忍 | 可丢（下帧纠正） | 不可丢 |
| 顺序要求 | 松散 | 严格 |
| 延迟敏感 | 极敏感 | 不敏感 |

UDP 的根本限制：

1. **单个数据报 ≤ 64 KB**（IPv4 UDP 最大载荷 65507 字节）。复制一张截图或一段长文本就会超过这个限制。
2. **没有分片重组**。IP 层会分片，但只要丢失任一分片，整个数据报就丢失。在 WiFi 上这个概率不可忽略。
3. **没有可靠交付**。剪贴板内容丢了就是丢了，用户会感到"粘贴不过去"。
4. **没有顺序保证**。快速连续复制两次，接收端可能先收到后一次、再收到前一次。

鼠标事件之所以能容忍这些问题，是因为每一帧都是一个相对 delta，丢一两帧不影响最终状态，而剪贴板是状态快照，不允许丢失或乱序。

**结论**：鼠标走 UDP，剪贴板走 TCP。两个通道独立、互不干扰。

## 二、混合架构

```
┌──────────────┐                              ┌──────────────┐
│   Server     │                              │    Client    │
│              │                              │              │
│  ┌────────┐  │  UDP :4242 (鼠标事件，低延迟) │  ┌────────┐  │
│  │  mouse │◀─┼──────────────────────────────┼─▶│ mouse  │  │
│  └────────┘  │                              │  └────────┘  │
│              │                              │              │
│  ┌────────┐  │  TCP :4243 (剪贴板，可靠)    │  ┌────────┐  │
│  │  clip  │◀─┼──────────────────────────────┼─▶│  clip  │  │
│  └────────┘  │                              │  └────────┘  │
└──────────────┘                              └──────────────┘
```

- 端口约定：剪贴板 TCP 端口 = 鼠标 UDP 端口 + 1
- Server 启动时额外 `spawn` 一个线程运行 `clipboard::run_server(port+1)`
- Client UDP 握手成功后 `spawn` 一个线程运行 `clipboard::run_client(addr+1)`
- 两套通道完全独立，互不阻塞

关键代码（[src/net/server.rs](../src/net/server.rs)）：

```rust
let clipboard_port = self.port + 1;
std::thread::Builder::new()
    .name("clipboard-server".into())
    .spawn(move || {
        crate::clipboard::run_server(clipboard_port);
    })?;
```

## 三、TCP 长度前缀帧协议

TCP 是字节流，没有消息边界。我们需要自己设计一个简单的帧协议：

```
┌──────────────┬───────────────────────────┐
│ 4-byte BE len│   bincode payload ...     │
└──────────────┴───────────────────────────┘
```

- 前 4 字节：大端序的 payload 长度（u32）
- 后面是 bincode 序列化的 `ClipboardMessage`
- 上限 64 MB，防止恶意或损坏的数据耗尽内存

实现（[src/clipboard.rs:52-71](../src/clipboard.rs#L52-L71)）：

```rust
fn write_framed(stream: &mut TcpStream, msg: &ClipboardMessage) -> Result<()> {
    let data = bincode::serialize(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}

fn read_framed(stream: &mut TcpStream) -> Result<ClipboardMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("clipboard message too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(bincode::deserialize(&buf)?)
}
```

`read_exact` 会阻塞直到拿满指定字节数，或对端关闭连接返回 `UnexpectedEof`。这正是我们想要的：一个完整的帧，要么到达、要么失败。

### 为什么用 u32 BE 而不是 varint？

- 简单：固定 4 字节，不需要处理变长编码
- 跨语言友好：网络字节序（大端）是事实标准
- 对齐：方便抓包分析

64 MB 用 u32 足够（理论最大 4 GB）。

## 四、arboard — 跨平台剪贴板访问

[`arboard`](https://crates.io/crates/arboard) 是一个跨平台剪贴板库，封装了：

- macOS：`NSPasteboard`
- Windows：`OpenClipboard` / `GetClipboardData`
- Linux：`x11` / `wayland`

只需要两个 API：

```rust
let mut clipboard = Clipboard::new()?;
let text = clipboard.get_text()?;     // 读取
clipboard.set_text(text.clone())?;    // 写入
```

本项目目前只同步 `Text` 类型（`ClipboardMessage::Text(String)`）。图片同步可以作为未来扩展：`arboard` 支持 `get_image`/`set_image`，但需要处理像素格式、压缩、以及 64 MB 上限。

### 为什么每个线程都要 `Clipboard::new()`？

`arboard::Clipboard` 内部持有平台句柄（NSPasteboard 实例 / HCLIPBOARDFORMAT），通常不是 `Send`。让发送线程和接收线程各自拥有一份是最简单的方案，避免了 `Arc<Mutex<Clipboard>>` 的开销和生命周期复杂度。

## 五、轮询式变化检测

剪贴板 API 没有"内容变化"事件（至少没有跨平台的）。我们用 **定时轮询 + 哈希比对** 来检测变化：

```rust
const POLL_INTERVAL: Duration = Duration::from_millis(500);

loop {
    std::thread::sleep(POLL_INTERVAL);
    let text = clipboard.get_text()?;
    let hash = hash_of(&text);
    if hash != last_hash {
        // 变化了，发送出去
        write_framed(&mut stream, &msg)?;
        last_hash = hash;
    }
}
```

500ms 是延迟 vs. CPU 的折中：
- 更短（比如 50ms）会让 CPU 负载明显上升，特别是剪贴板含大文本时
- 更长（比如 2s）用户会感到"复制后等了一下才能粘贴"
- 500ms 在用户感知上是"即时"的，CPU 也几乎无消耗

哈希使用 `DefaultHasher`（SipHash-1-3），对 64 MB 文本也只需几毫秒。

## 六、同步循环的防御

### 问题

两端都在同时"监听剪贴板 + 接收远端变化 + 写入本地剪贴板"。天然存在循环：

```
A 复制 "hello"
 → A 的 watcher 检测到变化，发给 B
 → B 的 recv 把 "hello" 写入自己的剪贴板
 → B 的 watcher 检测到"变化"（其实是刚写进去的），发回给 A  ← 死循环
```

### 哈希去重

共享一个 `last_hash: Mutex<Option<u64>>`，两个方向都会更新它：

```rust
// watcher：读到新内容 → 如果 hash 与 last_hash 相同，不发送
// receiver：收到远端内容 → 把 hash 存入 last_hash，再写入本地剪贴板
```

这样，B 的 recv 写入 `"hello"` 之前，就把 `"hello"` 的 hash 存进了 `last_hash`。之后 B 的 watcher 轮询时，算出相同的 hash，发现与 `last_hash` 一致，就不会再发回去。

## 七、竞态条件分析

简单的"哈希去重"乍看之下就够了，但仔细分析会发现一个凶险的竞态：

### 错误设计

```rust
// watcher（线程 A）
let text = clipboard.get_text()?;        // (1) 读剪贴板
let hash = hash_of(&text);
if hash != *last_hash.lock() {           // (2) 查 hash
    *last_hash.lock() = hash;            // (3) 更新 hash
    send(text);                          // (4) 发送
}
```

```rust
// receiver（线程 B）
let msg = recv();
let hash = hash_of(&msg);
*last_hash.lock() = hash;                // (a) 先更新 hash
clipboard.set_text(&msg)?;               // (b) 再写剪贴板
```

设想这个时序：
1. 用户复制 `"foo"`（hash = H1），watcher 线程执行 (1)，读到 `"foo"`
2. **同时**，receiver 线程收到远端的 `"bar"`（hash = H2），执行 (a) 把 `last_hash` 设为 H2，执行 (b) 把剪贴板写成 `"bar"`
3. watcher 线程继续执行 (2)：它手里的 hash 是 H1，`last_hash` 是 H2，不等，认为"发生变化"
4. watcher 执行 (3)(4)：把 `last_hash` 改成 H1，发出 `"foo"`
5. 但此时剪贴板里其实是 `"bar"`！watcher 发出的是**它以为还在剪贴板里的旧值**

结果：对端的 `"bar"` 被 watcher "覆盖"发回一个 `"foo"`，而本地剪贴板里是 `"bar"`。两端状态永远不一致。

换一种时序也会出问题：
1. receiver 执行 (a) 设 `last_hash = H2`
2. watcher 被调度，它上一轮读到的是 `"foo"`（H1），发现 `last_hash=H2 ≠ H1`，发送 `"foo"`
3. receiver 才执行 (b) 设剪贴板为 `"bar"`

本质：剪贴板的"内容"和 `last_hash` 是两个变量，必须保证它们的变更对另一个线程看起来是**原子**的。

### 正确设计：扩大临界区

**watcher**（[src/clipboard.rs:78-106](../src/clipboard.rs#L78-L106)）：

```rust
let to_send = {
    let mut guard = state.last_hash.lock().unwrap();
    match clipboard.get_text() {           // 在锁内读剪贴板
        Ok(text) if !text.is_empty() => {
            let msg = ClipboardMessage::Text(text);
            let hash = hash_of(&msg);
            if *guard != Some(hash) {      // 在锁内比对
                *guard = Some(hash);       // 在锁内更新
                Some(msg)
            } else {
                None
            }
        }
        _ => None,
    }
};  // 锁释放
if let Some(msg) = to_send {
    write_framed(&mut stream, &msg)?;     // 发送在锁外，避免阻塞 receiver
}
```

**receiver**（[src/clipboard.rs:109-134](../src/clipboard.rs#L109-L134)）：

```rust
let msg = read_framed(&mut stream)?;      // 读网络在锁外
let hash = hash_of(&msg);
{
    let mut guard = state.last_hash.lock().unwrap();
    *guard = Some(hash);                  // 锁内更新 hash
    clipboard.set_text(text.clone())?;    // 锁内写剪贴板
}
```

### 为什么这样就对了？

把每个线程的"读/写剪贴板 + 读/写 hash"放进同一个临界区，使得：

- receiver 更新 `last_hash` 和写入剪贴板是一个不可分割的整体
- watcher 读取剪贴板和读取 `last_hash` 也是一个不可分割的整体
- watcher 永远不会看到"新 hash 但旧剪贴板"或"旧 hash 但新剪贴板"的中间态

具体到之前的时序：watcher 在 (1) 读剪贴板时已经拿着锁，receiver 的 (a)(b) 只能排在 watcher 的整个块之前或之后。无论哪种排法，watcher 看到的剪贴板内容和 `last_hash` 永远是一致的。

### 性能代价

临界区变大会降低并行度，但代价可以接受：

- 轮询间隔是 500ms，临界区内的操作（读剪贴板 + 算 hash）通常 < 1ms
- 接收端写剪贴板也在毫秒级
- 两者竞争锁的概率极低
- 网络 I/O（`write_framed` / `read_framed`）在锁外执行，不会互相阻塞

## 八、连接管理与容错

### Server 端

```rust
pub fn run_server(port: u16) {
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))?;
    loop {
        let (stream, addr) = listener.accept()?;
        handle_connection(stream);   // 阻塞直到断连
        // 断连后回到 accept，等待下一个客户端
    }
}
```

Server 在一个时刻只服务一个剪贴板连接（和 UDP 鼠标通道的 1:1 关系一致）。断连后自动接受新连接。

### Client 端

```rust
pub fn run_client(addr: SocketAddr) {
    loop {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(stream) => {
                handle_connection(stream);   // 阻塞直到断连
            }
            Err(_) => { /* 连接失败 */ }
        }
        std::thread::sleep(RETRY_DELAY);    // 2s 后重试
    }
}
```

Client 永远会重连。典型场景：
- Server 先启动，Client 后启动 → Client 第一次连接成功
- Server 重启 → Client 检测到断连 → 2s 后重连 → 恢复
- 网络抖动 → Client 可能短暂断连 → 自动恢复

### 每个连接两个线程

`handle_connection` 为每个连接生成两个线程：

```rust
let send_handle = spawn(|| watch_and_send(send_stream, state));
let recv_handle = spawn(|| recv_and_apply(recv_stream, state));
send_handle.join();
recv_handle.join();
```

- `watch_and_send`：轮询本地剪贴板并发送
- `recv_and_apply`：从 TCP 读取并写入本地剪贴板
- 通过 `TcpStream::try_clone()` 让两个线程共享同一个底层 socket
- 任一方向出错，对应线程退出，`join` 返回，`handle_connection` 也返回，上层进入重连逻辑

TCP 是全双工的，两个方向独立收发，线程模型天然契合。

### 每个连接一个 `ClipboardState`

```rust
let state = ClipboardState::new();    // 新的 Mutex<Option<u64>>
```

每次建立新连接就创建一个新的 `last_hash`。这样重连后第一次复制仍然会发送（不会被上一次连接的残留 hash 卡住）。实现简单、行为正确。

## 九、局限与未来扩展

### 当前局限

1. **仅文本**：不支持图片、文件、富文本（RTF/HTML）。`arboard` 支持图片，但协议层只定义了 `Text` variant。
2. **500ms 轮询**：极快速的连续复制可能只同步到最后一次。对交互式使用几乎无影响。
3. **无加密**：内容在局域网上以 bincode 明文传输。WiFi 加密（WPA2/3）是唯一防线。对敏感数据可以在应用层加 TLS。
4. **64 MB 上限**：超大文件/图片无法同步。上限可以调整，但要权衡内存占用。

### 扩展方向

1. **图片同步**：增加 `ClipboardMessage::Image { width, height, data: Vec<u8> }` variant。格式用 PNG 压缩以节省带宽。
2. **差分同步**：对大文本用 `xxhash` 分块哈希，只传输变化的部分。收益有限（剪贴板通常整块替换），实现成本高。
3. **TLS 加密**：用 `rustls` 包装 `TcpStream`，PSK 模式避开证书管理。
4. **操作系统变化事件**：macOS 的 `NSPasteboard changeCount`、Windows 的 `AddClipboardFormatListener`、X11 的 `SelectionNotify` 都可以替代轮询。但会让 `clipboard.rs` 分裂成三个平台实现，复杂度骤增。目前 500ms 轮询的代价微不足道，不值得优化。

## 十、小结

| 关注点 | 做法 |
|--------|------|
| 为什么不用 UDP | 大小、可靠性、顺序 |
| 传输协议 | TCP + 4 字节长度前缀 + bincode |
| 跨平台 API | `arboard` |
| 变化检测 | 500ms 轮询 + 哈希比对 |
| 循环防御 | 共享 `last_hash` |
| 竞态防御 | 锁覆盖"剪贴板 + hash"整体 |
| 容错 | accept/connect 循环 + 2s 重试 |
| 集成方式 | Server/Client 各 spawn 一个后台线程 |

整个模块只有约 230 行代码，但每一行都有其存在的理由。剪贴板同步看似简单，真正要做对，关键在于理解**两个共享变量（系统剪贴板 + hash 缓存）必须同步变更**这一点。
