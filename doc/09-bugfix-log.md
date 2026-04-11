# Bug 修复记录

本文档记录项目上线调试过程中遇到的真实问题和修复方案。按"症状 → 根因 → 修复"的结构组织，方便后续排查类似问题。

## Bug #1：Server 启动后 capture 线程立刻报错退出

### 症状

```
[INFO  mouse_share::net::server] Client connected: 192.168.71.2:56313 (ScreenInfo { width: 1680, height: 1050 })
[INFO  mouse_share::net::server] Server screen: 2056x1329
[ERROR mouse_share::net::server] Capture channel disconnected
[ERROR mouse_share::net::server] Capture error: Failed to create event tap.
  Please grant Accessibility permission in System Preferences > Privacy & Security > Accessibility
```

客户端 UDP 握手正常完成，但 capture 线程一启动就失败，主线程看到 channel 断开后整个 server 退出。

### 根因

macOS 对全局输入监听有严格的 TCC（Transparency Consent Control）权限体系。`CGEventTap::new()` 失败的常见原因有两层：

**层 1 — 辅助功能（Accessibility）未授权**

我们用的 `CGEventTapOptions::Default`（主动 tap，能修改/抑制事件）必须有辅助功能权限。`ListenOnly`（被动 tap，只读）则只需要输入监控权限。

**层 2 — 输入监控（Input Monitoring）未授权**

从 macOS Catalina (10.15) 开始，`CGEventTapLocation::HID` 这种最底层的 tap 额外需要"输入监控"权限。macOS 15 (Sequoia) 的权限模型更严格，两个权限必须同时开。

**层 3 — TCC 身份识别失效**

即使用户在"辅助功能"列表里看到 `mouse-share` 开关是 ON 的，实际权限仍可能失效，原因是：

- **rebuild 让二进制身份变了**。TCC 基于代码签名（有签名）或 inode+路径+哈希（无签名）识别程序。Rust debug build 没有稳定签名，每次 `cargo build` 相当于新程序。
- **路径不一致**。列表里的条目可能指向另一个旧路径（比如 `target/debug/` vs 自定义 `CARGO_TARGET_DIR`）。

### 修复

代码上没有 bug，是运行时权限问题。固化修复流程：

1. **授权"辅助功能"和"输入监控"两处**
   - 系统设置 → 隐私与安全性 → 辅助功能 → 添加二进制
   - 系统设置 → 隐私与安全性 → 输入监控 → 添加二进制

2. **对 debug 二进制做 ad-hoc 代码签名**

   ```bash
   codesign --force --sign - /path/to/mouse-share
   ```

   即便是 ad-hoc 签名（identity 为 `-`），也给了二进制一个稳定的 `Identifier`，TCC 能正确跟踪身份。每次 `cargo build` 后都要重新签名一次。

3. **rebuild 后权限失效的应急方案**

   如果列表里看起来已授权但实际失败：
   - 先从列表删除 `mouse-share`（点 `-`）
   - 重启终端（Cmd+Q 彻底退出，不只是关窗口）
   - 重新添加当前二进制路径

4. **核选项**

   ```bash
   tccutil reset Accessibility
   ```

   清空所有辅助功能授权，重新来一遍。代价是其他 app 的授权也一起没了。

### 可选的代码改进

当前 capture 线程启动失败时，错误通过 "Capture channel disconnected" 的间接方式暴露给主线程，报错信息容易被淹没。可以改为主线程同步调用 `tap::new()` 做一次预检，失败时直接 panic 并打印完整权限授权步骤，让用户一眼看到问题。这是可读性改进，不影响功能。

---

## Bug #2：鼠标进入 Client 后光标消失

### 症状

Server 端 capture 权限搞定之后，鼠标确实能从 Server 屏幕的边缘"移过去"到 Client 了 —— log 显示 `Mouse entered client screen at (1679, 720)` 正常触发。但 **Client 端的可见光标消失了**，用户看不到鼠标在哪里。应用层（Hover/Drag 状态）表现也很奇怪。

