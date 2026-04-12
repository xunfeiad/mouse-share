# UI 日志系统实现指南

本文档描述 mouse-share 项目中 **UI 内嵌日志查看器** 的完整实现方案。读完后你可以在任何 Rust + egui 项目中复刻同样的功能。

---

## 架构总览

```
应用代码
    ↓
log::info!() / log::warn!() / log::error!()     ← 标准 log crate 宏
    ↓
TeeLogger (自定义 log::Log 实现)
   ├── env_logger  →  stderr (终端输出，保持不变)
   └── LogBuffer   →  内存环形缓冲区 (VecDeque, 1000 条，带序列号)
                          ↓
                      UI 每帧调用 drain_since(last_seq)
                      只拉取新增条目，追加到本地缓存
                          ↓
                      egui::ScrollArea::show_rows() 虚拟滚动
                      只渲染可见的 ~15 行
```

核心思路：**Tee（分流）+ 增量读取 + 虚拟滚动**——一条日志同时写到终端和内存缓冲区，UI 每帧只读取新增条目追加到本地 Vec（避免全量 clone），渲染时用 `show_rows()` 只布局可见行（避免 1000 行 widget 开销）。

---

## 第一步：定义日志数据结构

文件：`src/log_buffer.rs`

```rust
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

const CAPACITY: usize = 1000;  // 环形缓冲区容量

/// 一条日志记录
#[derive(Clone, Debug)]
pub struct LogLine {
    pub seq: u64,         // 单调递增序列号，UI 用它追踪已读位置
    pub ts_ms: u64,       // Unix 毫秒时间戳
    pub level: Level,     // Info / Warn / Error / Debug / Trace
    pub message: String,  // 格式化后的日志消息
}
```

**设计要点**：
- `seq` 是关键——每条日志有唯一递增编号，UI 只需记住"上次读到哪"就能增量拉取
- `ts_ms` 用 Unix 毫秒而非 `Instant`，因为 `Instant` 不能格式化为人类可读时间
- `Clone` 只在增量拉取时使用，每帧仅 clone 新增的 0~2 条

---

## 第二步：实现带序列号的环形缓冲区

```rust
/// 线程安全的环形日志缓冲区
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    buf: VecDeque<LogLine>,
    next_seq: u64,  // 下一个要分配的序列号，严格递增
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                buf: VecDeque::with_capacity(CAPACITY),
                next_seq: 1,
            })),
        }
    }

    /// 写入一条日志（由 TeeLogger 调用，任意线程）
    pub fn push(&self, ts_ms: u64, level: Level, message: String) {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.buf.len() == CAPACITY {
            inner.buf.pop_front();  // FIFO：满了就淘汰最老的
        }
        inner.buf.push_back(LogLine { seq, ts_ms, level, message });
    }

    /// 增量读取：只返回 seq > after_seq 的新条目
    ///
    /// 返回值包含：
    /// - lines: 新增的日志条目
    /// - oldest_seq: 缓冲区中最老条目的 seq
    /// - newest_seq: 缓冲区中最新条目的 seq
    ///
    /// 如果 after_seq < oldest_seq，说明调用方落后太多（中间的
    /// 条目已被环形淘汰），此时返回缓冲区的全部内容，调用方应
    /// 用这些数据替换（而非追加）本地缓存。
    pub fn drain_since(&self, after_seq: u64) -> DrainResult {
        let inner = self.inner.lock().unwrap();
        if inner.buf.is_empty() {
            return DrainResult {
                lines: Vec::new(),
                oldest_seq: 0,
                newest_seq: 0,
            };
        }
        let oldest_seq = inner.buf.front().unwrap().seq;
        let newest_seq = inner.buf.back().unwrap().seq;

        // 落后太多 → 返回全部，让调用方重建缓存
        let effective_after = if after_seq < oldest_seq {
            0
        } else {
            after_seq
        };

        let lines: Vec<LogLine> = inner.buf.iter()
            .filter(|l| l.seq > effective_after)
            .cloned()
            .collect();

        DrainResult { lines, oldest_seq, newest_seq }
    }
}

pub struct DrainResult {
    pub lines: Vec<LogLine>,
    pub oldest_seq: u64,
    pub newest_seq: u64,
}
```

**为什么用 `VecDeque` + 序列号而不是 channel？**
- UI 需要看到历史日志（滚动查看），不是消费一次就丢掉
- `VecDeque` 的 FIFO 淘汰天然实现了"只保留最近 N 条"
- 序列号让 UI 可以精确定位"从哪开始读"，无需全量 clone
- `drain_since()` 是非破坏性读取，缓冲区内容不受影响

**内存开销**：约 1000 × 2 KB ≈ 2 MB，对桌面应用可忽略。

---

## 第三步：实现 TeeLogger

