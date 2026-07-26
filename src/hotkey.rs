//! 联动热键: 按住鼠标语音键时, 合成一次键盘按键 (SendInput 扫描码), 与目标软件天然同步。
//!
//! 两种注入后端:
//! - `sendinput` (默认): 用 Windows `SendInput` 合成扫描码。系统会把它翻译成对应 VK
//!   (如右 Alt=VK_RMENU)。绝大多数软件 (豆包等) 在 LL hook 层就能收到。
//! - `interception`: 内核驱动方式, 能穿透 Raw Input 监听 (需要安装 Interception 驱动 +
//!   把 interception.dll 放到 exe 同目录)。部分用 Raw Input 监听热键的软件 (输入法/Discord)
//!   识别不到模拟按键时换它。
//!
//! 移植自 Python 的 HotKey / HotKeyInterception 与 HOTKEYS 表。

use std::path::PathBuf;

/// 可选热键名列表 (供 GUI 下拉 / CLI 校验)。
pub const HOTKEY_NAMES: &[&str] = &[
    "L_alt", "R_alt", "R_ctrl", "R_shift",
    "f9", "f10", "space", "grave", "capslock",
    "R_alt+space", "R_alt+R_shift", "R_alt+R_ctrl",
];

/// 单键名 -> (Set-1 扫描码, 是否扩展键)。与 Python HOTKEYS 表一一对应。
/// 同时兼容旧名 (left_alt/right_alt 等) 和新缩写 (L_alt/R_alt 等)。
fn single_scan(name: &str) -> Option<(u8, bool)> {
    let v = match name {
        "L_alt" | "left_alt" => (0x38, false),
        "R_alt" | "right_alt" => (0x38, true),
        "R_ctrl" | "right_ctrl" => (0x1D, true),
        "R_shift" | "right_shift" => (0x36, false),
        "f9" => (0x43, false),
        "f10" => (0x44, false),
        "space" => (0x39, false),
        "grave" => (0x29, false),
        "capslock" => (0x3A, false),
        _ => return None,
    };
    Some(v)
}

/// 名称 -> 扫描码列表 (支持 "+" 分隔的组合键)。
pub fn hotkey_scans(name: &str) -> Option<Vec<(u8, bool)>> {
    let parts: Vec<&str> = name.split('+').collect();
    let mut result = Vec::with_capacity(parts.len());
    for p in &parts {
        result.push(single_scan(p.trim())?);
    }
    if result.is_empty() { None } else { Some(result) }
}

/// 兼容旧接口: 单键查询。
pub fn hotkey_scan(name: &str) -> Option<(u8, bool)> {
    single_scan(name)
}

enum Backend {
    SendInput,
    #[allow(dead_code)]
    Interception(InterceptionCtx),
}

/// 联动热键管理器: 跟踪"按下"状态, 避免重复发送; 离开作用域时自动抬起 (防卡键)。
/// 支持组合键 (多个扫描码同时按下/释放)。
pub struct HotKey {
    scans: Vec<(u8, bool)>,
    down: bool,
    #[allow(dead_code)]
    backend: Backend,
}

impl HotKey {
    /// 按名称与驱动创建。driver: "sendinput" | "interception"。
    /// 支持组合键名如 "right_alt+space"。
    pub fn new(name: &str, driver: &str) -> anyhow::Result<Self> {
        let scans = hotkey_scans(name)
            .ok_or_else(|| anyhow::anyhow!("未知热键名: {} (可选: {:?})", name, HOTKEY_NAMES))?;
        let backend = match driver {
            "interception" => Backend::Interception(InterceptionCtx::new()?),
            _ => Backend::SendInput,
        };
        Ok(HotKey { scans, down: false, backend })
    }

    /// 按住 (若已按住则忽略)。
    pub fn press(&mut self) {
        if !self.down {
            self.inject(false);
            self.down = true;
        }
    }

    /// 抬起 (若已抬起则忽略)。
    pub fn release(&mut self) {
        if self.down {
            self.inject(true);
            self.down = false;
        }
    }

