//! HID 通信: 枚举 AJAZZ 鼠标 (VID=363C) 的音频接口、激活握手 (ARM_SEQ)、命令通道自动探测、
//! 有线/无线分类与链路切换。忠实移植自 Python 版 mousemic.py。
//!
//! 注意: hidapi Rust crate 的 `DeviceInfo` 访问方式 (`vendor_id()` 等方法 / `path()` 返回类型)
//! 在不同小版本可能略有差异; 若编译报字段/方法不匹配, 按 hidapi 文档微调即可 (逻辑不变)。

use crate::{AUDIO_USAGE_PAGE, CMD_USAGE_PAGES, VID};
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::ffi::CString;

/// 把存储的 path 字符串转为 CString 并打开 HID 设备。
/// HID 路径通常为 ASCII (如 `\\?\hid#vid_363c...`), 安全; 含 NUL 则返回 None。
fn open_by_path(api: &HidApi, path: &str) -> Option<HidDevice> {
    let c = CString::new(path.as_bytes()).ok()?;
    api.open_path(&c).ok()
}

#[derive(Debug, Clone)]
pub struct AjazzDevice {
    pub path: String,
    pub product_id: u16,
    pub product_string: String,
}

/// AJAZZ 激活序列 (与官方驱动会话逐字节一致)。每条 64 字节, 首字节为 ReportID 0x0B。
fn pkt(c: u8, tail: &[u8]) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = 0x0B;
    b[1] = c;
    let n = tail.len().min(62);
    b[2..2 + n].copy_from_slice(&tail[..n]);
    b
}


/// 构造激活序列 (等价于 Python 的 ARM_SEQ 列表)。
pub fn build_arm_seq() -> Vec<[u8; 64]> {
    vec![
        pkt(0x13, &[]),
        pkt(0x13, &[]),
        pkt(0x14, &[]),
        pkt(0x15, &[]),
        pkt(0x17, &[]),
        pkt(0x19, &[]),
        pkt(0x51, &[]),
        pkt(0x55, &[
            0x1a, 0x18, 0x01, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x04, 0x00, 0x00, 0x08, 0x01, 0x00,
            0x10, 0x02, 0x00, 0x20, 0x00, 0x00, 0x40, 0x00, 0x00, 0x80, 0x00,
        ]),
    ]
}

/// 判断某 HID 设备是有线还是无线 (主依据 product_string, 配置 PID 仅兜底)。
pub fn classify_link(ps: &str, wired_pids: &HashSet<u16>, wireless_pids: &HashSet<u16>, pid: u16) -> &'static str {
    if wired_pids.contains(&pid) {
        return "wired";
    }
    if wireless_pids.contains(&pid) {
        return "wireless";
    }
    let s = ps.to_ascii_lowercase();
    if s.contains("2.4g") {
        "wireless"
    } else if s.contains("mouse") {
        "wired"
    } else {
        "unknown"
    }
}

pub fn classify_label(link: &str) -> &'static str {
    match link {
        "wired" => "有线",
        "wireless" => "无线",
        _ => "未知",
    }
}

/// 枚举所有 AJAZZ 音频接口 (usage_page=0xFFAA)。
pub fn enumerate_audio(api: &HidApi) -> Vec<AjazzDevice> {
    api.device_list()
        .filter(|d| d.vendor_id() == VID && d.usage_page() == AUDIO_USAGE_PAGE)
        .map(|d| AjazzDevice {
            path: d.path().to_string_lossy().into_owned(),
            product_id: d.product_id(),
            product_string: d.product_string().unwrap_or("").to_string(),
        })
        .collect()
}

/// 按 有线 > 无线 > 未知 排序的候选音频链路。
fn priority_order(
    api: &HidApi,
    wired_pids: &HashSet<u16>,
    wireless_pids: &HashSet<u16>,
    exclude_path: Option<&str>,
) -> Vec<AjazzDevice> {
    let mut v = enumerate_audio(api);
    if let Some(ex) = exclude_path {
        v.retain(|d| d.path != ex);
    }
    let rank = |d: &AjazzDevice| -> u8 {
        match classify_link(&d.product_string, wired_pids, wireless_pids, d.product_id) {
            "wired" => 0,
            "wireless" => 1,
            _ => 2,
        }
    };
    v.sort_by_key(rank);
    v
}

