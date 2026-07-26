//! 主桥接循环: 读 HID 音频包 -> 解码 mSBC -> 输出 PCM (经 sink) -> 热键注入 -> 断线重连/链路切换。
//!
//! 忠实移植自 Python 的 `run_bridge()`。关键策略:
//! - 按 product_string 自动分类有线/无线 (无线带 "2.4G"、有线带 "Mouse", 与 PID 无关),
//!   已知鼠标保持"有线优先"; 同系列换 PID 也能自动识别。
//! - 用"激活握手是否成功"判断链路真实在线, 而非仅凭 HID 枚举 (Windows 拔线后条目会残留)。
//! - 每 ~1.5s 探测一次: 插线 -> 切有线; 拔线 -> 切回无线; 全断开 -> 等重连 (不退出)。
//! - 切换/断开后**重建 mSBC 解码器**: 解码器有状态, 旧设备的半截帧会卡坏状态、迟迟不同步,
//!   表现为"能切换但不出声"; 重建即复刻 Python 手动停止再启动时的行为。
//!
//! 注意: `Bridge` 持有 `HidDevice`, 必须在**同一个线程内**创建并使用 (不要跨线程移动)。
//! GUI 应在工作线程里构造并 `run`, 而不是把 Bridge 传过线程边界。

use crate::config::Config;
use crate::hid;
use crate::hotkey::HotKey;
use crate::msbc::MsbcDecoder;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 探测在线链路的间隔 (秒)。
const PROBE_INTERVAL: f64 = 1.5;

pub struct Bridge {
    api: HidApi,
    wired: HashSet<u16>,
    wireless: HashSet<u16>,
    audio: Option<HidDevice>,
    cmd: Option<HidDevice>,
    control: Option<HidDevice>,
    current_path: Option<String>,
    current_pid: u16,
    current_ps: String,
    decoder: Option<MsbcDecoder>,
    hotkey: Option<HotKey>,
    ai_key_a: bool,         // true=键A, false=键B
    n_pkts: u64,
    n_dec_ok: u64,
    n_dec_fail: u64,
    fail_streak: u32,
    last_audio: f64,
    hotkey_engaged: bool,
    last_probe: f64,
    audio_started: bool,
    start: Instant,
    hotkey_name: Option<String>,
    auto_enter: bool,
    auto_enter_mode: String,
    auto_enter_delay: f64,
    voice_ended_at: Option<Instant>,
    auto_enter_sent: bool,
}

impl Bridge {
    /// 构造桥接器 (在目标工作线程内调用)。会初始化 HID 并预建热键。
    pub fn new(config: &Config, log: &dyn Fn(&str)) -> anyhow::Result<Self> {
        let api = HidApi::new().map_err(|e| anyhow::anyhow!("初始化 HID 失败: {}", e))?;
        let wired = config.wired_pids();
        let wireless = config.wireless_pids();

        let mut hotkey = None;
        let mut hotkey_name = None;
        if let Some(name) = &config.hotkey {
            match HotKey::new(name, &config.driver) {
                Ok(h) => { hotkey_name = Some(name.clone()); hotkey = Some(h); }
                Err(e) => log(&format!("热键初始化失败 (将不联动): {}", e)),
            }
        }

        Ok(Bridge {
            api, wired, wireless,
            audio: None, cmd: None, control: None,
            current_path: None, current_pid: 0, current_ps: String::new(),
            decoder: None,
            hotkey, hotkey_name,
            ai_key_a: config.ai_key == "a",
            n_pkts: 0, n_dec_ok: 0, n_dec_fail: 0, fail_streak: 0,
            last_audio: 0.0, hotkey_engaged: false, last_probe: 0.0,
            audio_started: false, start: Instant::now(),
            auto_enter: config.auto_enter,
            auto_enter_mode: config.auto_enter_mode.clone(),
            auto_enter_delay: config.auto_enter_delay,
            voice_ended_at: None, auto_enter_sent: false,
        })
    }

    /// 断开当前连接, 释放解码器/设备/热键握手。重连前必调用。
    fn disconnect(&mut self) {
        if let Some(hk) = &mut self.hotkey { hk.release(); }
        self.hotkey_engaged = false;
        self.decoder = None;
        if let Some(d) = self.audio.take() { drop(d); }
        if let Some(c) = self.cmd.take() { drop(c); }
        if let Some(ctrl) = self.control.take() { drop(ctrl); }
        self.current_path = None;
        self.current_pid = 0;
        self.current_ps.clear();
    }

