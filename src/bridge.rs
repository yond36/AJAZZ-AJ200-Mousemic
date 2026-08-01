//! 主桥接循环: 读 Consumer Control(0x0C) 区分按键 + HID 音频包 -> 解码 mSBC -> 输出 PCM。
//!
//! 双键联动: 前进键/后退键独立绑定不同热键。热键为 "无" 则该键 AI 功能禁用。

use crate::config::Config;
use crate::hid;
use crate::hotkey::HotKey;
use crate::msbc::MsbcDecoder;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROBE_INTERVAL: f64 = 1.5;

pub struct Bridge {
    api: HidApi,
    wired: HashSet<u16>,
    wireless: HashSet<u16>,
    audio: Option<HidDevice>,
    cmd: Option<HidDevice>,
    control: Option<HidDevice>,
    consumer: Option<HidDevice>,
    battery: Option<HidDevice>,
    battery_pct: Option<u8>,
    battery_charging: bool,
    current_path: Option<String>,
    current_pid: u16,
    current_ps: String,
    decoder: Option<MsbcDecoder>,
    hotkey_fwd: Option<HotKey>,
    hotkey_fwd_name: Option<String>,
    hotkey_bwd: Option<HotKey>,
    hotkey_bwd_name: Option<String>,
    n_pkts: u64,
    n_dec_ok: u64,
    n_dec_fail: u64,
    fail_streak: u32,
    last_audio: f64,
    hotkey_engaged: bool,
    hotkey_engaged_key: Option<u8>,   // 当前按下的热键是哪个 (0x08/0x04)
    active_key: Option<u8>,   // 0x08=forward, 0x04=backward
    last_probe: f64,
    audio_started: bool,
    start: Instant,
    auto_enter: bool,
    auto_enter_mode: String,
    auto_enter_delay: f64,
    voice_ended_at: Option<Instant>,
    auto_enter_sent: bool,
    typeless_fwd: bool,
    typeless_bwd: bool,
}

impl Bridge {
    pub fn new(config: &Config, log: &dyn Fn(&str)) -> anyhow::Result<Self> {
        let api = HidApi::new().map_err(|e| anyhow::anyhow!("初始化 HID 失败: {}", e))?;
        let wired = config.wired_pids();
        let wireless = config.wireless_pids();

        let mk_hotkey = |name: &Option<String>, driver: &str, label: &str, log: &dyn Fn(&str)|
            -> (Option<HotKey>, Option<String>)
        {
            match name.as_deref() {
                Some(n) if n != "无" => match HotKey::new(n, driver) {
                    Ok(h) => (Some(h), Some(n.to_string())),
                    Err(e) => { log(&format!("{}热键初始化失败: {}", label, e)); (None, None) }
                },
                _ => (None, None),
            }
        };

        let (hotkey_fwd, hotkey_fwd_name) = mk_hotkey(&config.hotkey_forward, &config.driver, "前进", log);
        let (hotkey_bwd, hotkey_bwd_name) = mk_hotkey(&config.hotkey_backward, &config.driver, "后退", log);

        Ok(Bridge {
            api, wired, wireless,
            audio: None, cmd: None, control: None, consumer: None, battery: None,
            battery_pct: None, battery_charging: false,
            current_path: None, current_pid: 0, current_ps: String::new(),
            decoder: None,
            hotkey_fwd, hotkey_fwd_name,
            hotkey_bwd, hotkey_bwd_name,
            n_pkts: 0, n_dec_ok: 0, n_dec_fail: 0, fail_streak: 0,
            last_audio: 0.0, hotkey_engaged: false, hotkey_engaged_key: None, active_key: None,
            last_probe: 0.0, audio_started: false, start: Instant::now(),
            auto_enter: config.auto_enter,
            auto_enter_mode: config.auto_enter_mode.clone(),
            auto_enter_delay: config.auto_enter_delay,
            voice_ended_at: None, auto_enter_sent: false,
            typeless_fwd: config.typeless_fwd,
            typeless_bwd: config.typeless_bwd,
        })
    }