/// 候选命令通道路径列表 (按 CMD_USAGE_PAGES 优先级, 再兜底同 PID 非音频接口)。
pub fn find_command_paths(api: &HidApi, audio_pid: Option<u16>) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    if let Some(pid) = audio_pid {
        for &up in &CMD_USAGE_PAGES {
            for d in api.device_list() {
                if d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == up {
                    let p = d.path().to_string_lossy().into_owned();
                    if seen.insert(p.clone()) {
                        paths.push(p);
                    }
                }
            }
        }
        // 兜底: 同 PID 下非音频、非通用桌面 (0x0001) 的接口
        let skip: HashSet<u16> = [AUDIO_USAGE_PAGE, 0x0001].into_iter().collect();
        for d in api.device_list() {
            if d.vendor_id() == VID && d.product_id() == pid && !skip.contains(&d.usage_page()) {
                let p = d.path().to_string_lossy().into_owned();
                if seen.insert(p.clone()) {
                    paths.push(p);
                }
            }
        }
    }
    if paths.is_empty() {
        for &up in &CMD_USAGE_PAGES {
            for d in api.device_list() {
                if d.vendor_id() == VID && d.usage_page() == up {
                    let p = d.path().to_string_lossy().into_owned();
                    if seen.insert(p.clone()) {
                        paths.push(p);
                        break;
                    }
                }
            }
            if !paths.is_empty() {
                break;
            }
        }
    }
    paths
}