    pub fn is_down(&self) -> bool {
        self.down
    }

    /// 短按 (按下后立即松开)。
    pub fn tap(&mut self) {
        self.inject(false);
        std::thread::sleep(std::time::Duration::from_millis(30));
        self.inject(true);
        self.down = false;
    }

    /// 退出前确保抬起, 防止热键卡在按下状态。
    pub fn close(&mut self) {
        self.release();
    }

    fn inject(&self, keyup: bool) {
        if keyup {
            // 释放: 反序 (先释放主键, 再释放修饰键)
            for &(sc, ext) in self.scans.iter().rev() {
                match &self.backend {
                    Backend::SendInput => send_scan(sc, ext, true),
                    Backend::Interception(ctx) => ctx.inject(sc, ext, true),
                }
            }
        } else {
            // 按下: 正序 (先按修饰键, 再按主键)
            for &(sc, ext) in &self.scans {
                match &self.backend {
                    Backend::SendInput => send_scan(sc, ext, false),
                    Backend::Interception(ctx) => ctx.inject(sc, ext, false),
                }
            }
        }
    }
}

impl Drop for HotKey {
    fn drop(&mut self) {
        self.release();
    }
}

// ----------------------------------------------------------------------------
// SendInput 后端 (Windows)
// ----------------------------------------------------------------------------

