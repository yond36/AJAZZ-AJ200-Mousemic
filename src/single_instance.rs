//! 单实例: Windows 命名互斥量, 防止多开导致 HID/热键冲突。
//! 首次实例获取互斥量; 后续实例找到已有窗口并置前, 然后退出。

#[cfg(windows)]
mod imp {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{BOOL, ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, IsIconic, SW_RESTORE, SW_SHOW,
    };

    /// 尝试获取单实例锁。返回 true 表示本进程是第一个实例; false 表示已有实例在运行。
    pub fn try_acquire() -> bool {
        let name: Vec<u16> = "Global\\mousemic_single\0".encode_utf16().collect();
        unsafe {
            let handle = match CreateMutexW(None, BOOL::from(false), PCWSTR(name.as_ptr())) {
                Ok(h) => h,
                Err(_) => return false,
            };
            if handle.is_invalid() {
                return false;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return false;
            }
            true
        }
    }

    /// 找到已有实例的窗口并置前。若找不到窗口 (如最小化到托盘) 则不做任何事。
    pub fn bring_existing_to_front() {
        let class: Vec<u16> = "NativeWindowsGuiWindow\0".encode_utf16().collect();
        let title: Vec<u16> = "AJAZZ 语音鼠标桥接器\0".encode_utf16().collect();
        unsafe {
            let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr())) else {
                return;
            };
            if hwnd.0.is_null() {
                return;
            }
            // 若最小化则还原
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn try_acquire() -> bool { true }
    pub fn bring_existing_to_front() {}
}

pub use imp::{try_acquire, bring_existing_to_front};