### 根因

这是 macOS 上模拟鼠标的经典坑。我们原来的实现只做了一件事：

```rust
let event = CGEvent::new_mouse_event(source, MouseMoved, point, ...)?;
event.post(HID);
```

**`CGEventPost` 的语义是"通知系统和应用发生了一次鼠标移动"，但它并不保证真正把可见光标图形挪到新位置**。具体行为取决于系统状态：

1. **Client 机上没有活跃的 HID 输入**。Client Mac 的用户没有在操作自己的触控板/鼠标，macOS 会在一段时间后主动**隐藏光标**。我们 post 的事件虽然让应用知道光标在移动，但光标图形本身依然是隐藏的。
2. **Cursor 与 mouse 的关联（association）可能被打断**。如果之前有任何程序调用过 `CGAssociateMouseAndMouseCursorPosition(false)`，posted events 就不会驱动可见光标移动。
3. **Posted events 的"视觉效果"在不同 macOS 版本上不一致**。即使关联正常、光标未隐藏，有时候 post 一个 MouseMoved 也只是发出事件而不实际移动光标图形（取决于 tap location、event source、窗口层级等因素）。

换句话说，`event.post()` 能让你点击、拖拽、触发 hover，但**它不是"把光标挪到那里"的可靠手段**。真正挪动可见光标的 API 是 `CGWarpMouseCursorPosition` 或 `CGDisplayMoveCursorToPoint`。

### 修复

每次模拟鼠标移动都做四件事，次序很重要：

```rust
fn warp_and_show(point: CGPoint) {
    let _ = CGDisplay::warp_mouse_cursor_position(point);
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    let _ = CGDisplay::main().show_cursor();
}

fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
    self.current_x = x;
    self.current_y = y;
    let point = self.current_point();
    warp_and_show(point);
    self.post_mouse_event(CGEventType::MouseMoved, point, CGMouseButton::Left)
}
```

每一步对应一个独立的问题：

| 调用 | 作用 |
|------|------|
| `CGWarpMouseCursorPosition(point)` | **真正把可见光标图形挪到 `point`**。这是解决"看不到光标"的核心。 |
| `CGAssociateMouseAndMouseCursorPosition(true)` | 重新建立"光标 ↔ 鼠标事件"的关联。如果之前关联被打断，后续 posted events 才会驱动光标移动。 |
| `CGDisplay::main().show_cursor()` | 强制让光标可见。如果系统因"无本地输入"把光标隐藏了，这一步把它调回来。`show_cursor` 和 `hide_cursor` 用引用计数管理，重复调用是安全的。 |
| `event.post(MouseMoved)` | 通知应用层发生了一次鼠标移动。让 Hover、Drag 等交互状态正常工作。光标挪位已经被 warp 完成，这一步补上应用可见的事件。 |

为什么不能只 warp 不 post？

- warp 只是把光标图形挪过去，应用层**不会收到 `mouseMoved` 事件**
- Hover 高亮、Drag 选区、某些游戏的视角控制都依赖事件而不是光标位置
- 只 warp 的结果就是"光标能动但点不了、拖不了"

为什么不能只 post 不 warp？

- 就是原 bug 的情形：事件发出去了但可见光标没动
- 有时候 post 连 association 都未必能触发光标更新

两者是正交的：**warp 管视觉，post 管事件**。必须同时做。

### 延伸：Enter/move_relative 都要修

原本 `move_to` 和 `move_relative` 都只 post 事件。修复需要两个函数都调 `warp_and_show`。Enter 消息触发 `move_to`，后续所有移动触发 `move_relative`，任意一个只 post 不 warp 都会让光标"消失"。

### 为什么 Windows 不需要同样的修复

