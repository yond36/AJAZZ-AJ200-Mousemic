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
    /// 前进键联动热键名; None或"无" = 不联动。绑定热键时该键 AI(语音)自动启用。
    pub hotkey_forward: Option<String>,
    /// 后退键联动热键名; None或"无" = 不联动。绑定热键时该键 AI(语音)自动启用。
    pub hotkey_backward: Option<String>,
    /// 前进键 AI 语音开关(独立于热键): true 时即使未绑定联动热键, 长按也触发语音
    /// (纯麦克风, 不注入按键)。绑定热键时隐式启用, 无需置 true。
    #[serde(default)]
    pub ai_fwd: bool,
    /// 后退键 AI 语音开关(独立于热键), 语义同 ai_fwd。
    #[serde(default)]
    pub ai_bwd: bool,
    /// 热键注入方式: "sendinput" | "interception"。
    pub driver: String,
    /// 启动时最小化到托盘 (配合 --autostart / 开机自启使用; 运行中关闭窗口始终收进托盘)。
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
            // 默认关闭: 旧配置无此字段 → 保持旧行为(仅绑了热键的键启用语音)。
            // 勾选 GUI 的"仅语音(无热键)"或 CLI 不带 --hotkey 时置 true。
            ai_fwd: false,
            ai_bwd: false,
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
                if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    // 显式 0x 前缀 → 十六进制
                    u16::from_str_radix(hex, 16).ok()
                } else {
                    // 无前缀 → 先按十进制; 失败再兜底试十六进制 (兼容旧写法 "ED03")
                    t.parse::<u16>().ok().or_else(|| u16::from_str_radix(t, 16).ok())
                }
            })
            .collect()
    }

    pub fn wired_pids(&self) -> std::collections::HashSet<u16> { Self::coerce_pids(&self.wired_pids) }
    pub fn wireless_pids(&self) -> std::collections::HashSet<u16> { Self::coerce_pids(&self.wireless_pids) }

    /// 热键名是否"真实绑定"(None 或字面量 "无" 都视为未绑定)。
    fn hotkey_bound(name: &Option<String>) -> bool {
        matches!(name.as_deref(), Some(n) if n != "无")
    }

    /// 前进键 AI(语音)是否启用: 绑定了联动热键, 或独立开关 ai_fwd 开启(仅语音模式)。
    pub fn ai_forward_enabled(&self) -> bool {
        Self::hotkey_bound(&self.hotkey_forward) || self.ai_fwd
    }

    /// 后退键 AI(语音)是否启用: 绑定了联动热键, 或独立开关 ai_bwd 开启(仅语音模式)。
    pub fn ai_backward_enabled(&self) -> bool {
        Self::hotkey_bound(&self.hotkey_backward) || self.ai_bwd
    }

    pub fn load() -> Config {
        let mut cfg = match std::fs::read_to_string(Self::config_path()) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("配置解析失败 ({}), 已回退默认配置: {}", Self::config_path().display(), e);
                    Config::default()
                }
            },
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
            let exe = std::env::current_exe().map_err(std::io::Error::other)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_enabled_decisions() {
        let mut c = Config::default();
        // 默认: 前进绑了 R_alt → 隐式启用; 后退没绑 → 禁用
        assert!(c.ai_forward_enabled());
        assert!(!c.ai_backward_enabled());
        // 后退开"仅语音" → 不绑热键也启用
        c.ai_bwd = true;
        assert!(c.ai_backward_enabled());
        // 旧式字面量 "无" 视为未绑定
        c.hotkey_forward = Some("无".to_string());
        c.ai_fwd = false;
        assert!(!c.ai_forward_enabled());
        c.hotkey_backward = Some("无".to_string());
        c.ai_bwd = false; // 复位之前设的仅语音开关
        assert!(!c.ai_backward_enabled());
    }

    #[test]
    fn coerce_pids_formats() {
        let set = Config::coerce_pids(&[
            "0xED03".to_string(), // 显式十六进制
            "57003".to_string(),  // 十进制
            "1234".to_string(),   // 十进制 1234 (不应被当成 0x1234)
            "ED03".to_string(),   // 无前缀十六进制 (兼容旧写法)
            "垃圾".to_string(),   // 无效输入
        ]);
        assert!(set.contains(&0xED03));
        assert!(set.contains(&57003));
        assert!(set.contains(&1234));
        assert!(!set.contains(&0x1234));
        assert_eq!(set.len(), 3); // 0xED03 与 ED03 是同一个值, 去重后 3 个
    }
}
