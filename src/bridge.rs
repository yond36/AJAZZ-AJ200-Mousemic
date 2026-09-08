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

// 轻量探测间隔: 纯本机枚举、不写固件, 故可放宽到 3s, 进一步降低 USB 枚举开销。
// 历史值 1.5s + 每次全量 ARM 握手 (8 包, 最坏 2.5s) 是光标偶发卡 1~2s 的根因。
const PROBE_INTERVAL: f64 = 3.0;
// 空闲电量刷新间隔: 这款固件只在收到命令流量后才上报电池, 平时零写入就永远
// 收不到。空闲时做一次完整重握手 (与老版本每 1.5s 一次相同动作) 即可逗出上报,
// 频率降到 5 分钟一次后, 顿感几乎不可察, 且永远避开语音期间。
const IDLE_ARM_INTERVAL: f64 = 300.0;
// 判定"空闲"的语音静默时长: 超过它才允许空闲重握手。
const IDLE_SILENCE: f64 = 60.0;
/// 每次 poll_consumer 最多排空的报告数 (防设备刷屏饿死主循环)。
const CONSUMER_DRAIN_MAX: u32 = 64;

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
    /// 前进键 AI 语音开关(独立于热键; 仅语音模式时 true, 不注入按键)。
    ai_fwd: bool,
    /// 后退键 AI 语音开关(独立于热键)。
    ai_bwd: bool,
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
    /// 本轮语音是否来过包 (用于会话结束时补发一次 AI 配置, 见 run 末尾)。
    session_open: bool,
    /// 上次空闲重握手时间 (秒, start 基准), 防止空闲刷新连击。
    last_idle_arm: f64,
    /// 连接成功后补一次电量刷新 (连接时那次握手拿不到上报, 隔 10s 再做一次)。
    need_post_connect_refresh: bool,
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
            ai_fwd: config.ai_fwd,
            ai_bwd: config.ai_bwd,
            n_pkts: 0, n_dec_ok: 0, n_dec_fail: 0, fail_streak: 0,
            last_audio: 0.0, hotkey_engaged: false, hotkey_engaged_key: None, active_key: None,
            last_probe: 0.0, audio_started: false, start: Instant::now(),
            auto_enter: config.auto_enter,
            auto_enter_mode: config.auto_enter_mode.clone(),
            auto_enter_delay: config.auto_enter_delay,
            voice_ended_at: None, auto_enter_sent: false,
            typeless_fwd: config.typeless_fwd,
            typeless_bwd: config.typeless_bwd,
            session_open: false,
            last_idle_arm: 0.0,
            need_post_connect_refresh: false,
        })
    }

    fn disconnect(&mut self) {
        if let Some(hk) = &mut self.hotkey_fwd { hk.release(); }
        if let Some(hk) = &mut self.hotkey_bwd { hk.release(); }
        self.hotkey_engaged = false;
        self.hotkey_engaged_key = None;
        self.active_key = None;
        self.session_open = false;
        self.need_post_connect_refresh = false;
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

    /// 用全新 HidApi 实例替换当前枚举。拔线后 refresh_devices 可能残留 stale 条目
    /// (表现为握手"成功"但再也收不到包), 全新枚举比 refresh 更可靠。
    /// 失败则保留旧实例并返回 false。
    fn reenumerate(&mut self, log: &dyn Fn(&str)) -> bool {
        match HidApi::new() {
            Ok(api) => { self.api = api; true }
            Err(e) => { log(&format!("HID 重新枚举失败: {}", e)); false }
        }
    }

    /// 重连入口: 先等设备就绪, 就绪后只握手一次。
    ///
    /// 插拔线后 USB 重枚举需要时间, 无线射频重入网还要再滞后几秒。在此期间
    /// 向半就绪设备发 ARM 曾把鼠标打挂到光标完全不动, 向未上线的射频发握手
    /// 则会假成功 (日志正常但永远无声)。所以: 在此函数里只做纯枚举等待
    /// (零写固件), 等最优链路连续两次 stable (间隔 1s) 且无线目标额外多等 2s
    /// 射频, 然后一次 connect。失败就退回等下个周期, 绝不连击。
    fn reconnect(&mut self, exclude_path: Option<&str>, debug: bool, log: &dyn Fn(&str)) -> bool {
        log("链路变化, 等待设备就绪 (期间不读写设备)...");
        let mut last_best: Option<String> = None;
        let mut stable_count = 0;
        for _ in 0..6 {
            self.reenumerate(log);
            let best = hid::priority_order(&self.api, &self.wired, &self.wireless, exclude_path)
                .into_iter()
                .next();
            match best {
                Some(d) if Some(d.path.clone()) == last_best => {
                    stable_count += 1;
                    if stable_count >= 2 {
                        break;
                    }
                }
                Some(d) => {
                    last_best = Some(d.path);
                    stable_count = 0;
                }
                None => {
                    last_best = None;
                    stable_count = 0;
                }
            }
            std::thread::sleep(Duration::from_millis(1000));
        }
        // 稳定后若目标是无线, 射频可能还差一截, 再多等 2s。
        if let Some(cand) = hid::best_light_link(&self.api, &self.wired, &self.wireless) {
            if hid::classify_link(&cand.product_string, &self.wired, &self.wireless, cand.product_id) == "wireless" {
                log("等待无线射频重入网 2 秒...");
                std::thread::sleep(Duration::from_millis(2000));
                self.reenumerate(log);
            }
        }
        self.connect(exclude_path, debug, log)
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
        // 刚连上不计入空闲刷新, 从此刻起 5 分钟后才允许第一次;
        // 但先安排一次启动补刷新 (10s 后, 拿首份电量)。
        self.last_idle_arm = self.start.elapsed().as_secs_f64();
        self.need_post_connect_refresh = true;

        match MsbcDecoder::new() {
            Ok(d) => { self.decoder = Some(d); true }
            Err(e) => { log(&format!("sbc 解码器初始化失败: {}", e)); false }
        }
    }

    /// 按当前配置向鼠标写入 AI 键设置 (前进/后退分别启用或禁用)。
    /// 仅 connect 后调用一次。周期探测已改为纯枚举零写入, 不再重发 ARM,
    /// 故不需要每次探测后重应用 (旧逻辑每 1.5s 写一次固件, 是光标卡死的根因)。
    ///
    /// 启用判定 = 绑定热键 **或** 独立 AI 开关(仅语音模式): 两者解耦, 允许
    /// "长按触发麦克风但不注入任何按键"。未启用的键长按恢复默认浏览器前进/后退。
    fn apply_ai_config(&mut self, debug: bool, log: &dyn Fn(&str)) {
        if let Some(ref ctrl) = self.control {
            let fwd = self.hotkey_fwd_name.is_some() || self.ai_fwd;
            let bwd = self.hotkey_bwd_name.is_some() || self.ai_bwd;
            std::thread::sleep(Duration::from_millis(50));
            hid::set_ai_keys(ctrl, fwd, bwd);
            if debug {
                let fmt = |enabled: bool, name: &Option<String>| -> String {
                    if !enabled {
                        "禁用".to_string()
                    } else if let Some(n) = name {
                        format!("启用({})", n)
                    } else {
                        "启用(仅语音)".to_string()
                    }
                };
                log(&format!("AI键: 前进={} 后退={}",
                    fmt(fwd, &self.hotkey_fwd_name),
                    fmt(bwd, &self.hotkey_bwd_name)));
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
            // 上限保护: 防止设备持续刷非 0x0C 报告时死循环饿死主循环 (音频读 200ms 超时)
            let mut drained = 0u32;
            loop {
                if drained >= CONSUMER_DRAIN_MAX {
                    break;
                }
                drained += 1;
                match consumer.read_timeout(&mut buf, 0) {
                    // 读到数据: 仅收集 0x0C 按键事件; 其他报告 ID 忽略但继续排空,
                    // 避免一条无关报告把后续排队的按键事件卡在驱动缓冲里 (漏掉按下)。
                    Ok(n) if n > 0 => {
                        if buf[0] == 0x0C {
                            events.push((buf[1], buf[2]));
                        }
                    }
                    _ => { break; } // 无数据 (Ok(0)) 或错误: 结束本轮排空
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
        let mut last_bat_dbg_log = 0.0f64;
        let mut last_cb_total: usize = 0;
        let mut buf = [0u8; 64];

        loop {
            if stop.load(Ordering::SeqCst) { break; }
            let now = self.start.elapsed().as_secs_f64();

            // ---- 读取按键事件 (非阻塞, 先于音频读取, 避免长语音期间事件堆积) ----
            self.poll_consumer();

            // ---- 周期性探测链路 (轻量: 只枚举, 零写入) ----
            // 之前这里调 hid::live_link() 做全量 ARM 握手 (8 包 × (20ms + 最长 300ms)),
            // 最坏阻塞 2.5s 且让鼠标固件忙到光标卡 1~2s。现改为纯枚举:
            // 路径还在 = 在线, 什么都不做 (不重握手、不重发 AI 配置);
            // 路径消失 / 出现更高优先级链路才重连 (重连那一次才写固件)。
            // 固件假死(枚举在但不出声)由下方音频 read 错误分支覆盖重连。
            if now - self.last_probe >= PROBE_INTERVAL {
                self.last_probe = now;
                let speech_active = now - self.last_audio < crate::HOTKEY_IDLE_TIMEOUT;
                if !speech_active {
                    if let Err(e) = self.api.refresh_devices() {
                        log(&format!("HID 枚举刷新失败: {}", e));
                    }
                    match hid::best_light_link(&self.api, &self.wired, &self.wireless) {
                        None => {
                            if self.current_path.is_some() { log("鼠标已断开, 等待重新连接..."); }
                            if !self.reconnect(None, debug, log) {
                                continue;
                            }
                            log(&format!("已重新连接 ({}模式)。", self.mode_label()));
                        }
                        Some(live) => {
                            if self.current_path.as_deref() != Some(&live.path) {
                                let old_desc = format!("PID={:04X} {}", self.current_pid, self.current_ps);
                                log(&format!("检测到链路变化, 切换到{}模式... (原:{} 新:PID={:04X} {})",
                                    hid::classify_label(hid::classify_link(&live.product_string, &self.wired, &self.wireless, live.product_id)),
                                    old_desc, live.product_id, live.product_string));
                                if self.reconnect(None, debug, log) {
                                    log(&format!("已切换至{}模式。", self.mode_label()));
                                } else {
                                    log("切换失败, 下次探测继续重试...");
                                }
                                continue;
                            }
                            // 链路未变: 刻意什么都不做。旧代码在这里重发 ARM + 重发
                            // AI 配置, 正是光标周期性卡死的来源。
                        }
                    }
                }
            }

            // ---- 启动/切换后补一次电量刷新: 连接时那次握手拿不到上报,
            // 相隔 10s (给句柄/射频留余量) 再做一次同动作; 语音中则顺延到松手后。
            // 做完即清标志, 且顺手把 last_idle_arm 续上 (下一次就是 5 分钟后)。
            if self.need_post_connect_refresh
                && self.current_path.is_some()
                && now - self.last_audio >= crate::HOTKEY_IDLE_TIMEOUT
                && now - self.last_idle_arm >= 10.0
            {
                self.need_post_connect_refresh = false;
                self.last_idle_arm = now;
                if debug { log("获取启动电量: 重握手一次..."); }
                if hid::arm_mouse(&self.api, Some(self.current_pid)).is_none() && debug {
                    log("启动电量刷新: 握手无应答, 等空闲刷新再试");
                }
                self.apply_ai_config(debug, log);
            }

            // ---- 空闲电量刷新: 静默超 60s 且距上次超 5 分钟, 做一次完整重握手 ----
            // 与老版本每 1.5s 的动作相同, 只是频率降到可忽略; 语音进行中
            // (last_audio 在超时内) 绝不执行, 刚切换链路 5 分钟内也不会执行
            // (last_idle_arm 在 connect 成功时重置)。临时句柄握手、用完即丢,
            // 已持有的 audio/cmd 句柄不受影响。
            if self.current_path.is_some()
                && now - self.last_audio >= IDLE_SILENCE
                && now - self.last_idle_arm >= IDLE_ARM_INTERVAL
            {
                self.last_idle_arm = now;
                if debug { log("空闲刷新: 重握手一次以获取电量上报..."); }
                if hid::arm_mouse(&self.api, Some(self.current_pid)).is_none() && debug {
                    log("空闲刷新: 握手无应答, 下次再试");
                }
                self.apply_ai_config(debug, log);
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
                    self.session_open = true;
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
                    let ex = self.current_path.clone();
                    if !self.reconnect(ex.as_deref(), debug, log) {
                        log("重连暂未成功, 等待下次探测/读取重试...");
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
                            } else if debug && t - last_bat_dbg_log >= 5.0 {
                                // 调试用: 包到了但解析失败 (格式/阈值对不上), 打印前 24 字节,
                                // 5 秒节流, 只读不写, 不影响鼠标。
                                last_bat_dbg_log = t;
                                let preview: Vec<String> = bbuf[..n.min(24)].iter().map(|b| format!("{:02X}", b)).collect();
                                log(&format!("电池包解析失败 (len={}): {}", n, preview.join(" ")));
                            }
                        }
                        _ => break,
                    }
                }
            }

            // ---- 会话结束: 补发一次 AI 键配置 (单包, 非调试不打日志) ----
            // 两个作用: ① 若固件不知何故丢了 AI 配置, 这里自愈;
            // ② 这款固件只在收到命令流量后才上报电池, 这一个单包大概率能把电量
            // 包"逗"出来, 且频率仅为"每次语音一次", 远够不上卡鼠标的量级。
            if self.session_open && self.start.elapsed().as_secs_f64() - self.last_audio > crate::HOTKEY_IDLE_TIMEOUT {
                self.session_open = false;
                self.apply_ai_config(debug, log);
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