Windows 的 `SendInput` 带 `MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE` 标志时，**既更新系统光标位置，又投递事件给应用**。Windows 没有 macOS 那种 "光标可见性" 和 "事件投递" 分离的设计，所以一个 API 调用搞定一切。这也解释了为什么同样的 trait 抽象在两个平台上的实现复杂度差异这么大。

---

## Bug #3：进入 Client 后立刻返回，状态机抽搐

### 症状

Server log 中每毫秒都在重复：

```
Mouse entered client screen at (1679, 533)
Mouse returned to server screen
Mouse entered client screen at (1679, 532)
Mouse returned to server screen
Mouse entered client screen at (1679, 528)
Mouse returned to server screen
```

Enter 和 Return 几乎在同一毫秒成对出现，每秒数十次。用户感知是"鼠标根本推不过去"。

### 根因

这是一个非常隐蔽的状态机 bug：**入口位置和返回阈值是同一个坐标**。

以 `Edge::Left`（client 在 server 左侧）为例：

```rust
// 进入时放到 client 屏幕的右边缘
Edge::Left => (cw - 1.0, y / sh * ch),

// 返回条件：cursor 到达 client 右边缘
Edge::Left => client_cursor_x >= cw - 1.0,
```

进入的那一个事件循环迭代内执行流程是：

1. `at_edge(cx=0)` 判定为 true → 设置 `forwarding = true`
2. `client_cursor_x = cw - 1.0 = 1679`
3. 同一次迭代继续走 `if forwarding { ... }` 分支
4. 加上这次事件的 `event.dx`（可能是 0 或微小正值）
5. 检查 `client_cursor_x >= cw - 1.0` → **立刻为真**
6. 触发 Return，forwarding 重置回 false
7. 下一个事件又触发 at_edge，重复 1-6

本质错误：**一个点同时既是"入口"又是"返回触发线"，状态机没有任何缓冲区**。

### 修复：两层防护

**第一层：进入的同一个事件不再参与 delta 累加**

```rust
if config.at_edge(cx, cy) {
    // ... 设置 forwarding = true、发送 Enter ...
    continue;  // 这一帧就到此为止，下一个事件才进入 forwarding 分支
}
```

**第二层：return_armed 状态机**

返回检测只有在虚拟光标已经离开入口边缘**至少 20 像素**后才开放：

```rust
const RETURN_ARM_DISTANCE: f64 = 20.0;

// 进入时
return_armed = false;

// 每个 forwarding 事件更新
if !return_armed {
    let moved_inside = match config.edge {
        Edge::Left => entry_x - client_cursor_x,
        Edge::Right => client_cursor_x - entry_x,
        Edge::Top => entry_y - client_cursor_y,
        Edge::Bottom => client_cursor_y - entry_y,
    };
    if moved_inside >= RETURN_ARM_DISTANCE {
        return_armed = true;
    }
}

let should_return = return_armed && match config.edge {
    // 原有的边界检测
};
```

语义：**"用户必须先走进 client 屏幕，才能再走回来。"** 这符合物理直觉 —— 你不能一进门就立刻返回，必须先在屋里待过。

为什么是 20 像素而不是 1 像素？因为 HID delta 可能一次 10-15 像素，1-2 像素的阈值很容易被一次事件跨过。20 像素对应肉眼可见的轻微移动，既防抖动又不影响手感。

### 这类 bug 的普遍规律

**入口点和退出点重合**是状态机设计的常见陷阱。例如：

- 电梯门关上瞬间又打开（关门传感器和开门按钮触发同一条件）
- 程序进入循环的条件和退出循环的条件相同（死循环或瞬退）
- HTTP 重定向到自己

修复方式几乎永远是**引入一个"已经走远了"的状态**：`entered_at`、`armed`、`has_moved`、`grace_period`。单纯"比较当前值和阈值"是不够的，你需要比较**相对初始状态的变化量**。

---

## Bug #4：抑制模式下 dx 永远是 0 或噪声