```rust
/// 分流日志器：同时写 stderr + 内存缓冲区
struct TeeLogger {
    inner: env_logger::Logger,  // 负责终端输出
    buffer: LogBuffer,          // 负责 UI 展示
}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)  // 复用 env_logger 的过滤规则
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // 1. 正常写终端
        self.inner.log(record);
        // 2. 同时写到内存缓冲区（自动分配 seq）
        self.buffer.push(
            now_ms(),
            record.level(),
            record.args().to_string(),
        );
    }

    fn flush(&self) {
        self.inner.flush();
    }
}
```

**关键**：`record.args().to_string()` 在当前线程完成格式化，避免把 `Record`（含引用）跨线程传递。

---

## 第四步：全局单例安装

```rust
static GLOBAL: OnceLock<LogBuffer> = OnceLock::new();

/// 安装日志器。幂等——多次调用安全。返回共享的 LogBuffer。
pub fn install() -> LogBuffer {
    // 已安装则直接返回
    if let Some(existing) = GLOBAL.get() {
        return existing.clone();
    }

    let buffer = LogBuffer::new();
    let inner = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .format_timestamp_millis()
    .build();
    let filter = inner.filter();

    let tee = TeeLogger {
        inner,
        buffer: buffer.clone(),
    };

    if log::set_boxed_logger(Box::new(tee)).is_ok() {
        log::set_max_level(filter);
    } else {
        log::set_max_level(LevelFilter::Info);
    }

    let _ = GLOBAL.set(buffer.clone());
    buffer
}

/// 获取全局 LogBuffer（懒安装）
pub fn global() -> LogBuffer {
    GLOBAL.get().cloned().unwrap_or_else(|| install())
}
```

**使用方式**——在 `main()` 最顶部调用一次：

```rust
fn main() {
    let _ = log_buffer::install();
    // 之后所有 log::info!() 自动进入缓冲区
}
```

**日志级别控制**：通过 `RUST_LOG` 环境变量。例如 `RUST_LOG=debug cargo run` 开启 debug 级别。

---

## 第五步：UI 增量读取 + 渲染

文件：`src/bin/ui.rs`

### 5.1 App 结构体中添加日志缓存

```rust
struct App {
    // ... 其他字段 ...

    /// 本地日志缓存，增量更新，避免每帧全量 clone
    log_cache: Vec<log_buffer::LogLine>,
    /// 上次读到的最大序列号
    log_last_seq: u64,
}
```

### 5.2 日志面板主函数

```rust
fn log_tab(ui: &mut egui::Ui, app: &mut App) {
    // ── 增量读取：只 clone 新增条目 ──
    let result = log_buffer::global().drain_since(app.log_last_seq);
    if !result.lines.is_empty() {
        // 如果本地缓存落后太多（条目被环形淘汰），全量重建
        if app.log_last_seq < result.oldest_seq {
            app.log_cache = result.lines;
        } else {
            // 常规路径：追加新增的 0~几条
            app.log_cache.extend(result.lines);
        }
        // 本地缓存也限制容量，与环形缓冲区同步
        const LOG_CAPACITY: usize = 1000;
        if app.log_cache.len() > LOG_CAPACITY {
            app.log_cache.drain(..app.log_cache.len() - LOG_CAPACITY);
        }
        app.log_last_seq = result.newest_seq;
    }
    let lines = &app.log_cache;

    // ── 统计各级别数量 ──
    let (mut info_count, mut warn_count, mut err_count) = (0, 0, 0);
    for l in lines {
        match l.level {
            Level::Info | Level::Debug | Level::Trace => info_count += 1,
            Level::Warn => warn_count += 1,
            Level::Error => err_count += 1,
        }
    }

    // ── 顶部过滤标签 ──
    ui.horizontal(|ui| {
        log_chip(ui, &format!("All {}", lines.len()), /* ... */);
        log_chip(ui, &format!("Info {}", info_count), /* ... */);
        log_chip(ui, &format!("Warn {}", warn_count), /* ... */);
        log_chip(ui, &format!("Error {}", err_count), /* ... */);
    });

    // ── 虚���滚动日志区域 ──
    // 用 show_rows() 替代 show()：egui 只回调可见行范围
    // （260px / ~18px ≈ 15 行），其余行仅分配占位空间。
    // 行高用 text_style_height() 动态获取，适配不同 DPI。
    let row_height = ui.text_style_height(&egui::TextStyle::Body)
        .max(14.0);
    let total = lines.len();

    egui::ScrollArea::vertical()
        .max_height(260.0)
        .stick_to_bottom(true)   // ← 新日志到来时自动滚动
        .show_rows(ui, row_height, total, |ui, row_range| {
            for i in row_range {
                render_log_line(ui, &lines[i]);
            }
        });

    // ── 底部状态栏 ──
    ui.label(format!("{} events", total));
}
```