#[cfg(windows)]
fn send_scan(scancode: u8, extended: bool, keyup: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, SendInput, VIRTUAL_KEY,
    };

    const F_SCANCODE: u32 = 0x0008;
    const F_EXTENDED: u32 = 0x0001;
    const F_KEYUP: u32 = 0x0002;

    let mut flags = F_SCANCODE;
    if extended {
        flags |= F_EXTENDED;
    }
    if keyup {
        flags |= F_KEYUP;
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode as u16,
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(std::slice::from_ref(&input), std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(not(windows))]
fn send_scan(_scancode: u8, _extended: bool, _keyup: bool) {
    // 非 Windows 平台无 SendInput; 仅作为库可编译的占位 (本工具仅 Windows 运行)。
}

/// 注入一个 Enter 键 (按下+释放)。
#[cfg(windows)]
pub fn inject_enter() {
    // Enter 扫描码 0x1C, 非扩展键
    send_scan(0x1C, false, false); // 按下
    send_scan(0x1C, false, true);  // 释放
}

/// 注入 Ctrl+Enter (按下 Ctrl → 按下 Enter → 释放 Enter → 释放 Ctrl)。
#[cfg(windows)]
pub fn inject_ctrl_enter() {
    send_scan(0x1D, false, false); // Ctrl 按下
    send_scan(0x1C, false, false); // Enter 按下
    send_scan(0x1C, false, true);  // Enter 释放
    send_scan(0x1D, false, true);  // Ctrl 释放
}

#[cfg(not(windows))]
pub fn inject_enter() {}

#[cfg(not(windows))]
pub fn inject_ctrl_enter() {}

// ----------------------------------------------------------------------------
// Interception 内核驱动后端 (Windows, 可选)
// ----------------------------------------------------------------------------

#[cfg(windows)]
struct InterceptionCtx {
    _lib: libloading::Library,
    ctx: *mut std::os::raw::c_void,
    device: i32,
}

#[cfg(windows)]
unsafe impl Send for InterceptionCtx {}

#[cfg(windows)]
impl InterceptionCtx {
    fn new() -> anyhow::Result<Self> {
        let lib = load_interception_dll()
            .map_err(|e| anyhow::anyhow!("找不到 interception.dll: {} (需安装 Interception 驱动并放到 exe 同目录)", e))?;

        type CreateCtx = unsafe extern "C" fn() -> *mut std::os::raw::c_void;
        type IsKb = unsafe extern "C" fn(i32) -> i32;
        type IsInvalid = unsafe extern "C" fn(i32) -> i32;
        type GetHwid = unsafe extern "C" fn(*mut std::os::raw::c_void, i32, *mut u16, u32) -> u32;

        let create: libloading::Symbol<CreateCtx> = unsafe {
            lib.get(b"interception_create_context")
                .map_err(|e| anyhow::anyhow!("interception 符号缺失: {}", e))?
        };
        let ctx = unsafe { create() };
        if ctx.is_null() {
            return Err(anyhow::anyhow!("Interception 上下文创建失败 (驱动未安装或未重启?)"));
        }

        // 选键盘设备: 优先 AJAZZ 鼠标自带的键盘接口, 否则第一个非虚拟实体键盘。
        let is_kb: libloading::Symbol<IsKb> = unsafe { lib.get(b"interception_is_keyboard")? };
        let is_invalid: libloading::Symbol<IsInvalid> = unsafe { lib.get(b"interception_is_invalid")? };
        let get_hwid: libloading::Symbol<GetHwid> = unsafe { lib.get(b"interception_get_hardware_id")? };

        let mut best = 0i32;
        let mut fallback = 0i32;
        for dev in 1..11 {
            if unsafe { is_invalid(dev) } != 0 || unsafe { is_kb(dev) } == 0 {
                continue;
            }
            let mut buf = [0u16; 512];
            let n = unsafe { get_hwid(ctx, dev, buf.as_mut_ptr(), 512) };
            if n == 0 {
                continue;
            }
            let hwid: String = buf[..(n as usize).min(buf.len())]
                .iter()
                .map(|&c| char::from_u32(c as u32).unwrap_or('\0'))
                .collect();
            if hwid.contains("VID_363C") {
                best = dev;
                break;
            }
            if fallback == 0 && !hwid.contains("GVInput") {
                fallback = dev;
            }
        }
        let device = if best != 0 { best } else { fallback };
        if device == 0 {
            return Err(anyhow::anyhow!("Interception 找不到可用键盘设备"));
        }
        Ok(InterceptionCtx { _lib: lib, ctx, device })
    }

    fn inject(&self, scancode: u8, extended: bool, keyup: bool) {
        use libloading::Symbol;
        type SendFn = unsafe extern "C" fn(*mut std::os::raw::c_void, i32, *mut u8, u32) -> i32;
        let send: Symbol<SendFn> = match unsafe { self._lib.get(b"interception_send") } {
            Ok(s) => s,
            Err(_) => return,
        };
        // InterceptionKeyStroke: code(u16) state(u16) information(u32); 缓冲 20 字节 (与鼠标 stroke 同尺寸)。
        // 注意 scancode 是 u8, 但 Interception 的 code 字段是 u16 (小端), 必须 cast 成 u16 再 to_le_bytes
        // (否则 1 字节拷给 2 字节切片 → panic)。
        let state = (if keyup { 0x01 } else { 0x00 }) | (if extended { 0x02 } else { 0x00 });
        let mut buf = [0u8; 20];
        buf[0..2].copy_from_slice(&(scancode as u16).to_le_bytes());
        buf[2..4].copy_from_slice(&(state as u16).to_le_bytes());
        unsafe {
            send(self.ctx, self.device, buf.as_mut_ptr(), 1);
        }
    }
}

#[cfg(windows)]
impl Drop for InterceptionCtx {
    fn drop(&mut self) {
        type DestroyCtx = unsafe extern "C" fn(*mut std::os::raw::c_void);
        if let Ok(d) = unsafe { self._lib.get::<DestroyCtx>(b"interception_destroy_context") } {
            unsafe { d(self.ctx) };
        }
    }
}

#[cfg(windows)]
fn load_interception_dll() -> std::io::Result<libloading::Library> {
    let candidates: Vec<PathBuf> = {
        let mut v = vec![PathBuf::from("interception.dll")];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                v.insert(0, dir.join("interception.dll"));
            }
        }
        v
    };
    let mut last = None;
    for c in &candidates {
        match unsafe { libloading::Library::new(c) } {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{:?}", last),
    ))
}