### 症状

配合 Bug #3 看：在 Enter/Return 抽搐期间，每次虚拟光标增量几乎都是小数值（甚至正值，尽管用户在往左推）。用 `event.dx` 作为相对位移的逻辑实际上拿到的是噪声。

### 根因

原来 macOS capture 的 dx 是从 `event.location()` 做差分算出来的：

```rust
let pos = event.location();
let dx = pos.x - prev_x;
let dy = pos.y - prev_y;
last_x.set(pos.x);
last_y.set(pos.y);
```

这段代码在**正常模式下没问题**，但在**抑制模式**下会失效。为什么？

当 CGEventTap 的 callback 返回 `None`（抑制事件）时，**OS 不会应用那次事件的移动**。cursor 被"冻结"在边缘。下一次 HID 事件到来时：

- HID 硬件报告一个新的 delta（比如 `-5, 0`，用户继续往左推）
- OS 把这个 delta 加到当前冻结的 cursor 位置（0, y）上
- 因为 x 已经是 0，OS 把新位置 clamp 回 (0, y)
- 我们的 tap callback 收到的 `event.location()` 是 (0, y)，**和上一次完全一样**
- `pos.x - prev_x = 0 - 0 = 0`

结果：抑制模式下，无论用户怎么推，我们算出的 dx 都是 0。虚拟光标在 client 上一动不动。

更糟糕的是，cursor 在 clamp 过程中偶尔会有 +1 或 -1 的抖动（系统 UI 布局、Dock 边界、HID 采样噪声），这些抖动就成了我们能观测到的全部"移动"。正值抖动 + `client_cursor_x = 1679` 起点 → 立刻触发 Return。

### 修复：直接读 HID delta 字段

`CGEvent` 自带两个字段，保存的是 HID 硬件报告的原始相对位移，不受 cursor clamp 影响：

```rust
let dx = event
    .get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
let dy = event
    .get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;
```

这两个字段返回的是**自上一次事件以来鼠标实际移动了多少**，哪怕 OS 把 cursor clamp 到了边缘、或者某个应用禁用了光标移动，HID 层的真实位移都不会丢失。

这也是"相对 delta 模型"原本应有的正确实现 —— 不依赖 cursor 位置的差分，而是读硬件本身的报告值。

### 为什么最初没这么写？

最初的实现用 position 差分是因为：
1. 接口直观：`pos = event.location()` 比查 `EventField` 常量更容易想到
2. 正常模式下两者等价：cursor 跟随 HID 移动，差分 = HID delta
3. 没想到抑制模式下会出现"OS 冻结 cursor 但 HID 仍在更新"的状态分裂

这是个典型的"验证代码用正常路径，但生产代码走的是异常路径"的教训。写 input capture 的代码时，**要专门针对抑制模式测试 dx 的正确性**，不能假设和正常模式行为一致。

### 和 Windows 侧的对比

Windows 的 `WH_MOUSE_LL` hook 里，`MSLLHOOKSTRUCT::pt` 也是 cursor 的绝对位置，同样有抑制模式下的坐标冻结问题。Windows 的正确做法是计算前后两次 `pt` 的差，但需要注意 `LLMHF_INJECTED` 标志位来过滤自己注入的事件。macOS 改用 HID delta 字段更干净 —— 根本不涉及 cursor 位置。

---

## Bug #5：Client 端光标延迟严重，但 CPU 占用极低

### 症状

功能都正常跑通之后，接入一个 1000 Hz 轮询率的游戏鼠标，Server 控制 Client 时出现持续性的可见延迟：

- 在 Server 端快速甩动鼠标，Client 端的光标**肉眼可见地滞后**几十到几百毫秒追上来
- 动作越快延迟越明显，慢速移动反而看起来正常
- Server 端的本地光标响应完全正常，说明 capture 侧没问题
- **Client 进程的 CPU 占用几乎是平的**（个位数百分比），观感不像是 CPU 跑满
- `top` / Activity Monitor 看不到任何热点

