# 死锁与抖动修复 (Round 4)

Round 3 引入 `CGAssociateMouseAndMouseCursorPosition(false)` 来解决
"server 光标跟随物理鼠标" 的问题，副作用是把 "任一小 bug" 升级成了
"全系统死锁"。这一轮撤回这个副作用，同时把 client 端的抖动一并解决。

---

## 问题 1: 死锁 (Bug B)

**症状**: Server 和 client 建立连接后，鼠标进入 client 一段时间后，
整个系统进入卡死状态 —
- Server 的 UI 主窗口点不到 (包括 Stop 按钮)
- Client 也关不掉
- 鼠标完全不显示
- 只能用 `Cmd+Option+Esc` 强退两端进程才能恢复

**根因**: `CGAssociateMouseAndMouseCursorPosition(false)` 是一个**全局**
状态 — 调用后 macOS 的 window server 停止根据 HID 事件更新任何光标
位置。这是进程**不是**作用域的：即使我们的进程崩溃，这个状态也会
持续到下次显式调用 `true` 或用户注销。

一旦 forwarding 状态机因为任何原因卡住 (tap 被禁用未及时恢复、事件
通道空了 watchdog 还没触发、`Enter`/`Leave` 消息丢包),光标就永久
冻结在 server 一侧。而 egui 的 UI 主线程依赖能点击到按钮才能触发
"停止服务",于是用户无法自救。

