//! AJ200 系列设备注册表: 支持的 PID 清单 + PID -> 型号/传感器/飞轮信息。
//!
//! 设备识别改为 PID 白名单制: 只有 PID 命中 [`SUPPORTED_PIDS`] 的 AJAZZ (VID=0x363C)
//! 设备才参与音频/命令通道枚举与连接。GUI 依赖检查里的"鼠标"项据此显示识别到的型号。

/// 单条设备记录。
#[derive(Debug, Clone, Copy)]
pub struct DeviceInfo {
    pub pid: u16,
    pub model: &'static str,
    pub sensor: &'static str,
    /// 是否带飞轮 (滚轮)。
    pub flywheel: bool,
    /// 同一 PID 被多个型号共用时的消歧备注 (如 0xFC0C)。
    pub note: &'static str,
}

const fn d(pid: u16, model: &'static str, sensor: &'static str, flywheel: bool, note: &'static str) -> DeviceInfo {
    DeviceInfo { pid, model, sensor, flywheel, note }
}

/// AJ200 系列全部支持的 (PID, 型号) 记录 (22 条记录 / 21 个唯一 PID, 0xFC0C 两型号共用)。
pub const SUPPORTED_DEVICES: &[DeviceInfo] = &[
    // AJ200 NL AI MC (3311)
    d(0xED03, "AJ200 NL AI MC", "PAW3311", false, ""),
    d(0xED00, "AJ200 NL AI MC", "PAW3311", false, ""),
    d(0xED05, "AJ200P NL AI MC", "PAW3311", false, ""),
    d(0xED06, "AJ200P NL AI MC", "PAW3311", false, ""),
    // AJ200 NL AI PRO+ (3395)
    d(0xED07, "AJ200 NL AI PRO+", "PAW3395", false, ""),
    d(0xED08, "AJ200 NL AI PRO+", "PAW3395", false, ""),
    d(0xFC08, "AJ200 NL AI PRO+", "PAW3395", false, ""),
    // AJ200P NL AI ULTRA (3950/3955)
    d(0xED09, "AJ200P NL AI ULTRA", "PAW3950", false, ""),
    d(0xED0A, "AJ200P NL AI ULTRA", "PAW3950", false, ""),
    d(0xFC0A, "AJ200P NL AI ULTRA", "PAW3950", false, ""),
    d(0xED1E, "AJ200P NL AI ULTRA", "PAW3955", false, ""),
    // AJ200P NL AI ULTRA+ (3950, 飞轮)
    d(0xFC0C, "AJ200P NL AI ULTRA+", "PAW3950", true, ""),
    d(0xED0B, "AJ200P NL AI ULTRA+", "PAW3950", true, ""),
    d(0xED0C, "AJ200P NL AI ULTRA+", "PAW3950", true, ""),
    // AJ200P NL AI ULTRA-3955 (飞轮; 0xFC0C 与 ULTRA+ 同 PID 共用)
    d(0xFC0C, "AJ200P NL AI ULTRA-3955", "PAW3950*", true, "与 ULTRA+ 同 PID"),
    d(0xED1D, "AJ200P NL AI ULTRA-3955", "PAW3955", true, ""),
    // AJ200P AI MASTER (3955, 飞轮)
    d(0xFC1E, "AJ200P AI MASTER", "PAW3955", true, ""),
    d(0xED1B, "AJ200P AI MASTER", "PAW3955", true, ""),
    d(0xED1C, "AJ200P AI MASTER", "PAW3955", true, ""),
    // AJ200P NL AI S ULTRA (3955, 飞轮)
    d(0xFC1C, "AJ200P NL AI S ULTRA", "PAW3955", true, ""),
    d(0xED25, "AJ200P NL AI S ULTRA", "PAW3955", true, ""),
    d(0xED26, "AJ200P NL AI S ULTRA", "PAW3955", true, ""),
];

/// 支持的 PID 集合 (25 个唯一值)。
pub fn supported_pids() -> std::collections::HashSet<u16> {
    SUPPORTED_DEVICES.iter().map(|d| d.pid).collect()
}

/// PID 是否在支持列表内。
pub fn is_supported(pid: u16) -> bool {
    SUPPORTED_DEVICES.iter().any(|d| d.pid == pid)
}

/// 按 PID 查所有匹配记录 (0xFC0C 会返回两条)。
pub fn lookup(pid: u16) -> Vec<&'static DeviceInfo> {
    SUPPORTED_DEVICES.iter().filter(|d| d.pid == pid).collect()
}

/// 格式化 PID 命中的型号描述, 如:
///   "AJ200P NL AI ULTRA+ (PAW3950, 飞轮)"
/// 同 PID 多型号: "AJ200P NL AI ULTRA+ / AJ200P NL AI ULTRA-3955 (与 ULTRA+ 同 PID) (PAW3950, 飞轮)"
pub fn describe_pid(pid: u16) -> Option<String> {
    let recs = lookup(pid);
    if recs.is_empty() {
        return None;
    }
    let first = recs[0];
    let model = if recs.len() == 1 {
        first.model.to_string()
    } else {
        recs.iter()
            .map(|r| {
                if r.note.is_empty() {
                    r.model.to_string()
                } else {
                    format!("{} ({})", r.model, r.note)
                }
            })
            .collect::<Vec<_>>()
            .join(" / ")
    };
    Some(format!(
        "{} ({}{})",
        model,
        first.sensor,
        if first.flywheel { ", 飞轮" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_pid_count() {
        assert_eq!(supported_pids().len(), 21);
        assert_eq!(SUPPORTED_DEVICES.len(), 22);
    }

    #[test]
    fn lookup_shared_pid() {
        let recs = lookup(0xFC0C);
        assert_eq!(recs.len(), 2);
        assert!(describe_pid(0xFC0C).unwrap().contains("ULTRA+"));
        assert!(describe_pid(0xFC0C).unwrap().contains("ULTRA-3955"));
    }

    #[test]
    fn unknown_pid_unsupported() {
        assert!(!is_supported(0x1234));
        assert!(describe_pid(0x1234).is_none());
        assert!(is_supported(0xED03));
    }
}