用户描述非常精准："CPU 占用很低，感觉像是帧数低"。

### 根因：window server 的 cursor 管线被打爆

一个关键认知：**光标的可见移动不是由我们的调用直接完成的，而是由 macOS 的 window server 在它自己的线程/管线里处理**。我们调的 `CGWarpMouseCursorPosition` 和 `CGEvent::post` 都是**异步 IPC**，它们把请求扔进 window server 的队列就立刻返回 —— 所以我们的 CPU profile 看起来是空的。

但 window server 那边**不是**以任意频率消化这个队列的。它内部的 cursor 更新节奏是挂在 display refresh rate 上的（60/120 Hz），超过这个速率的请求会**排队**，而不是合并或丢弃。

原来的 `macos_simulate::move_relative` 长这样：

```rust
fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
    self.current_x += dx;
    self.current_y += dy;
    let point = self.current_point();
    warp_cursor(point);                                          // IPC #1
    self.post_mouse_event(CGEventType::MouseMoved, point, ...)   // IPC #2
    Ok(())
}
```

**每个 move 两次 IPC**。1000 Hz 输入 → 2000 个异步请求/秒扔给 window server，但 window server 只能以 ~120 Hz 消化 —— 积压比例约 16:1，也就是每秒积累 250~500 ms 的 backlog。这正是"CPU 平但动作落后几百毫秒"的症状形态。

为什么 CPU 测不到：我们的线程在 `warp_cursor` / `post_mouse_event` 返回之后立刻去读下一个 UDP 包，完全不等 window server。问题不出在我们的 CPU time，出在 window server 的 wall time。**profiler 看不到 window server 的 IPC 排队延迟**，但用户眼睛能看到。

次要原因：Client 原来每收到一个 UDP 包就调一次 `move_relative`，即便前一轮已经做了 move 合并,因为 LAN 上 UDP 包往往是**一个一个到的**，drain 循环每轮实际只合并了 1 个包，所以 client 这一侧的 coalesce 几乎没起作用 —— warp 速率还是紧贴包到达速率。

### 修复：两层

**层 1 —— 每个 move 只做 1 次 IPC，不做 2 次**

`CGWarpMouseCursorPosition` 和 `CGEvent::post(MouseMoved)` 功能正交但**目标重合**：两者都是为了"让 Client 端表现出光标在动"。砍掉其中一个：

- 保留 `warp_cursor`：它是权威的可见移动操作。window server 自己的 cursor tracking（菜单 hover、`NSTrackingArea`、hit testing、Dock magnification 等）监听的就是这个，不需要我们单独 post 事件通知。
- 砍掉 `post_mouse_event(MouseMoved)`：唯一会因为它被砍而收不到事件的，是**自己装了 CGEventTap 监听 MouseMoved 的程序**（输入录制器、某些辅助功能工具、一小部分游戏）。这些场景下光标照样会动，只是 tap 里看不到合成事件。对一个屏幕共享工具来说完全可接受。

这个修复把单次 move 的 IPC 数减半，但更重要的是**砍掉了 post 这一路**。post 比 warp 更容易排队：warp 是一次性的位置覆盖（new warp 覆盖旧 warp），多个 warp 在 window server 里理论上可以合并；而 post 进 HID 事件管线之后必须按顺序消化，它才是真正在积压的一路。

**注：这推翻了之前在 Bug #2/Bug #5 小结里留的"post 和 warp 两者正交，两者都要做"的说法。**那个结论在"低速、偶尔调用"的场景下是对的 —— 两者确实职能不同。但在高频 (500~1000 Hz) 调用下，同时调两个就是把两条 IPC 管线同时打爆，成本远大于收益。取舍换了。

**层 2 —— 把 warp 速率钉到 ~125 Hz (8 ms)**

