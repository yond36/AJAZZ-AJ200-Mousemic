//! 配置: 与 Python 版 mousemic_gui.json 兼容的 JSON schema, 加注册表自启。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_mode() -> String { "play".to_string() }
fn default_cable() -> String { "CABLE Input".to_string() }
fn default_driver() -> String { "sendinput".to_string() }
fn default_auto_enter_mode() -> String { "enter".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 输出模式: "play" = 扬声器试听; "cable" = 转发到虚拟麦克风 (VB-CABLE)。
    pub mode: String,
    /// 虚拟声卡输入设备名, 如 "CABLE Input"。
    pub cable_device: String,
    /// 前进键联动热键名; None或"无" = 不联动=禁用该键AI。
    pub hotkey_forward: Option<String>,
    /// 后退键联动热键名; None或"无" = 不联动=禁用该键AI。
    pub hotkey_backward: Option<String>,
    /// 热键注入方式: "sendinput" | "interception"。
    pub driver: String,
    /// 关闭窗口时最小化到托盘。
    pub minimize_to_tray: bool,
    /// 启动 GUI 时自动启动桥接服务。
    pub auto_start_service: bool,
    /// 手动指定"有线" PID 集合 (兜底分类用, 字符串可写 0xED03 或十进制)。
    #[serde(default)]
    pub wired_pids: Vec<String>,
    /// 手动指定"无线" PID 集合。
    #[serde(default)]
    pub wireless_pids: Vec<String>,
    /// 注册表自启状态 (不持久化到 JSON, 单独走注册表)。
    #[serde(skip)]
    pub autostart: bool,
    /// 语音结束后自动按回车: 是否启用。
    #[serde(default)]
    pub auto_enter: bool,
    /// 回车模式: "enter" 或 "ctrl_enter"。
    #[serde(default = "default_auto_enter_mode")]
    pub auto_enter_mode: String,
    /// 语音结束后的延迟 (秒)。
    #[serde(default)]
    pub auto_enter_delay: f64,
    /// Typeless 模式 (前进键): 按住语音键=短按热键, 松开语音键=音频结束后再短按热键。
    #[serde(default)]
    pub typeless_fwd: bool,
    /// Typeless 模式 (后退键)。
    #[serde(default)]
    pub typeless_bwd: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: default_mode(),
            cable_device: default_cable(),
            hotkey_forward: Some("R_alt".to_string()),
            hotkey_backward: None,
            driver: default_driver(),
            minimize_to_tray: false,
            auto_start_service: false,
            wired_pids: vec![],
            wireless_pids: vec![],
            autostart: false,
            auto_enter: false,
            auto_enter_mode: default_auto_enter_mode(),
            auto_enter_delay: 0.5,
            typeless_fwd: false,
            typeless_bwd: false,
        }
    }
}

impl Config {
    /// 程序所在目录 (打包后为 exe 目录, 否则为源码/工作目录)。
    pub fn app_dir() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn config_path() -> PathBuf {
        Self::app_dir().join("mousemic_gui.json")
    }

    /// 把字符串 PID ("0xED03" / "57003") 解析为 u16 集合。
    pub fn coerce_pids(list: &[String]) -> std::collections::HashSet<u16> {
        list.iter()
            .filter_map(|v| {
                let t = v.trim();
                if let Ok(x) = u16::from_str_radix(t.trim_start_matches("0x").trim_start_matches("0X"), 16) {
                    Some(x)
                } else {
                    t.parse::<u16>().ok()
                }
            })
            .collect()
    }

    pub fn wired_pids(&self) -> std::collections::HashSet<u16> { Self::coerce_pids(&self.wired_pids) }
    pub fn wireless_pids(&self) -> std::collections::HashSet<u16> { Self::coerce_pids(&self.wireless_pids) }

    pub fn load() -> Config {
        let mut cfg = match std::fs::read_to_string(Self::config_path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| Config::default()),
            Err(_) => Config::default(),
        };
        cfg.autostart = is_autostart_on();
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let mut clone = self.clone();
        clone.autostart = is_autostart_on(); // 不写进 JSON
        let s = serde_json::to_string_pretty(&clone).unwrap_or_default();
        std::fs::write(Self::config_path(), s)
    }
}

#[cfg(windows)]
mod registry {
    use winreg::enums::*;
    use winreg::RegKey;

    const REG_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const REG_NAME: &str = "MouseMic";

    pub fn set_autostart(enable: bool) -> std::io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.open_subkey_with_flags(REG_KEY, KEY_SET_VALUE)
            .or_else(|_| hkcu.create_subkey_with_flags(REG_KEY, KEY_SET_VALUE).map(|(k, _)| k))?;
        if enable {
            // 打包后直接跑 exe 自身; 开发模式用 pythonw 跑脚本由 GUI 决定, 这里统一用 exe 路径。
            let exe = std::env::current_exe().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            let val = format!("\"{}\" --autostart", exe.display());
            key.set_value(REG_NAME, &val)
        } else {
            let _ = key.delete_value(REG_NAME);
            Ok(())
        }
    }

    pub fn is_autostart_on() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu.open_subkey_with_flags(REG_KEY, KEY_QUERY_VALUE) {
            Ok(key) => key.get_value::<String, _>(REG_NAME).is_ok(),
            Err(_) => false,
        }
    }
}

#[cfg(not(windows))]
mod registry {
    pub fn set_autostart(_enable: bool) -> std::io::Result<()> { Ok(()) }
    pub fn is_autostart_on() -> bool { false }
}

pub use registry::{is_autostart_on, set_autostart};