**修复**: [capture.rs:96-135](../src/input/capture.rs#L96-L135)
`hide_local_cursor` 不再调用 `CGAssociate(false)`,只保留 `hide_cursor()`
的视觉隐藏。抑制工作完全由 HID 级别的 CGEventTap 承担 —
如果 tap 一切正常,视觉和抑制都到位;如果 tap 挂了,最坏结果是 **视觉
隐藏但鼠标还能物理移动**,用户可以点到 Stop 按钮自救,**永远不会
死锁**。

**自救兜底**: [capture.rs:155-169](../src/input/capture.rs#L155-L169)
新增 `restore_cursor_state_on_startup()`,在 main 启动时幂等调用
`CGAssociate(true)` + `show_cursor()`。用户如果之前用 Round 3 的
版本崩溃过并留下了冻结状态,升级到这版后下次启动就会自动解冻,
不用注销。

**权衡**: 如果 tap suppress 完全失效 (比如权限被临时撤销),server 上
的鼠标会在用户看不见的情况下继续在屏幕上移动 — 视觉难受但可操作。
对比原方案 "死锁整台机器",这是可接受的降级。

---

## 问题 2: 点击穿透诊断 (Bug A)

**症状**: Mouse 已在 client 侧后,物理点击鼠标,server 和 client 都
触发点击。

**可能根因**:
- (a) Tap callback 返回 `None` 但 macOS 忽略了 (tap 被悄悄降级为
  passive ListenOnly,例如权限问题或 callback 太慢)
- (b) `suppressing` flag 在那一瞬间是 false (watchdog 误触发、
  刚从 client 返回未及时 store(true)、race condition)
- (c) Tap 被 macOS 自动禁用 (`TapDisabledByTimeout` /
  `TapDisabledByUserInput`) 且我们还没来得及 re-enable

这一轮不直接修 bug A,而是加了两件事辅助下次复现时定位:

**诊断日志**:
[macos_capture.rs:117-140](../src/input/macos_capture.rs#L117-L140)
Tap callback 里对每个 button/scroll 事件都会记一条
`tap suppressing {event_type}` 或 `tap passing through {event_type}`,
这样日志能直接回答 "点击漏下来时,到底是 tap callback 没被调用 /
被调用但没 suppress / suppress 了但 macOS 没理我们"。

**更快的 tap 恢复**:
[macos_capture.rs:152-162](../src/input/macos_capture.rs#L152-L162)
把外层 runloop 的 poll 间隔从 100 ms 降到 16 ms。这是在 macOS 自动
禁用 tap (`TapDisabledByUserInput`) 后到我们重新 enable 的最大延迟。
从 100 ms 降到 16 ms 意味着最坏情况下只有一个显示帧的事件能漏过。

**更严格的内存顺序**:
[macos_capture.rs:117](../src/input/macos_capture.rs#L117)
`suppressing.load(Relaxed)` → `SeqCst`。多一次内存屏障换一个可读的
因果链 — 成本在人类鼠标尺度上是零。

---

## 问题 3: 抖动 (Bug C)

**症状**: 用户说 "server 进入 client 侧,没之前那么卡(延迟),但总会
时不时的抖动一会"。延迟是 Round 3 解决的,抖动是 Round 3 引入的。

### 根因: 固定 8 ms sleep 的量子化

Round 3 的 client 代码在每次 `simulator.move_relative` 之后硬 sleep
了 8 ms,用来把 warp 速率砍到 125 Hz — 防止 macOS window server
的光标管线被 1 kHz 级别的 warp 打爆。

这解决了延迟,但产生了新的问题:

```
drain packets → flush warp → sleep 8 ms → drain packets → flush warp → sleep 8 ms ...
```

每个 sleep **无条件** 等 8 ms。所以:
- **慢速移动**: 用户手慢慢滑,server 每包只含几个像素的 dx/dy。
  Client 本来每包都能立即响应,但被硬 sleep 憋成 8 ms 一次 —
  看起来是 "一步一步阶梯状" 前进,不是平滑滑动。
- **drain 慢**: 如果一次 drain + flush 本身就花了 6 ms
  (syscall / 日志偶发),仍然多 sleep 8 ms → 累计 14 ms 一次 warp,
  降到 ~71 Hz,主观上更抖。
- **drain 快**: 如果 drain + flush 只花了 0.1 ms,sleep 固定 8 ms →
  刚好 125 Hz,这种情况是唯一工作符合预期的。

### 修复 1: 基于 Instant 的速率限制 (不再固定 sleep)

[client.rs:284-300](../src/net/client.rs#L284-L300) 把 "flush 后 sleep
8 ms" 改成 "距上次 warp 至少 `min_warp_interval`":

```rust
let since = last_warp.elapsed();
if since < min_warp_interval {
    std::thread::sleep(min_warp_interval - since);
}
// ... flush ...
last_warp = Instant::now();
```

效果:
- drain 慢 (已经花了 6 ms) → 只补 2 ms → 维持精确节奏
- drain 快 (花了 0.1 ms) → 补 7.9 ms → 维持精确节奏
- 零额外延迟,精确封顶 `1/refresh_hz`

### 修复 2: 一劳永逸 — 和**显示器刷新率**对齐,不再硬编码 125 Hz

用户很敏锐地问: **"如果我的鼠标不是 125 Hz 呢?"**

关键点:**warp 速率和鼠标的 polling rate 完全无关**。

- 鼠标 polling rate 决定 server 每秒捕获多少个 `MouseEvent`。
  1 kHz 的鼠标每秒产生 1000 个事件,125 Hz 的办公鼠标每秒 125 个。
  这些都会经 UDP 送到 client。**没有任何事件被丢弃**。
- Client 每次 drain 周期会把到达的所有事件 **合并成一次 warp**。
  所以 warp 频率 ≠ 鼠标频率。
- Warp 频率 受限于 **显示器的刷新管线** — 在 60 Hz 显示器上发 200 Hz
  的 warp 毫无意义,多出来的会被合并到同一帧,视觉上看不到差别,
  只会堵塞 window server。反过来,在 240 Hz 显示器上发 125 Hz 的
  warp 就白白浪费了一半的运动分辨率。

正确的速率是 **显示器刷新率本身**。新增
[screen.rs:27-71](../src/screen.rs#L27-L71) 的
`get_display_refresh_hz()` 查询主显示器实际的刷新率 (macOS:
`CGDisplayModeGetRefreshRate`;Windows: `GetDeviceCaps(VREFRESH)`),
client 启动时拿到这个值作为 `min_warp_interval` 的基准:

[client.rs:87-115](../src/net/client.rs#L87-L115)
```rust
let refresh_hz = get_display_refresh_hz();
let min_warp_interval = Duration::from_secs_f64(1.0 / refresh_hz);
```

于是:

| 显示器 | refresh_hz | min_warp_interval | 效果 |
|---|---|---|---|
| 办公显示器 | 60 Hz | 16.67 ms | 每帧一次 warp,不 over-feed |
| MacBook ProMotion | 120 Hz | 8.33 ms | 每帧一次 warp |
| 144 Hz 游戏屏 | 144 Hz | 6.94 ms | 每帧一次 warp |
| 240 Hz 电竞屏 | 240 Hz | 4.17 ms | 每帧一次 warp |

对于笔记本的变刷新率显示器 (ProMotion / 内建面板报 0 Hz),默认
fallback 到 120 Hz。

### 为什么这真的是 "永久方案"

因为它和用户的硬件自动对齐。用户不需要调参,不需要知道 warp / poll /
refresh 三个概念的区别;不管换什么鼠标 (125 / 500 / 1000 / 8000 Hz)
或什么显示器 (60 / 120 / 144 / 240 Hz),client 都会在 "显示器刷新率
这一帧一次 warp" 的位置工作,这是物理上最优的点。

---

## 紧急恢复 (留档)

即使不死锁了,进程还是可能被强杀。如果某次用户发现鼠标冻结,恢复
流程:

1. `Cmd+Option+Esc` → 强制退出 → 找到所有 `mouse-share*` 进程终结
2. 如果 `Cmd+Option+Esc` 也不响应,开 Terminal:
   `pkill -9 mouse-share-ui` / `pkill -9 mouse-share`
3. 如果鼠标还冻着 (旧版本残留 `CGAssociate(false)` 状态):
   **启动一次新版 mouse-share** — `restore_cursor_state_on_startup()`
   会在 main 开始时调用 `CGAssociate(true)`,自动解冻。
4. 如果不能运行新版,最后兜底:注销用户重新登录 (`Cmd+Shift+Q`)。

---

## 变更文件清单

- [src/input/capture.rs](../src/input/capture.rs) —
  `hide_local_cursor` 去掉 `CGAssociate(false)`,新增
  `restore_cursor_state_on_startup`
- [src/input/macos_capture.rs](../src/input/macos_capture.rs) —
  discrete-event 诊断日志,`Relaxed` → `SeqCst` 抑制标志读取,
  外层 runloop poll `100ms` → `16ms`
- [src/screen.rs](../src/screen.rs) — 新增 `get_display_refresh_hz()`
- [src/net/client.rs](../src/net/client.rs) — 固定 8 ms sleep 替换为
  基于 `Instant` + 显示器刷新率的速率限制
- [src/main.rs](../src/main.rs) / [src/bin/ui.rs](../src/bin/ui.rs) —
  启动时调用 `restore_cursor_state_on_startup`