即便只保留 warp，在原 drain 结构下，每来一个 UDP 包仍然会触发一次 warp —— 也就是 ~1000 warp/秒，照样超出 window server 的消化速率。

在 client 的 `event_loop` 里，每次 flush 完 pending 的 move 之后显式 sleep 8 ms：

```rust
if have_move {
    simulator.move_relative(pending_dx, pending_dy);
    // ...
    std::thread::sleep(Duration::from_millis(8));
}
```

sleep 期间发生了什么：

1. **本线程空闲**，不再吃 CPU（其实本来也没吃多少）。
2. **Kernel UDP buffer 继续积事件**。前面调到 1 MiB 的 `SO_RCVBUF` 就是为这个留的余量 —— 8 ms 内 1 kHz 的包是 8 个，完全放得下。
3. **下一轮 drain 把这 8 个包一次性取出**，由于之前已经有 move coalescing，所有的 dx/dy 会合并成一个总 delta，然后只做一次 warp。

最终效果：warp 调用频率被压到 ~125 Hz，和 display refresh 对齐，不再进 window server 的 backlog；但**没有丢任何事件**，因为所有事件都在 kernel buffer 里缓冲过，最后被合并发出了。

为什么 click/scroll/key 不受 8 ms sleep 影响：drain 循环里，**非 move 事件是 inline 处理的** —— 遇到 button/scroll/key 时先 flush 掉 pending 的 move，然后立刻 send 该事件，走完这一条才会继续往下。到 sleep 那里时，这些非 move 事件早就已经处理完了。sleep 只延迟下一批 move 的 flush，不延迟 click。

### 为什么 profiler 没抓到

这是这个 bug 最值得记下来的一点。**常规的 CPU profiler 无法观测到 IPC-backlog 类的延迟**：

- `perf` / Instruments 只统计我们进程消耗的 CPU time
- 我们的调用是 async 的，`warp_cursor` 几微秒就返回了
- 排队发生在 window server 内部，属于另一个进程的状态
- 用户看到的延迟 = (当前时刻在队列里的请求数) / (消化速率)，这和我们的 CPU 图完全脱钩

这种延迟只有**用眼睛看 + 用耳朵听用户描述**才能发现。"CPU 占用很低，感觉像是帧数" 就是典型的特征描述 —— 记住这句话，以后遇到类似症状的时候能省下几个小时。

### 诊断思路总结

下次遇到"动作有延迟但 CPU 没跑满"的情形，按这个顺序排查：

1. **确认方向**。是 Client 端还是 Server 端延迟？（Server 端一般是 capture 或 channel 卡住；Client 端一般是 simulation 或 render。）
2. **在 hot path 上数 IPC 次数**。对 macOS 来说就是 window server 相关的 API：warp、post、event tap、CG\* 系列。
3. **把 IPC 次数和预期消化速率比较**。display refresh rate (~60~120 Hz) 是硬上限，超过它就一定会排队。
4. **如果必须以高频调用，考虑 rate limit 或 batching**。客户端 sleep、server 端按 display rate 发送、合并成更大的单次请求 —— 三选一或组合。

### 为什么最初没这么写？

原作者（也是同一个作者）写 simulator 的时候：

1. 功能上先要把"光标能动"做出来，这时候加 `post_mouse_event` 是为了兼容性 —— 担心 "warp 不通知 app" 会引发奇怪问题，所以两条都开。
2. 测试时用慢速、单次、手动的移动，根本触发不了 backlog —— 那种频率下 IPC 随便调。
3. 上 1000 Hz 游戏鼠标 + 大幅度快速移动时才会进入 backlog 区间，而且因为延迟是"滞后"而不是"掉帧"，肉眼看不出是频率问题，会怀疑 CPU。

**教训**：输入模拟类代码，测试时必须拿高轮询率的硬件（500 Hz / 1000 Hz 鼠标）真实使用，桌面级缓慢测试会放掉高频场景的一整类 bug。