**性能对比**：

| 指标 | 旧方案 (snapshot + show) | 新方案 (drain_since + show_rows) |
|------|--------------------------|----------------------------------|
| 每帧 clone 数 | 1000 条 String | 0~2 条 String |
| 每帧 widget 数 | ~3000 (1000行 × 3个label) | ~45 (15 可见�� × 3) |
| 每帧堆分配 | ~1000 次 | ~0 次（无新日志时） |
| 锁持有时间 | ~1ms (memcpy 1000条) | ~1µs (scan + clone 几条) |

### 5.3 单行渲染

```rust
fn render_log_line(ui: &mut egui::Ui, line: &LogLine) {
    ui.horizontal(|ui| {
        // 时间戳 [HH:MM:SS]，等宽字体，弱色
        ui.label(
            RichText::new(format_ts(line.ts_ms))
                .size(11.0)
                .color(TEXT_MUTED)
                .family(FontFamily::Monospace),
        );

        // 级别标签，彩色背景小徽章
        let (label, fg, bg) = match line.level {
            Level::Info  => ("INFO",  GREEN, GREEN_SOFT),
            Level::Warn  => ("WARN",  YELLOW, YELLOW_SOFT),
            Level::Error => ("ERR",   RED, RED_SOFT),
            _ => ("DBG", GRAY, GRAY_SOFT),
        };
        Frame::none()
            .fill(bg)
            .rounding(Rounding::same(4.0))
            .inner_margin(Margin::symmetric(6.0, 1.0))
            .show(ui, |ui| {
                ui.label(RichText::new(label).size(10.0).color(fg).strong());
            });

        // 日志消息，等宽字体
        ui.label(
            RichText::new(&line.message)
                .size(12.0)
                .color(TEXT)
                .family(FontFamily::Monospace),
        );
    });
}

/// 时间戳格式化：Unix毫秒 → HH:MM:SS (UTC)
fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let tod = secs % 86400;
    format!("{:02}:{:02}:{:02}", tod / 3600, (tod % 3600) / 60, tod % 60)
}
```

---

## 依赖清单

```toml
[dependencies]
log = "0.4"
env_logger = "0.11"
eframe = { version = "0.29", features = ["default_fonts", "glow"] }
```

仅 3 个直接依赖。`log` + `env_logger` 是 Rust 生态事实标准，`eframe` 是 egui 的桌面框架。

---

## 文件结构

```
src/
├── log_buffer.rs          # LogLine + LogBuffer + TeeLogger + install()
│                          # drain_since() 增量读取接口
├── main.rs                # log_buffer::install() 调用点
├── lib.rs                 # pub mod log_buffer; 导出
└── bin/
    └── ui.rs              # App.log_cache / App.log_last_seq
                           # log_tab() + render_log_line() + log_entry()
```

---

## 复刻清单

在你自己的项目中实现同样功能的步骤：

1. **新建 `src/log_buffer.rs`**——复制 `LogLine`（含 seq）、`LogBuffer`（含 Inner + next_seq）、`TeeLogger`、`install()`、`global()`、`drain_since()`
2. **在 `src/lib.rs` 中导出**——`pub mod log_buffer;`
3. **在 `main()` 顶部安装**——`log_buffer::install();`
4. **App 结构体加两个字段**——`log_cache: Vec<LogLine>` + `log_last_seq: u64`
5. **UI 每帧调用 `drain_since(last_seq)`**——新增条目追加到 `log_cache`，落后时全量重建
6. **用 `ScrollArea::show_rows()` 虚拟滚动**——只渲染可见行，行高用 `ui.text_style_height()` 动态获取
7. **`stick_to_bottom(true)`** 实现新日志自动滚到最新
8. **按 level 着色**——Info 绿色、Warn 黄色、Error 红色的彩色标签

无需修改任何已有的 `log::info!()` 调用——安装 TeeLogger 后所有日志自动进入缓冲区。

---

## 注意事项

| 问题 | 解决方案 |
|------|----------|
| 锁竞争 | `drain_since()` 拿锁时间 ≈ scan + clone 几条新条目 < 10µs。push() 拿锁 < 1µs。无瓶颈 |
| 内存泄漏 | 环形缓冲区 + 本地缓存均限 1000 条，上限 ~2MB |
| 多线程安全 | Arc<Mutex> 包裹，任意线程可 push |
| 本地缓存落后 | `after_seq < oldest_seq` 时自动全量重建，不会丢数据 |
| 热路径日志 | 高频路径（如 1kHz 鼠标事件）应降级为 `log::debug!()` 或限频，避免缓冲区被刷屏 |
| 日志级别动态切换 | 当前通过 `RUST_LOG` 环境变量控制，运行时不可变。如需动态切换，可在 `TeeLogger::enabled()` 中检查一个 `AtomicUsize` 级别标志 |
