# 光标入口坐标修复 (Round 5)

Round 4 修复了死锁和抖动，但用户反馈：**鼠标从 server 中间滑入 client 时，
光标总是出现在 client 屏幕的左上角，而不是对应的 Y 位置。**

---

## 问题描述

Server 在 ARM MacBook (2056×1329)，Client 在 Intel Mac (1680×1050)。
用户从 server 屏幕中间向右滑到 client，期望光标从 client 左边缘 Y≈70%
的位置出现，实际却出现在左上角 (0,0) 附近。

## 诊断过程

### 1. 排除坐标空间不匹配

首先怀疑 macOS Retina 坐标系问题 —— `CGDisplayPixelsWide()` 返回像素宽度，
而 `CGEvent::location()` 返回 points。在 Retina 显示器上两者可能不同。

通过 Swift 脚本验证：

```
pixels_wide = 2056   (display mode 逻辑分辨率)
bounds.width = 2056  (global display coordinate space, points)
pixelWidth = 4112    (物理像素, 2x)
```

当前机器上 `pixels_wide == bounds.width`，但为安全起见仍切换到
`CGDisplayBounds()` —— 这是 Apple 文档中明确标注 "returns in the global
display coordinate space" 的 API，和 `CGEvent::location()` 保证同一坐标系。

### 2. 增加入口诊断日志

在 server 的 Enter 消息发送处加了完整诊断：

```
Mouse entered client: cursor=(2055,936) → entry=(0,740)
  server=2056x1329 client=1680x1050 edge=Right
```

**坐标映射完全正确**：`936/1329 × 1050 ≈ 740`。问题不在 server 端。

### 3. 定位到 client 端的 show_local_cursor

根因在 `show_local_cursor()` 内部的 **restore warp**。

`hide_local_cursor()` 的设计是在隐藏光标前保存当前位置，然后
`show_local_cursor()` 在恢复时 warp 回保存的位置。这个机制是给
**server** 用的 —— 鼠标回到 server 后恢复原来的光标位置，避免跳到 (1,1)。

但在 **client** 端，调用链是：

```
Enter(0, 740) 到达
  → show_local_cursor()
    → show cursor on all displays
    → warp 回启动时 hide 保存的位置 (≈ 左上角)  ← 覆盖了入口坐标!
  → simulator.move_to(0, 740)  ← 被上面的 warp 覆盖了
```

注意 `show_local_cursor()` 的 restore warp 在 `move_to()` **同一帧内**
执行，最终光标位置取决于哪个 warp 最后生效。由于 `show_local_cursor` 的
restore warp 和 `move_to` 之间可能存在 window server pipeline 的
异步行为，restore warp 可以覆盖 move_to 的结果。

## 修复

### `show_local_cursor_no_restore()`

[capture.rs](../src/input/capture.rs) 将 `show_local_cursor` 拆分为：

```rust
pub fn show_local_cursor() {
    show_local_cursor_inner(true);   // 带 restore —— server 用
}

pub fn show_local_cursor_no_restore() {
    show_local_cursor_inner(false);  // 不 restore —— client 用
}

fn show_local_cursor_inner(restore_position: bool) {
    // ... show cursor on all displays ...

    let saved = CURSOR_RESTORE_POS.lock().ok().and_then(|mut g| g.take());
    if restore_position {
        // 只有 server 路径才 warp 回保存的位置
        if let Some((x, y)) = saved {
            CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
        }
    }
    // ... CGAssociate(true) ...
}
```

### Client Enter 路径

[client.rs](../src/net/client.rs) 的 Enter 处理改为：

```rust
Message::Enter { x, y } => {
    // ...
    if cursor_hidden {
        capture::show_local_cursor_no_restore(); // 不恢复旧位置
        cursor_hidden = false;
    }
    simulator.move_to(x, y);  // 唯一的权威 warp
}
```

`move_to(x, y)` 是唯一的定位操作，不会被 restore warp 覆盖。

### `get_screen_info` 改用 `CGDisplayBounds`

[screen.rs](../src/screen.rs) macOS 端从 `pixels_wide()/pixels_high()` 改为
`bounds().size.width/height`。虽然在测试机器上两者一致，但 `CGDisplayBounds`
是 Apple 文档中明确保证和 `CGEvent::location()` 同坐标系的 API，语义更安全。

---

## 两个 show 函数的使用场景

| 函数 | 场景 | 为什么 |
|------|------|--------|
| `show_local_cursor()` | Server: 鼠标从 client 返回 | 需要 restore —— 恢复用户离开 server 前的光标位置 |
| `show_local_cursor_no_restore()` | Client: 鼠标从 server 进入 | 不需要 restore —— 入口坐标由 `move_to()` 控制 |
| `show_local_cursor()` | Teardown: 进程退出时恢复 | 需要 restore —— 确保光标回到正常位置 |

---

## 变更文件清单

- [src/input/capture.rs](../src/input/capture.rs) —— 新增 `show_local_cursor_no_restore()`，
  内部拆分为 `show_local_cursor_inner(restore_position: bool)`
- [src/net/client.rs](../src/net/client.rs) —— Enter handler 改用 `show_local_cursor_no_restore()`
- [src/screen.rs](../src/screen.rs) —— macOS `get_screen_info` 从 `pixels_wide/high` 改为
  `bounds().size`；增加启动日志对比两种 API 的返回值
- [src/net/server.rs](../src/net/server.rs) —— Enter 日志增加完整诊断
  (cursor 原始坐标、映射坐标、双方屏幕尺寸、edge 方向)
