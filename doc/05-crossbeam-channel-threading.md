# 跨线程通信：crossbeam-channel 与原子操作

本项目使用多线程架构：输入捕获线程 + 网络事件循环主线程。线程间通信依赖 `crossbeam-channel` 和 `AtomicBool`。

## 一、为什么需要多线程

macOS 和 Windows 的输入捕获都需要运行一个阻塞的事件循环：

- **macOS**：`CFRunLoop::run_current()` 永不返回
- **Windows**：`GetMessageW()` 循环永不返回

如果在主线程运行捕获，网络事件循环就无法执行。因此：

```
┌──────────────────┐        crossbeam-channel        ┌──────────────────┐
│  Capture Thread   │ ──── MouseEvent ──────────────▶ │  Main Thread      │
│  (CFRunLoop /     │                                 │  (UDP send/recv   │
│   GetMessageW)    │ ◀──── AtomicBool ──────────────│   event loop)     │
│                   │       (suppress flag)            │                   │
└──────────────────┘                                  └──────────────────┘
```

## 二、crossbeam-channel

### 为什么不用 std::sync::mpsc？

| 特性 | `std::sync::mpsc` | `crossbeam-channel` |
|------|-------------------|---------------------|
| 性能 | 一般 | 极高（lock-free 算法） |
| 有界通道 | 不支持（仅 `sync_channel`） | `bounded(n)` |
| `recv_timeout` | 支持 | 支持 |
| `try_send` | `try_send`（sync_channel） | `try_send` |
| Select | 不支持 | `select!` 宏 |

### 创建有界通道

```rust
use crossbeam_channel::{bounded, Sender, Receiver};

// 容量 256 的有界通道
let (sender, receiver) = bounded::<MouseEvent>(256);
```

**为什么有界？** 如果网络层处理不过来（比如 UDP 缓冲区满了），事件会积压。无界通道会无限增长内存。有界通道在满时 `try_send` 会丢弃最新事件，保护内存。

### 发送端（Capture 线程）

```rust
// src/input/macos_capture.rs
// 回调中使用 try_send，不阻塞捕获线程
let _ = sender.try_send(mouse_event);
```

**`try_send` vs `send`**：

- `send(&item)`：阻塞直到有空间。如果在事件捕获回调中阻塞，macOS 会禁用 event tap。
- `try_send(item)`：立即返回。通道满时返回 `Err(TrySendError::Full)`，丢弃事件。

在 120Hz 鼠标事件 + 256 容量的配置下，通道有约 2 秒的缓冲。正常情况下主线程每 1ms 取一次，不会满。

### 接收端（主线程）

```rust
// src/net/server.rs
match receiver.recv_timeout(Duration::from_millis(1)) {
    Ok(event) => { /* 处理事件 */ }
    Err(RecvTimeoutError::Timeout) => { /* 无事件，继续循环 */ }
    Err(RecvTimeoutError::Disconnected) => { /* 发送端已关闭 */ }
}
```

`recv_timeout(1ms)` 是事件循环的节拍器。1ms 超时既保证低延迟，又不会空转浪费 CPU。

## 三、AtomicBool — 抑制标志

### 需求

主线程需要告诉 Capture 线程"开始/停止抑制事件"。这是一个单 bit 的跨线程信号。

### 实现

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// 创建共享标志
let suppressing = Arc::new(AtomicBool::new(false));

// Capture 线程持有一份引用
let suppress_clone = suppressing.clone();

// 主线程设置
suppressing.store(true, Ordering::SeqCst);   // 开始抑制
suppressing.store(false, Ordering::SeqCst);  // 停止抑制

// Capture 线程读取
if suppress_clone.load(Ordering::Relaxed) {
    // 吞掉事件
}
```

### 内存排序 (Memory Ordering)

| Ordering | 含义 | 用途 |
|----------|------|------|
| `Relaxed` | 仅保证原子性，不保证其他操作的可见顺序 | 独立标志位 |
| `Acquire` | 读操作后的所有读写不会被重排到该读之前 | 锁的获取 |
| `Release` | 写操作前的所有读写不会被重排到该写之后 | 锁的释放 |
| `SeqCst` | 全局顺序一致性 | 最安全但最慢 |

本项目中：
- **写端（主线程）** 使用 `SeqCst`：确保抑制状态变更对所有线程立即可见
- **读端（捕获回调）** 使用 `Relaxed`：回调频率极高(120Hz)，Relaxed 最快；即使读到稍微旧的值，最多延迟一帧(8ms)，用户无感知

## 四、Arc — 原子引用计数

`Arc<AtomicBool>` 拆解：

```
Arc          → 线程安全的引用计数智能指针（Atomic Reference Counting）
AtomicBool   → 原子布尔值

Arc<AtomicBool> = 多个线程安全地共享同一个原子布尔值
```

```rust
let flag = Arc::new(AtomicBool::new(false));

// clone 只增加引用计数，不复制数据
let flag2 = flag.clone();  // flag 和 flag2 指向同一块内存

// 两个线程看到的是同一个值
std::thread::spawn(move || {
    flag2.store(true, Ordering::SeqCst);
});
// 主线程稍后能看到 true
```

### 为什么不用 Mutex<bool>?

| | `Arc<AtomicBool>` | `Arc<Mutex<bool>>` |
|--|-------------------|---------------------|
| 开销 | 一条 CPU 指令 | 系统调用（futex） |
| 阻塞 | 永不阻塞 | 可能阻塞 |
| 适用 | 单个原子值 | 复杂数据结构 |

对于单个 bool 标志，`AtomicBool` 是最优解。

## 五、线程创建

```rust
// src/net/server.rs

let _capture_thread = std::thread::Builder::new()
    .name("input-capture".into())   // 线程名（调试时可见）
    .spawn(move || {                 // move 闭包，转移所有权
        if let Err(e) = capturer.run(sender) {
            log::error!("Capture error: {}", e);
        }
    })?;
```

`move` 关键字将 `capturer` 和 `sender` 的所有权转移到新线程。主线程通过 `receiver` 和 `suppress` 与捕获线程通信。

### 线程生命周期

项目中捕获线程在程序退出前一直运行。`_capture_thread` 的 `JoinHandle` 被丢弃时，线程变为 detached（不会被 join），但仍然继续运行。程序退出时所有线程被强制终止。

## 六、Cell — 单线程内部可变性

macOS 捕获回调中使用 `std::cell::Cell`：

```rust
// src/input/macos_capture.rs
let last_x = std::cell::Cell::new(init_x);
let last_y = std::cell::Cell::new(init_y);

// 闭包内修改
let prev_x = last_x.get();
last_x.set(pos.x);
```

`Cell` 提供内部可变性（不需要 `mut` 引用就能修改值），但 **不是线程安全的**（`!Sync`）。这里可以用是因为 macOS event tap 回调始终在同一个 CFRunLoop 线程上执行。

Windows 端使用 `thread_local!` + `Cell`/`RefCell` 达到同样效果。