    fn disconnect(&mut self) {
        if let Some(hk) = &mut self.hotkey_fwd { hk.release(); }
        if let Some(hk) = &mut self.hotkey_bwd { hk.release(); }
        self.hotkey_engaged = false;
        self.hotkey_engaged_key = None;
        self.active_key = None;
        if let Some(ref ctrl) = self.control {
            // 发送禁用 AI 键命令, 恢复前进/后退默认行为
            std::thread::sleep(Duration::from_millis(20));
            hid::ai_off(ctrl);
        }
        self.decoder = None;
        drop(self.audio.take());
        drop(self.cmd.take());
        drop(self.control.take());
        drop(self.consumer.take());
        drop(self.battery.take());
        self.battery_pct = None;
        self.battery_charging = false;
        self.current_path = None;
        self.current_pid = 0;
        self.current_ps.clear();
    }

    fn connect(&mut self, exclude_path: Option<&str>, debug: bool, log: &dyn Fn(&str)) -> bool {
        self.disconnect();
        let connected = hid::connect_audio(&self.api, &self.wired, &self.wireless, exclude_path, log);
        let Some(c) = connected else { return false; };

        self.audio = Some(c.audio);
        self.cmd = Some(c.cmd);
        self.control = c.control;
        self.consumer = c.consumer;
        self.battery = c.battery;
        self.current_path = Some(c.path);
        self.current_pid = c.pid;
        self.current_ps = c.product_string;

        // 按配置决定哪些键启用 AI
        self.apply_ai_config(debug, log);

        match MsbcDecoder::new() {
            Ok(d) => { self.decoder = Some(d); true }
            Err(e) => { log(&format!("sbc 解码器初始化失败: {}", e)); false }
        }
    }

    /// 按当前配置向鼠标写入 AI 键设置 (前进/后退分别启用或禁用)。
    /// connect 后与链路探测后都要调用: 探测会重发 ARM 序列, 其最后一个包就是
    /// AI 键配置报告, 会把鼠标按键行为重置回默认 (双键全开), 覆盖用户在 GUI 里
    /// 设置的"某键=无"配置, 故每次探测成功后必须重新应用一次。
    fn apply_ai_config(&mut self, debug: bool, log: &dyn Fn(&str)) {
        if let Some(ref ctrl) = self.control {
            let fwd = self.hotkey_fwd_name.is_some();
            let bwd = self.hotkey_bwd_name.is_some();
            std::thread::sleep(Duration::from_millis(50));
            hid::set_ai_keys(ctrl, fwd, bwd);
            if debug {
                let fh = self.hotkey_fwd_name.as_deref().unwrap_or("无");
                let bh = self.hotkey_bwd_name.as_deref().unwrap_or("无");
                log(&format!("AI键: 前进={}({}) 后退={}({})",
                    if fwd { "启用" } else { "禁用" }, fh,
                    if bwd { "启用" } else { "禁用" }, bh));
            }
        } else if debug {
            log("未找到 control 接口 (usage_page=0xFFA0), AI键配置无法生效");
        }
    }