/// 打开命令通道并发送激活序列。握手成功返回保持打开的设备 (需存活整个运行期); 失败返回 None。
pub fn arm_mouse(api: &HidApi, audio_pid: Option<u16>) -> Option<HidDevice> {
    for path in find_command_paths(api, audio_pid) {
        let dev = match open_by_path(api, &path) {
            Some(d) => d,
            None => continue,
        };
        let seq = build_arm_seq();
        let mut ok = true;
        for p in &seq {
            if dev.write(p).unwrap_or(0) != 64 {
                ok = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            // 读 0x0A 应答; 读超时/失败视为握手不通过
            let mut rbuf = [0u8; 64];
            match dev.read_timeout(&mut rbuf, 300) {
                Ok(_) => {}
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return Some(dev);
        }
    }
    None
}

pub struct Connected {
    pub audio: HidDevice,
    pub cmd: HidDevice,
    pub control: Option<HidDevice>,
    pub path: String,
    pub pid: u16,
    pub product_string: String,
}

/// 开启 AI 语音功能 (使用官方驱动命令格式)。
pub fn ai_on(control: &HidDevice) -> bool {
    let mut report = [0u8; 64];
    report[0] = 0x0b;
    report[1] = 0x55;
    report[2] = 0x1a;
    report[3] = 0x10;  // 0x10 = AI ON
    report[5] = 0x01;
    // 官方 payload (report[6..28]): 00 00 02 00 00 04 00 00 08 01 00 10 00 00 20 00 00 40 00 00 80 00
    let payload: [u8; 22] = [0,0, 0x02,0,0, 0x04,0,0, 0x08,0x01,0, 0x10,0,0, 0x20,0,0, 0x40,0,0, 0x80,0];
    for (i, b) in payload.iter().enumerate() { report[6 + i] = *b; }
    // 发送两次, 间隔 50ms
    if control.write(&report).unwrap_or(0) != 64 { return false; }
    std::thread::sleep(std::time::Duration::from_millis(50));
    report[4] = 0x01;  // 第二次 byte[4]=1
    control.write(&report).unwrap_or(0) == 64
}

/// 关闭 AI 语音功能 (使用官方驱动命令格式)。
pub fn ai_off(control: &HidDevice) -> bool {
    let mut report = [0u8; 64];
    report[0] = 0x0b;
    report[1] = 0x55;
    report[2] = 0x1a;
    report[3] = 0x08;  // 0x08 = AI OFF
    report[5] = 0x01;
    let payload: [u8; 22] = [0,0, 0x02,0,0, 0x04,0,0, 0x08,0x01,0, 0x10,0,0, 0x20,0,0, 0x40,0,0, 0x80,0];
    for (i, b) in payload.iter().enumerate() { report[6 + i] = *b; }
    if control.write(&report).unwrap_or(0) != 64 { return false; }
    std::thread::sleep(std::time::Duration::from_millis(50));
    report[4] = 0x01;
    control.write(&report).unwrap_or(0) == 64
}

/// 旧接口兼容
pub fn set_ai_key_mode(control: &HidDevice, mode: bool) -> bool {
    if mode { ai_on(control) } else { ai_off(control) }
}

/// 打开鼠标音频接口 + 命令通道。返回连接元组; 全部失败返回 None。
/// log: 可选诊断回调。
pub fn connect_audio(
    api: &HidApi,
    wired_pids: &HashSet<u16>,
    wireless_pids: &HashSet<u16>,
    exclude_path: Option<&str>,
    log: &dyn Fn(&str),
) -> Option<Connected> {
    let candidates = priority_order(api, wired_pids, wireless_pids, exclude_path);
    if candidates.is_empty() {
        log("未找到鼠标音频 HID 接口 (usage_page=0xFFAA)。请确认鼠标已连接(无线接收器或数据线)。");
        return None;
    }
    let mut tried = Vec::new();
    for d in &candidates {
        let cmd = match arm_mouse(api, Some(d.product_id)) {
            Some(c) => c,
            None => {
                tried.push(format!("PID={:04X} ({}) 命令通道握手失败", d.product_id, d.product_string));
                continue;
            }
        };
        match open_by_path(api, &d.path) {
            Some(audio) => {
                let control = api.device_list()
                    .filter(|dd| dd.vendor_id() == 0x363C
                        && dd.product_id() == d.product_id
                        && dd.usage_page() == 0xffa0
                        && dd.usage() == 0x0002)
                    .find_map(|dd| open_by_path(api, dd.path().to_string_lossy().as_ref()));
                return Some(Connected {
                    audio,
                    cmd,
                    control,
                    path: d.path.clone(),
                    pid: d.product_id,
                    product_string: d.product_string.clone(),
                });
            }
            None => {
                tried.push(format!("PID={:04X} 音频接口打开失败", d.product_id));
            }
        }
    }
    log(&format!(
        "找到鼠标音频接口但无法建立连接。诊断: {}",
        tried.join("; ")
    ));
    None
}

/// 返回当前真实在线的音频链路 (基于激活握手确认); 都不在线返回 None。
pub fn live_link(api: &HidApi, wired_pids: &HashSet<u16>, wireless_pids: &HashSet<u16>) -> Option<AjazzDevice> {
    for d in priority_order(api, wired_pids, wireless_pids, None) {
        if arm_mouse(api, Some(d.product_id)).is_some() {
            return Some(d);
        }
    }
    None
}

/// 打印所有 HID 设备, 重点标注 AJAZZ (VID=363C) 与音频 usage_page (0xFFAA)。
/// 移植自 Python 的 list_hid(), 用于插线后确认鼠标的真实 VID/PID/音频接口。
pub fn list_hid(log: &dyn Fn(&str)) {
    let api = match HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            log(&format!("初始化 HID 失败: {}", e));
            return;
        }
    };
    log("HID 设备清单 (VID PID  接口  usage_page  厂商/产品):");
    for d in api.device_list() {
        let vid = d.vendor_id();
        let pid = d.product_id();
        let up = d.usage_page();
        let iface = d.interface_number();
        let manu = d.manufacturer_string().unwrap_or("");
        let prod = d.product_string().unwrap_or("");
        let mut mark = String::new();
        if vid == VID {
            mark.push_str("  <== AJAZZ");
            let cl = classify_link(prod, &HashSet::new(), &HashSet::new(), pid);
            if cl == "wired" {
                mark.push_str("  [有线]");
            } else if cl == "wireless" {
                mark.push_str("  [无线]");
            }
        }
        if up == AUDIO_USAGE_PAGE {
            mark.push_str("  [音频接口]");
        }
        log(&format!(
            "  {:04X} {:04X}   #{:>3}  0x{:04X}  {} {}{}",
            vid, pid, iface, up, manu, prod, mark
        ));
    }
    log("");
    log("提示: 音频接口需 usage_page=0xFFAA。若插线后看不到 AJAZZ 或没有该接口,");
    log("说明此鼠标有线的 USB 未暴露语音 HID (硬件限制), 只能无线用语音。");
}