---

## 排查方法论：从症状到根因

两个 bug 看起来都像"鼠标移不过去"，但根因完全不同：

| Bug | 现象 | 诊断关键 |
|-----|------|----------|
| #1 | Server 进程直接退出 | 看 log 里的 Capture error |
| #2 | Server 正常运行，Client 端光标不见 | 从 Server log 看 `Mouse entered...` 是否打印 |

复用诊断路径：**先看 Server 有没有打印 Enter 消息**。

- **没打印** → 边缘检测或事件捕获的问题（权限、坐标系、Dock 遮挡、显示器拓扑）
- **打印了但 Client 没反应** → 网络问题（UDP 丢包、防火墙）或 Client 模拟的问题（本 bug）
- **打印了 Client 也有日志但看不到光标** → Client 的视觉渲染问题（本 bug）

对应的调试工具是我们在 Server 的 `event_loop` 里加的诊断日志：

```rust
log::info!(
    "cursor=({:.0},{:.0}) dx={:.1} dy={:.1} at_edge={}",
    cx, cy, event.dx, event.dy, config.at_edge(cx, cy)
);
```

每秒节流一次，能直接看到：
- 光标坐标是否在动
- `at_edge` 的判断结果
- dx/dy 的方向是否合理

这种"在每个关键决策点输出一行结构化日志"的做法，比加断点单步调试高效得多 —— 特别是在输入捕获这种**必须真实交互才能触发**的代码路径上，断点本身就会打断交互流。

---

## 未来可以加的预检

一些运行前的健全性检查可以提前暴露类似问题：

1. **启动时测试 CGEventTap 创建**。如果失败，立即退出并给出权限授权步骤指引。不要让用户看到一堆 UDP 握手日志之后才发现权限没过。

2. **启动时测试 CGWarpMouseCursorPosition 可用**。这个 API 在某些沙箱/虚拟化环境下可能被禁用。失败时降级为 "只 post 不 warp" 并警告用户光标可能不可见。

3. **Client 端日志**。目前 Client 只在收到 Enter/Leave 时打 log，每次 `move_relative` 静默执行。可以加一个节流日志 `Simulated move to (x, y)`，便于对照 Server 的发送日志排查丢包或坐标偏移。

这些都是"小工具"类改进，不影响正常使用，但能在出问题时把排查时间从小时级降到分钟级。

---

## 小结

| 关键点 | 教训 |
|--------|------|
| macOS 权限双重性 | 辅助功能 ≠ 输入监控。两者独立，都可能卡住 `CGEventTap`。 |
| TCC 身份识别 | 未签名 debug 二进制的 TCC 身份不稳定，ad-hoc `codesign` 是轻量解法。 |
| `post` vs `warp`（低频） | 低频调用下两者正交：`CGEventPost` 通知事件、`CGWarpMouseCursorPosition` 移动光标。 |
| `post` vs `warp`（高频） | 1 kHz 场景下两者之和会打爆 window server 的 IPC 管线（见 Bug #5）。只保留 `warp`，并把调用速率钉到 display refresh (~125 Hz)。 |
| 隐形副作用 | macOS 会主动隐藏无活跃输入的光标。模拟输入的程序必须显式 `show_cursor`。 |
| HID delta 字段 | 抑制模式下 `event.location()` 会冻结；读 `MOUSE_EVENT_DELTA_X/Y` 才是 HID 真实位移。 |
| profiler 盲区 | CPU profiler 看不到跨进程 IPC backlog。"CPU 很低但感觉像帧数低"就是典型特征。 |
| 诊断日志胜过断点 | 输入捕获类代码的调试，加节流 log 比断点快十倍。 |
| 高频硬件必测 | 输入类代码必须拿 500~1000 Hz 真鼠标测过，慢速手动测会放掉整类高频 bug。 |