    fn mode_label(&self) -> &'static str {
        hid::classify_label(hid::classify_link(&self.current_ps, &self.wired, &self.wireless, self.current_pid))
    }

    /// 判断指定键是否启用 typeless 模式。
    fn is_typeless(&self, key: u8) -> bool {
        match key {
            0x08 => self.typeless_fwd,
            0x04 => self.typeless_bwd,
            _ => false,
        }
    }

    /// 从 Consumer Control 接口读 0x0C 按键事件，设置 active_key。
    /// 每个主循环迭代开始时调用一次 (非阻塞, timeout=0), 保证长语音期间
    /// 按键按下事件不会被音频包饿死; 释放动作统一由"音频停止后"超时路径处理。
    fn poll_consumer(&mut self) {
        // 先收集事件, 释放 consumer 借用后再处理 (避免 borrow conflict)
        let mut events: Vec<(u8, u8)> = Vec::new();
        if let Some(ref mut consumer) = self.consumer {
            let mut buf = [0u8; 64];
            loop {
                match consumer.read_timeout(&mut buf, 0) {
                    Ok(n) if n > 0 && buf[0] == 0x0C => {
                        events.push((buf[1], buf[2]));
                    }
                    _ => { break; }
                }
            }
        }
        for (key, state) in events {
            if state == 0xEE && (key == 0x08 || key == 0x04) {
                self.active_key = Some(key);
            } else if state == 0x00 {
                self.active_key = None;
                // 释放热键统一交给"热键松开 (音频停止后)"超时路径: 按键释放事件
                // 会在音频拖尾期间到达, 若在此立即释放会截断目标软件正在录的语音。
            }
        }
    }

    fn release_active_hotkey(&mut self, engaged_key: u8) {
        match engaged_key {
            0x08 => { if let Some(ref mut hk) = self.hotkey_fwd { hk.release(); } }
            0x04 => { if let Some(ref mut hk) = self.hotkey_bwd { hk.release(); } }
            _ => {}
        }
    }

    fn tap_hotkey_by_key(&mut self, key: u8) {
        match key {
            0x08 => { if let Some(ref mut hk) = self.hotkey_fwd { hk.tap(); } }
            0x04 => { if let Some(ref mut hk) = self.hotkey_bwd { hk.tap(); } }
            _ => {}
        }
    }

    pub fn run(
        &mut self,
        sink: &mut dyn FnMut(&[i16]),
        diag: &dyn Fn() -> (usize, usize),
        stop: &Arc<AtomicBool>,
        debug: bool,
        log: &dyn Fn(&str),
        battery_cb: &dyn Fn(u8, bool),
    ) -> anyhow::Result<()> {
        if !self.connect(None, debug, log) {
            return Err(anyhow::anyhow!("未找到鼠标音频 HID 接口。请确认鼠标已连接。"));
        }
        log(&format!("已连接 ({}模式), 等待语音键...", self.mode_label()));

        let mut last_count_log = 0.0f64;
        let mut last_cb_total: usize = 0;
        let mut buf = [0u8; 64];

        loop {
            if stop.load(Ordering::SeqCst) { break; }
            let now = self.start.elapsed().as_secs_f64();

            // ---- 读取按键事件 (非阻塞, 先于音频读取, 避免长语音期间事件堆积) ----
            self.poll_consumer();

            // ---- 周期性探测链路 ----
            if now - self.last_probe >= PROBE_INTERVAL {
                self.last_probe = now;
                // 语音进行中跳过重握手: 音频流本身证明链路在线, 且 ARM 序列
                // (8 个包 × 20ms 延时) 会打断语音; 中途断开会走下方音频读错误分支。
                let speech_active = now - self.last_audio < crate::HOTKEY_IDLE_TIMEOUT;
                if !speech_active {
                    if let Err(e) = self.api.refresh_devices() {
                        log(&format!("HID 枚举刷新失败: {}", e));
                    }
                    match hid::live_link(&self.api, &self.wired, &self.wireless) {
                        None => {
                            if self.current_path.is_some() { log("鼠标已断开, 等待重新连接..."); }
                            if !self.connect(None, debug, log) {
                                std::thread::sleep(Duration::from_millis(500));
                                continue;
                            }
                            log(&format!("已重新连接 ({}模式)。", self.mode_label()));
                        }
                        Some(live) => {
                            if self.current_path.as_deref() != Some(&live.path) {
                                log(&format!("检测到链路变化, 切换到{}模式...", hid::classify_label(hid::classify_link(&live.product_string, &self.wired, &self.wireless, live.product_id))));
                                if self.connect(None, debug, log) {
                                    log(&format!("已切换至{}模式。", self.mode_label()));
                                } else {
                                    log("切换失败, 继续重试...");
                                    std::thread::sleep(Duration::from_millis(300));
                                }
                                continue;
                            }
                            // 链路未变, 但探测刚重发过 ARM 序列 (含 AI 键配置包),
                            // 鼠标按键行为已被重置为默认, 这里按配置重新应用一次。
                            self.apply_ai_config(debug, log);
                        }
                    }
                }
            }

            // ---- 读取音频包 (优先) ----
            let read_res = match &mut self.audio {
                Some(d) => d.read_timeout(&mut buf, 200),
                None => { std::thread::sleep(Duration::from_millis(200)); continue; }
            };

            match read_res {
                Ok(n) if n == 64 && buf[0] == crate::REPORT_ID && buf[2] == crate::AUDIO_PAYLOAD_LEN => {
                    if self.decoder.is_none() {
                        match MsbcDecoder::new() {
                            Ok(d) => { self.decoder = Some(d); log("检测到语音输入, 已启动音频解码。"); self.audio_started = true; }
                            Err(e) => log(&format!("解码器启动失败: {}", e)),
                        }
                    }
                    if let Some(dec) = &mut self.decoder {
                        match dec.decode_frame(&buf[3..60]) {
                            Some(pcm) => {
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
                                if self.fail_streak >= 2 {
                                    self.fail_streak = 0;
                                    match MsbcDecoder::new() {
                                        Ok(d) => { self.decoder = Some(d); log("解码器重建以恢复 mSBC 帧同步"); }
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

                    // 语音来了, 触发对应热键
                    if !self.hotkey_engaged && self.active_key.is_some() {
                        let key = self.active_key.unwrap();
                        let has_hotkey = match key {
                            0x08 => self.hotkey_fwd_name.is_some(),
                            0x04 => self.hotkey_bwd_name.is_some(),
                            _ => false,
                        };
                        if has_hotkey {
                            self.hotkey_engaged = true;
                            self.hotkey_engaged_key = Some(key);
                            if self.is_typeless(key) {
                                // typeless: 短按热键 (开始录音信号)
                                if debug {
                                    let key_label = if key == 0x08 { "前进" } else { "后退" };
                                    log(&format!("typeless: 短按热键开始 ({}键)", key_label));
                                }
                                self.tap_hotkey_by_key(key);
                            } else {
                                // 按住模式
                                let name = match key {
                                    0x08 => self.hotkey_fwd_name.as_deref().unwrap_or(""),
                                    0x04 => self.hotkey_bwd_name.as_deref().unwrap_or(""),
                                    _ => "",
                                };
                                if debug {
                                    let key_label = if key == 0x08 { "前进" } else { "后退" };
                                    log(&format!("联动热键已激活: 按住 {} ({}键)", name, key_label));
                                }
                                match key {
                                    0x08 => { if let Some(ref mut hk) = self.hotkey_fwd { hk.press(); } }
                                    0x04 => { if let Some(ref mut hk) = self.hotkey_bwd { hk.press(); } }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    log("鼠标连接中断, 尝试重新连接...");
                    let _ = self.api.refresh_devices();
                    let ex = self.current_path.clone();
                    if !self.connect(ex.as_deref(), debug, log) {
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
                    log(&format!("语音包={} 解码OK={} 失败={} 队列≈{} 回调+{}次/秒 最近音频{:.1}s前",
                        self.n_pkts, self.n_dec_ok, self.n_dec_fail, q, cb_delta, t - self.last_audio));
                }
            }

            // ---- 轮询电池状态上报 (0xFFA0 usage=0x0002, 被动上报) ----
            if let Some(ref bat) = self.battery {
                let mut bbuf = [0u8; 64];
                loop {
                    match bat.read_timeout(&mut bbuf, 0) {
                        Ok(n) if n > 0 => {
                            if let Some((pct, charging)) = hid::parse_battery(&bbuf[..n]) {
                                let changed = self.battery_pct != Some(pct) || self.battery_charging != charging;
                                self.battery_pct = Some(pct);
                                self.battery_charging = charging;
                                if changed {
                                    battery_cb(pct, charging);
                                }
                            }
                        }
                        _ => break,
                    }
                }
            }

            // ---- 热键松开 (音频停止后) ----
            if self.hotkey_engaged && self.start.elapsed().as_secs_f64() - self.last_audio > crate::HOTKEY_IDLE_TIMEOUT {
                if let Some(key) = self.hotkey_engaged_key {
                    if self.is_typeless(key) {
                        // typeless: 音频结束后短按热键 (结束录音信号)
                        if debug { log("typeless: 短按热键结束"); }
                        self.tap_hotkey_by_key(key);
                    } else {
                        self.release_active_hotkey(key);
                    }
                }
                self.hotkey_engaged = false;
                self.hotkey_engaged_key = None;
                if self.auto_enter && self.voice_ended_at.is_none() {
                    self.voice_ended_at = Some(Instant::now());
                    if debug { log(&format!("语音结束, {}秒后自动按 {}", self.auto_enter_delay, self.auto_enter_mode)); }
                }
            }

            // ---- 自动回车 ----
            if self.auto_enter && !self.auto_enter_sent {
                if let Some(end) = self.voice_ended_at {
                    if end.elapsed().as_secs_f64() >= self.auto_enter_delay {
                        self.auto_enter_sent = true;
                        match self.auto_enter_mode.as_str() {
                            "ctrl_enter" => crate::hotkey::inject_ctrl_enter(),
                            _ => crate::hotkey::inject_enter(),
                        }
                        if debug { log(&format!("自动回车: {}", self.auto_enter_mode)); }
                    }
                }
            }
        }

        // 退出时恢复 AI 键为默认 (禁用)
        self.disconnect();

        if self.audio_started { log(&format!("已处理 {} 个语音包。", self.n_pkts)); }
        log(if stop.load(Ordering::SeqCst) { "桥接已正常停止。" } else { "桥接已退出。" });
        Ok(())
    }
}
