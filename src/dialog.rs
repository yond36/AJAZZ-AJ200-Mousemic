//! 错误可视化: 在 Windows 上用 MessageBox 弹出错误。
//!
//! 关键用途: 发布版 (`windows_subsystem = "windows"`) 没有控制台, 任何 panic 或
//! `eframe::run_native` 返回的错误都会“无声无息”地消失, 表现为“只有黑框/什么都没发生”。
//! 这里集中提供: 弹出错误框 + 安装 panic 钩子, 让所有失败都看得见。

#[cfg(windows)]
mod imp {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    /// 弹出一个错误对话框 (阻塞, 直到用户点确定)。
    pub fn show_error_box(msg: &str) {
        let title: Vec<u16> = "MouseMic 错误".encode_utf16().collect();
        let text: Vec<u16> = msg.encode_utf16().collect();
        unsafe {
            let _ = MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    /// 安装 panic 钩子: 任何未捕获 panic 都弹出对话框, 而不是只写进不可见的 stderr。
    pub fn install_panic_hook() {
        std::panic::set_hook(Box::new(|info| {
            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "未知错误".to_string()
            };
            let loc = if let Some(l) = info.location() {
                format!(" ({}:{})", l.file(), l.line())
                } else {
                String::new()
            };
            show_error_box(&format!("MouseMic 发生未捕获错误:\n{}{}", payload, loc));
        }));
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn show_error_box(_msg: &str) {}
    pub fn install_panic_hook() {}
}

pub use imp::*;