    /// 关闭旧连接并重建: 找到在线音频链路 -> 握手 -> 打开 -> 重建解码器。
    /// 成功返回 true (新句柄已写入 self), 失败返回 false。
    fn connect(&mut self, exclude_path: Option<&str>, log: &dyn Fn(&str)) -> bool {
        self.disconnect();
        let connected = hid::connect_audio(
            &self.api,
            &self.wired,
            &self.wireless,
            exclude_path,
            log,
        );
        let Some(c) = connected else {
            return false;
        };
        self.audio = Some(c.audio);
        self.cmd = Some(c.cmd);
        self.control = c.control;
        self.current_path = Some(c.path);
        self.current_pid = c.pid;
        self.current_ps = c.product_string;

        // 设置 AI 语音键
        if let Some(ref ctrl) = self.control {
            hid::set_ai_key_mode(ctrl, self.ai_key_a);
        }

        match MsbcDecoder::new() {
            Ok(d) => {
                self.decoder = Some(d);
                true
            }
            Err(e) => {
                log(&format!("sbc 解码器初始化失败: {}", e));
                false
            }
        }
    }

    /// 当前链路的有线/无线标签 ("有线"/"无线"/"未知")。
    fn mode_label(&self) -> &'static str {
        let link = hid::classify_link(&self.current_ps, &self.wired, &self.wireless, self.current_pid);
        hid::classify_label(link)
    }

    /// 主循环。`sink` 接收解码出的 16k mono i16 PCM 切片 (由调用方决定写到声卡还是文件)。
    /// `stop` 置位后干净退出。
    pub fn run(
        &mut self,
        sink: &mut dyn FnMut(&[i16]),
        diag: &dyn Fn() -> (usize, usize),
        stop: &Arc<AtomicBool>,
        debug: bool,
        log: &dyn Fn(&str),
    ) -> anyhow::Result<()> {
        if !self.connect(None, log) {
            return Err(anyhow::anyhow!(
                "未找到鼠标音频 HID 接口 (usage_page=0xFFAA)。请确认鼠标已连接(无线接收器或数据线)。"
            ));
        }
        log(&format!(
            "已连接鼠标音频接口 ({}模式), 等待语音键...",
            self.mode_label()
        ));

        let mut last_count_log = 0.0f64;
        let mut last_cb_total: usize = 0;
        let mut buf = [0u8; 64];
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let now = self.start.elapsed().as_secs_f64();

            // ---- 周期性探测链路 (有线优先, 残留设备自动跳过) ----
            // 关键: hidapi Rust crate 的 device_list() 返回的是内部缓存快照, 必须
            // 先 refresh_devices() 才能看到热插拔后的新设备。Python 版每次
            // hid.enumerate() 都是新枚举, 故无此问题。少了这步 refresh, 插线后
            // live_link 永远看不到新链路 -> 热插拔自动切换完全失效。
            if now - self.last_probe >= PROBE_INTERVAL {
                self.last_probe = now;
                // 刷新 HID 枚举快照 (拔插设备后才能探测到)。失败仅记日志, 不阻断。
                if let Err(e) = self.api.refresh_devices() {
                    log(&format!("HID 枚举刷新失败: {}", e));
                }
                match hid::live_link(&self.api, &self.wired, &self.wireless) {
                    None => {
                        if self.current_path.is_some() {
                            log("鼠标已断开, 等待重新连接...");
                        }
                        if !self.connect(None, log) {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                        log(&format!("已重新连接 ({}模式)。", self.mode_label()));
                    }
                    Some(live) => {
                        if self.current_path.as_deref() != Some(&live.path) {
                            let link = hid::classify_link(
                                &live.product_string,
                                &self.wired,
                                &self.wireless,
                                live.product_id,
                            );
                            log(&format!(
                                "检测到链路变化, 切换到{}模式...",
                                hid::classify_label(link)
                            ));
                            if self.connect(None, log) {
                                log(&format!("已切换至{}模式。", self.mode_label()));
                            } else {
                                log("切换失败, 继续重试...");
                                std::thread::sleep(Duration::from_millis(300));
                            }
                            continue;
                        }
                    }
                }
            }

            // ---- 读取音频包 (200ms 超时, 自带节奏, 不会忙等) ----
            let read_res = match &mut self.audio {
                Some(d) => d.read_timeout(&mut buf, 200),
                None => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };

            match read_res {
                Ok(n) if n == 64
                    && buf[0] == crate::REPORT_ID
                    && buf[2] == crate::AUDIO_PAYLOAD_LEN =>
                {
                    if self.decoder.is_none() {
                        // 兜底: connect 已建解码器, 极端情况下若为空则重建
                        match MsbcDecoder::new() {
                            Ok(d) => {
                                self.decoder = Some(d);
                                log("检测到语音输入, 已启动音频解码。");
                                self.audio_started = true;
                            }
                            Err(e) => log(&format!("解码器启动失败: {}", e)),
                        }
                    }
                    if let Some(dec) = &mut self.decoder {
                        match dec.decode_frame(&buf[3..60]) {
                            Some(pcm) => {
                                // s16le 字节 -> i16 切片 (每帧 240 字节 = 120 样本)
                                let mut samples = Vec::with_capacity(pcm.len() / 2);
                                let mut i = 0;
                                while i + 1 < pcm.len() {
                                    samples.push(i16::from_le_bytes([pcm[i], pcm[i + 1]]));
                                    i += 2;
                                }
                                sink(&samples);
                                self.n_dec_ok += 1;
                                self.fail_streak = 0;
                            }
                            None => {
                                self.n_dec_fail += 1;
                                self.fail_streak += 1;
                                // 连续解码失败: 重建 mSBC 解码器以恢复帧同步。
                                // sbc 为有状态解码器 (含分析滤波器历史), 偶发失步后需重 init 才能继续产出。
                                if self.fail_streak >= 2 {
                                    self.fail_streak = 0;
                                    match MsbcDecoder::new() {
                                        Ok(d) => {
                                            self.decoder = Some(d);
                                            log("解码器重建以恢复 mSBC 帧同步");
                                        }
                                        Err(e) => log(&format!("解码器重建失败: {}", e)),
                                    }
                                }
                            }
                        }
                    }
                    self.n_pkts += 1;
                    self.last_audio = self.start.elapsed().as_secs_f64();
                    self.auto_enter_sent = false;
                    self.voice_ended_at = None;
                    if let Some(hk) = &mut self.hotkey {
                        if !self.hotkey_engaged {
                            self.hotkey_engaged = true;
                            if let Some(name) = &self.hotkey_name {
                                log(&format!("联动热键已激活: 按住 {}", name));
                            }
                        }
                        hk.press();
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    log("鼠标连接中断, 尝试重新连接...");
                    let _ = self.api.refresh_devices();
                    let ex = self.current_path.clone();
                    if !self.connect(ex.as_deref(), log) {
                        std::thread::sleep(Duration::from_millis(500));
                    } else {
                        log(&format!("已重新连接 ({}模式)。", self.mode_label()));
                    }
                    continue;
                }
            }

            // ---- 周期性汇报 ----
            let t = self.start.elapsed().as_secs_f64();
            if t - last_count_log >= 1.0 {
                last_count_log = t;
                if debug && self.n_pkts > 0 {
                    let (q, cb_total) = diag();
                    let cb_delta = cb_total.saturating_sub(last_cb_total);
                    last_cb_total = cb_total;
                    log(&format!(
                        "语音包={} 解码OK={} 解码失败={} 队列≈{} 回调+{}次/秒 累计{}次 最近音频{:.1}s前",
                        self.n_pkts, self.n_dec_ok, self.n_dec_fail, q, cb_delta, cb_total, t - self.last_audio
                    ));
                }
            }

            // ---- 热键松开判定 ----
            if self.hotkey_engaged
                && self.start.elapsed().as_secs_f64() - self.last_audio > crate::HOTKEY_IDLE_TIMEOUT
            {
                if let Some(hk) = &mut self.hotkey { hk.release(); }
                self.hotkey_engaged = false;
                if self.auto_enter && self.voice_ended_at.is_none() {
                    self.voice_ended_at = Some(Instant::now());
                    if debug {
                        log(&format!("语音结束, {}秒后自动按 {}", self.auto_enter_delay, self.auto_enter_mode));
                    }
                }
            }

            // ---- 自动回车: 语音结束后延迟按 Enter / Ctrl+Enter ----
            if self.auto_enter && !self.auto_enter_sent {
                if let Some(end) = self.voice_ended_at {
                    if end.elapsed().as_secs_f64() >= self.auto_enter_delay {
                        self.auto_enter_sent = true;
                        match self.auto_enter_mode.as_str() {
                            "ctrl_enter" => crate::hotkey::inject_ctrl_enter(),
                            _ => crate::hotkey::inject_enter(),
                        }
                        if debug {
                            log(&format!("自动回车: {}", self.auto_enter_mode));
                        }
                    }
                }
            }
        }

        if self.audio_started {
            log(&format!("已处理 {} 个语音包。", self.n_pkts));
        }
        if stop.load(Ordering::SeqCst) {
            log("桥接已正常停止。");
        } else {
            log("桥接已退出。");
        }
        Ok(())
    }
}
