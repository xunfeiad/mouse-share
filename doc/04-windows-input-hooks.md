# Windows 输入系统：低级鼠标钩子与 SendInput

Windows 通过 Win32 API 提供全局输入拦截和模拟能力。本项目使用低级鼠标钩子 (WH_MOUSE_LL) 捕获事件，使用 SendInput 模拟事件。

## 一、Windows 输入事件架构

```
硬件 (鼠标)
     │
     ▼
┌──────────────┐
│  HID Driver   │  ← 硬件驱动
└──────┬───────┘
       ▼
┌──────────────┐
│   Win32k.sys  │  ← 内核态输入子系统
└──────┬───────┘
       │
       ├──▶ Low-Level Hook (WH_MOUSE_LL) ←── 我们在这里拦截
       │
       ▼
┌──────────────┐
│  Message Queue │  ← 目标窗口的消息队列
└──────┬───────┘
       ▼
┌──────────────┐
│  Application  │  ← WM_MOUSEMOVE 等消息
└──────────────┘
```

## 二、SetWindowsHookEx — 全局鼠标钩子

### 核心 API

```rust
use windows::Win32::UI::WindowsAndMessaging::*;

// 安装低级鼠标钩子
let hook = unsafe {
    SetWindowsHookExW(
        WH_MOUSE_LL,            // 钩子类型：低级鼠标
        Some(mouse_hook_proc),   // 回调函数
        None,                    // 模块句柄（低级钩子传 None）
        0,                       // 线程 ID（0 = 全局钩子）
    )?
};
```

### WH_MOUSE_LL vs WH_MOUSE

| 特性 | WH_MOUSE_LL (低级) | WH_MOUSE (普通) |
|------|--------------------|--------------------|
| 拦截范围 | 全局所有进程 | 指定线程 |
| 运行位置 | 安装钩子的进程中 | 目标进程中（需要 DLL 注入） |
| 性能 | 每次跨进程调用 | 进程内调用 |
| 事件抑制 | 支持 | 支持 |
| 要求 | 需要消息泵 | 需要 DLL |

本项目选择 `WH_MOUSE_LL`，因为它不需要 DLL 注入，更容易实现和分发。

### 回调函数

```rust
// src/input/win_capture.rs

unsafe extern "system" fn mouse_hook_proc(
    code: i32,          // < 0 时必须调用 CallNextHookEx
    wparam: WPARAM,     // 消息类型（WM_MOUSEMOVE 等）
    lparam: LPARAM,     // 指向 MSLLHOOKSTRUCT 的指针
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // 解析鼠标数据
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    // info.pt.x, info.pt.y  — 光标绝对坐标
    // info.mouseData         — 滚轮 delta 或 X 按钮编号

    if should_suppress {
        LRESULT(1)  // 非零 = 吞掉事件
    } else {
        CallNextHookEx(None, code, wparam, lparam)  // 传递给下一个钩子
    }
}
```

### MSLLHOOKSTRUCT 结构

```c
typedef struct {
    POINT pt;           // 鼠标绝对坐标 (x, y)
    DWORD mouseData;    // 高 16 位：滚轮 delta 或 XBUTTON 编号
    DWORD flags;        // 事件标志
    DWORD time;         // 时间戳
    ULONG_PTR dwExtraInfo;
} MSLLHOOKSTRUCT;
```

### 鼠标消息类型 (wparam)

| 常量 | 值 | 含义 |
|------|----|------|
| `WM_MOUSEMOVE` | 0x0200 | 鼠标移动 |
| `WM_LBUTTONDOWN` | 0x0201 | 左键按下 |
| `WM_LBUTTONUP` | 0x0202 | 左键释放 |
| `WM_RBUTTONDOWN` | 0x0204 | 右键按下 |
| `WM_RBUTTONUP` | 0x0205 | 右键释放 |
| `WM_MBUTTONDOWN` | 0x0207 | 中键按下 |
| `WM_MBUTTONUP` | 0x0208 | 中键释放 |
| `WM_MOUSEWHEEL` | 0x020A | 垂直滚轮 |
| `WM_XBUTTONDOWN` | 0x020B | 侧键按下 |
| `WM_XBUTTONUP` | 0x020C | 侧键释放 |

### mouseData 字段解析

**滚轮事件 (WM_MOUSEWHEEL)**：
```rust
// 高 16 位是有符号 delta，单位为 WHEEL_DELTA (120)
let delta = ((info.mouseData >> 16) as i16) as f64 / 120.0;
// delta > 0: 向上滚动
// delta < 0: 向下滚动
```

**X 按钮事件 (WM_XBUTTONDOWN/UP)**：
```rust
// 高 16 位是按钮编号 (XBUTTON1=1, XBUTTON2=2)
let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u8;
```

## 三、消息泵 (Message Pump)

**关键：低级钩子必须有消息泵！** Windows 通过消息机制分发钩子事件。如果安装钩子的线程没有消息循环，钩子回调永远不会被调用。

```rust
// src/input/win_capture.rs

// 消息泵 — 阻塞当前线程
let mut msg = MSG::default();
unsafe {
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        // 不需要 TranslateMessage/DispatchMessage
        // 低级钩子的回调由系统直接调用
    }
}
```

`GetMessageW` 阻塞直到有消息到达。对于低级钩子，系统会向钩子线程发送特殊消息触发回调。

### 为什么不需要 DispatchMessage

普通窗口消息需要 `TranslateMessage` + `DispatchMessage` 送到窗口过程。但低级钩子的回调是系统在 `GetMessageW` 内部直接调用的，不经过窗口过程。

## 四、Thread-Local Storage 传递上下文

Windows 钩子回调是 `extern "system"` 函数，无法捕获闭包环境。项目使用 `thread_local!` 传递上下文：

```rust
thread_local! {
    static HOOK_SENDER: RefCell<Option<Sender<MouseEvent>>> = RefCell::new(None);
    static HOOK_SUPPRESS: RefCell<Option<Arc<AtomicBool>>> = RefCell::new(None);
    static LAST_POS: Cell<(i32, i32)> = Cell::new((0, 0));
}

// 安装钩子前设置
HOOK_SENDER.with(|s| *s.borrow_mut() = Some(sender));
HOOK_SUPPRESS.with(|s| *s.borrow_mut() = Some(suppress_flag));

// 回调中读取
HOOK_SENDER.with(|s| {
    if let Some(sender) = s.borrow().as_ref() {
        let _ = sender.try_send(mouse_event);
    }
});
```

这是安全的，因为钩子回调一定在安装钩子的同一线程上执行。

## 五、SendInput — 鼠标模拟

### 核心 API

```rust
use windows::Win32::UI::Input::KeyboardAndMouse::*;

let input = INPUT {
    r#type: INPUT_MOUSE,
    Anonymous: INPUT_0 {
        mi: MOUSEINPUT {
            dx: abs_x,        // X 坐标
            dy: abs_y,        // Y 坐标
            mouseData: 0,     // 滚轮 delta 或 X 按钮
            dwFlags: flags,   // 事件类型标志
            time: 0,
            dwExtraInfo: 0,
        },
    },
};

unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
```

### 坐标系统

Windows `SendInput` 使用 **归一化绝对坐标**：

```
原始像素坐标 (x, y)
       │
       ▼  归一化公式
(x / screen_width * 65535, y / screen_height * 65535)
```

```rust
// src/input/win_simulate.rs
let abs_x = (self.current_x / self.screen_w * 65535.0) as i32;
let abs_y = (self.current_y / self.screen_h * 65535.0) as i32;
```

65535 (0xFFFF) 是 Windows 定义的坐标范围上限。

### dwFlags 标志组合

| 标志 | 含义 |
|------|------|
| `MOUSEEVENTF_MOVE` | 鼠标移动 |
| `MOUSEEVENTF_ABSOLUTE` | 使用绝对坐标（否则是相对 delta） |
| `MOUSEEVENTF_LEFTDOWN` | 左键按下 |
| `MOUSEEVENTF_LEFTUP` | 左键释放 |
| `MOUSEEVENTF_RIGHTDOWN` | 右键按下 |
| `MOUSEEVENTF_RIGHTUP` | 右键释放 |
| `MOUSEEVENTF_MIDDLEDOWN` | 中键按下 |
| `MOUSEEVENTF_MIDDLEUP` | 中键释放 |
| `MOUSEEVENTF_WHEEL` | 垂直滚轮 |
| `MOUSEEVENTF_XDOWN` | X 按钮按下 |
| `MOUSEEVENTF_XUP` | X 按钮释放 |

标志可以组合：`MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE` 表示"移动到绝对坐标"。

### 获取屏幕尺寸

```rust
unsafe {
    let width = GetSystemMetrics(SM_CXSCREEN);   // 主屏幕宽度（像素）
    let height = GetSystemMetrics(SM_CYSCREEN);  // 主屏幕高度（像素）
}
```

## 六、`windows` Crate

本项目使用 Microsoft 官方的 [`windows`](https://github.com/microsoft/windows-rs) crate（非 `winapi`）。

### Cargo.toml 配置

```toml
[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = [
    "Win32_UI_WindowsAndMessaging",          # Hook API, GetSystemMetrics
    "Win32_UI_Input_KeyboardAndMouse",       # SendInput
    "Win32_Foundation",                       # POINT, LPARAM, LRESULT
    "Win32_Graphics_Gdi",                    # 屏幕相关
] }
```

`windows` crate 按 feature 粒度引入 API。只开启需要的 feature 可以显著减少编译时间。

### 与 winapi crate 的区别

| | `windows` | `winapi` |
|--|-----------|---------|
| 维护者 | Microsoft 官方 | 社区 |
| 类型安全 | 强类型（Result, Option） | 原始 C 类型 |
| 错误处理 | 返回 `windows::core::Result` | 需要手动检查返回值 |
| 更新 | 从 Windows 元数据自动生成 | 手动维护 |

## 七、权限要求

- 低级鼠标钩子 (`WH_MOUSE_LL`) 需要**管理员权限**才能跨进程拦截
- `SendInput` 无法向提升权限（以管理员运行）的窗口注入事件，除非自身也以管理员运行
- UAC 桌面切换时钩子暂时失效
