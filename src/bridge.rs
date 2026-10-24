//! 主桥接循环: 读 Consumer Control(0x0C) 区分按键 + HID 音频包 -> 解码 mSBC -> 输出 PCM。
//!
//! 双键联动: 前进键/后退键独立绑定不同热键。热键为 "无" 则该键 AI 功能禁用。

use crate::config::Config;
use crate::hid;
use crate::hotkey::HotKey;
use crate::msbc::MsbcDecoder;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
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
const LINK_RETRY_DELAY: f64 = 30.0;

#[derive(Default)]
struct LinkRetry {
    failed_until: HashMap<String, f64>,
    pending: Option<(String, f64)>,
}

impl LinkRetry {
    fn retain_present(&mut self, candidates: &[hid::AjazzDevice]) {
        self.failed_until
            .retain(|path, _| candidates.iter().any(|d| &d.path == path));
    }

    fn eligible(&self, path: &str, now: f64) -> bool {
        self.failed_until
            .get(path)
            .is_none_or(|until| now >= *until)
    }

    fn failed(&mut self, path: &str, now: f64) {
        self.failed_until
            .insert(path.to_string(), now + LINK_RETRY_DELAY);
        self.pending = None;
    }

    fn ready(&mut self, path: Option<&str>, now: f64, settle: f64) -> bool {
        let Some(path) = path else {
            self.pending = None;
            return false;
        };
        match &self.pending {
            Some((pending, since)) if pending == path => now - since >= settle,
            _ => {
                self.pending = Some((path.to_string(), now));
                false
            }
        }
    }
}

fn link_rank(device: &hid::AjazzDevice, wired: &HashSet<u16>, wireless: &HashSet<u16>) -> u8 {
    match hid::classify_link(&device.product_string, wired, wireless, device.product_id) {
        "wired" => 0,
        "wireless" => 1,
        _ => 2,
    }
}

fn preferred_link<'a>(
    candidates: &'a [hid::AjazzDevice],
    current_path: Option<&str>,
    retry: &LinkRetry,
    now: f64,
    wired: &HashSet<u16>,
    wireless: &HashSet<u16>,
) -> Option<&'a hid::AjazzDevice> {
    let best = candidates.iter().find(|d| retry.eligible(&d.path, now))?;
    if let Some(current) = candidates
        .iter()
        .find(|d| Some(d.path.as_str()) == current_path)
    {
        if link_rank(current, wired, wireless) <= link_rank(best, wired, wireless) {
            return Some(current);
        }
    }
    Some(best)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeyAction {
    Press(u8),
    Release(u8),
    Tap(u8),
}

#[derive(Default)]
struct HotkeySession {
    engaged: Option<(u8, bool)>,
}

impl HotkeySession {
    fn start(&mut self, binding: Option<(u8, bool)>) -> Vec<HotkeyAction> {
        if self.engaged == binding {
            return Vec::new();
        }
        // 松键事件在固件 ~0.8s 拖尾期间就会到达, 此时音频仍在流入;
        // 若立即收尾会提前释放热键、截断目标软件正在录的语音。
        // 按住会话只在"换键接管"时切换, 统一由音频静默超时 (finish) 收尾。
        let Some((key, typeless)) = binding else {
            return Vec::new();
        };
        let mut actions: Vec<_> = self.finish().into_iter().collect();
        self.engaged = binding;
        actions.push(if typeless {
            HotkeyAction::Tap(key)
        } else {
            HotkeyAction::Press(key)
        });
        actions
    }

    fn finish(&mut self) -> Option<HotkeyAction> {
        self.engaged.take().map(|(key, typeless)| {
            if typeless {
                HotkeyAction::Tap(key)
            } else {
                HotkeyAction::Release(key)
            }
        })
    }
}

pub struct Bridge {
    api: HidApi,
    wired: HashSet<u16>,
    wireless: HashSet<u16>,
    audio: Option<HidDevice>,
    /// 命令通道句柄 (连接时握手成功、保持打开整个连接期)。
    /// 语音包/空闲刷新都复用它, 不再每次开关临时句柄——临时句柄用完即关,
    /// 反复开关会给固件额外压力 (且 useless: 关掉的句柄没有任何作用)。
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
    hotkey_session: HotkeySession,
    active_key: Option<u8>, // 0x08=forward, 0x04=backward
    last_probe: f64,
    link_retry: LinkRetry,
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

        let mk_hotkey = |name: &Option<String>,
                         driver: &str,
                         label: &str,
                         log: &dyn Fn(&str)|
         -> (Option<HotKey>, Option<String>) {
            match name.as_deref() {
                Some(n) if n != "无" => match HotKey::new(n, driver) {
                    Ok(h) => (Some(h), Some(n.to_string())),
                    Err(e) => {
                        log(&format!("{}热键初始化失败: {}", label, e));
                        (None, None)
                    }
                },
                _ => (None, None),
            }
        };

        let (hotkey_fwd, hotkey_fwd_name) =
            mk_hotkey(&config.hotkey_forward, &config.driver, "前进", log);
        let (hotkey_bwd, hotkey_bwd_name) =
            mk_hotkey(&config.hotkey_backward, &config.driver, "后退", log);

        Ok(Bridge {
            api,
            wired,
            wireless,
            audio: None,
            cmd: None,
            control: None,
            consumer: None,
            battery: None,
            battery_pct: None,
            battery_charging: false,
            current_path: None,
            current_pid: 0,
            current_ps: String::new(),
            decoder: None,
            hotkey_fwd,
            hotkey_fwd_name,
            hotkey_bwd,
            hotkey_bwd_name,
            ai_fwd: config.ai_fwd,
            ai_bwd: config.ai_bwd,
            n_pkts: 0,
            n_dec_ok: 0,
            n_dec_fail: 0,
            fail_streak: 0,
            last_audio: 0.0,
            hotkey_session: HotkeySession::default(),
            active_key: None,
            last_probe: 0.0,
            link_retry: LinkRetry::default(),
            audio_started: false,
            start: Instant::now(),
            auto_enter: config.auto_enter,
            auto_enter_mode: config.auto_enter_mode.clone(),
            auto_enter_delay: config.auto_enter_delay,
            voice_ended_at: None,
            auto_enter_sent: false,
            typeless_fwd: config.typeless_fwd,
            typeless_bwd: config.typeless_bwd,
            session_open: false,
            last_idle_arm: 0.0,
            need_post_connect_refresh: false,
        })
    }

    fn disconnect(&mut self) {
        self.finish_hotkey_session();
        if let Some(hk) = &mut self.hotkey_fwd {
            hk.release();
        }
        if let Some(hk) = &mut self.hotkey_bwd {
            hk.release();
        }
        self.active_key = None;
        self.session_open = false;
        self.voice_ended_at = None;
        self.auto_enter_sent = true;
        self.last_audio = 0.0;
        self.fail_streak = 0;
        self.need_post_connect_refresh = false;
        // 仅在确有关联设备 (已知链路) 时才发禁用命令: current_path 为空说明本就
        // 未连接或已清空, 此时写固件是对 stale/无效句柄的无效操作, 跳过。
        if self.current_path.is_some() {
            if let Some(ctrl) = self.control.as_ref().or(self.cmd.as_ref()) {
                // 发送禁用 AI 键命令, 恢复前进/后退默认行为
                hid::ai_off(ctrl);
            }
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
            Ok(api) => {
                self.api = api;
                true
            }
            Err(e) => {
                log(&format!("HID 重新枚举失败: {}", e));
                false
            }
        }
    }

    fn connect_candidate(
        &mut self,
        candidate: &hid::AjazzDevice,
        stop: &AtomicBool,
        debug: bool,
        log: &dyn Fn(&str),
    ) -> bool {
        let decoder = match MsbcDecoder::new() {
            Ok(decoder) => decoder,
            Err(e) => {
                log(&format!("sbc 解码器初始化失败: {}", e));
                return false;
            }
        };
        let connected = hid::connect_candidate(&self.api, candidate, stop, debug, log);
        let Some(c) = connected else {
            if !stop.load(Ordering::SeqCst) {
                self.link_retry
                    .failed(&candidate.path, self.start.elapsed().as_secs_f64());
            }
            return false;
        };

        // 新链路完整建立后才丢弃旧链路, 失败候选不能打断仍可用的连接。
        self.disconnect();
        self.audio = Some(c.audio);
        self.cmd = Some(c.cmd);
        self.control = c.control;
        self.consumer = c.consumer;
        self.battery = c.battery;
        self.current_path = Some(c.path);
        self.current_pid = c.pid;
        self.current_ps = c.product_string;
        self.decoder = Some(decoder);
        self.link_retry.failed_until.remove(&candidate.path);
        self.link_retry.pending = None;
        if stop.load(Ordering::SeqCst) {
            self.disconnect();
            return false;
        }
        self.apply_ai_config(debug, log);
        self.last_idle_arm = self.start.elapsed().as_secs_f64();
        self.need_post_connect_refresh = true;
        true
    }

    fn connect_initial(&mut self, stop: &AtomicBool, debug: bool, log: &dyn Fn(&str)) -> bool {
        for candidate in hid::priority_order(&self.api, &self.wired, &self.wireless, None) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if self.connect_candidate(&candidate, stop, debug, log) {
                return true;
            }
        }
        false
    }

    fn poll_link(&mut self, stop: &AtomicBool, debug: bool, log: &dyn Fn(&str)) {
        let now = self.start.elapsed().as_secs_f64();
        if now - self.last_probe < PROBE_INTERVAL || stop.load(Ordering::SeqCst) {
            return;
        }
        self.last_probe = now;
        if self.session_open && now - self.last_audio < crate::HOTKEY_IDLE_TIMEOUT {
            return;
        }
        if !self.reenumerate(log) {
            return;
        }
        let candidates = hid::priority_order(&self.api, &self.wired, &self.wireless, None);
        self.link_retry.retain_present(&candidates);
        if self
            .current_path
            .as_ref()
            .is_some_and(|path| !candidates.iter().any(|d| &d.path == path))
        {
            log("鼠标已断开, 等待重新连接...");
            self.disconnect();
        }
        let target = preferred_link(
            &candidates,
            self.current_path.as_deref(),
            &self.link_retry,
            now,
            &self.wired,
            &self.wireless,
        );
        let Some(target) = target else {
            self.link_retry.ready(None, now, 0.0);
            return;
        };
        if self.current_path.as_deref() == Some(target.path.as_str()) {
            self.link_retry.pending = None;
            return;
        }
        // 枚举稳定期间继续服务旧连接, 不阻塞音频和停止请求。
        let settle = if link_rank(target, &self.wired, &self.wireless) == 1 {
            4.0
        } else {
            3.0
        };
        if !self.link_retry.ready(Some(&target.path), now, settle) {
            return;
        }
        if self.connect_candidate(target, stop, debug, log) {
            log(&format!("已连接 ({}模式)。", self.mode_label()));
        } else if !stop.load(Ordering::SeqCst) {
            log("候选链路连接失败, 保留当前连接并稍后重试。");
        }
        self.last_probe = self.start.elapsed().as_secs_f64();
    }

    /// 重握手命令通道 (获取电池上报用)。
    /// 优先复用连接时建立、保持打开的 `cmd` 句柄; 它已在 connect 时握手成功,
    /// 再次 arm_existing 即可。仅当 `cmd` 缺失/失效时才重新 open (走 arm_mouse)。
    /// 这样空闲/启动刷新只写固件、不反复开关临时句柄 (开关会给固件额外压力)。
    /// 返回是否成功 (失败仅用于日志, 不影响主流程)。
    fn rearm_command_channel(
        &mut self,
        stop: &AtomicBool,
        debug: bool,
        log: &dyn Fn(&str),
        ctx: &str,
    ) -> bool {
        if let Some(ref cmd) = self.cmd {
            if hid::arm_existing_until(cmd, stop) {
                return true;
            }
            if debug {
                log(&format!("{}: 常驻命令通道无应答, 尝试重开", ctx));
            }
        }
        let Some(audio) = self.audio.as_ref() else {
            return false;
        };
        match hid::arm_for_audio(&self.api, audio, self.current_pid, stop) {
            Some(dev) => {
                self.cmd = Some(dev);
                true
            }
            None => {
                if debug {
                    log(&format!("{}: 握手无应答, 稍后重试", ctx));
                }
                false
            }
        }
    }

    /// 按当前配置向鼠标写入 AI 键设置 (前进/后退分别启用或禁用)。
    /// 仅 connect 后调用一次。周期探测已改为纯枚举零写入, 不再重发 ARM,
    /// 故不需要每次探测后重应用 (旧逻辑每 1.5s 写一次固件, 是光标卡死的根因)。
    ///
    /// 启用判定 = 绑定热键 **或** 独立 AI 开关(仅语音模式): 两者解耦, 允许
    /// "长按触发麦克风但不注入任何按键"。未启用的键长按恢复默认浏览器前进/后退。
    fn apply_ai_config(&mut self, debug: bool, log: &dyn Fn(&str)) {
        if let Some(ctrl) = self.control.as_ref().or(self.cmd.as_ref()) {
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
                log(&format!(
                    "AI键: 前进={} 后退={}",
                    fmt(fwd, &self.hotkey_fwd_name),
                    fmt(bwd, &self.hotkey_bwd_name)
                ));
            }
        } else if debug {
            log("未找到 control 接口 (usage_page=0xFFA0), AI键配置无法生效");
        }
    }

    fn mode_label(&self) -> &'static str {
        hid::classify_label(hid::classify_link(
            &self.current_ps,
            &self.wired,
            &self.wireless,
            self.current_pid,
        ))
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
                        if n >= 3 && buf[0] == 0x0C {
                            events.push((buf[1], buf[2]));
                        }
                    }
                    _ => {
                        break;
                    } // 无数据 (Ok(0)) 或错误: 结束本轮排空
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

    fn apply_hotkey_action(&mut self, action: HotkeyAction) {
        let key = match action {
            HotkeyAction::Press(key) | HotkeyAction::Release(key) | HotkeyAction::Tap(key) => key,
        };
        let hotkey = match key {
            0x08 => &mut self.hotkey_fwd,
            0x04 => &mut self.hotkey_bwd,
            _ => return,
        };
        if let Some(hotkey) = hotkey {
            match action {
                HotkeyAction::Press(_) => hotkey.press(),
                HotkeyAction::Release(_) => hotkey.release(),
                HotkeyAction::Tap(_) => hotkey.tap(),
            }
        }
    }

    fn finish_hotkey_session(&mut self) -> bool {
        if let Some(action) = self.hotkey_session.finish() {
            self.apply_hotkey_action(action);
            true
        } else {
            false
        }
    }

    fn start_hotkey_session(&mut self, debug: bool, log: &dyn Fn(&str)) {
        let Some(key) = self.active_key else {
            return;
        };
        let has_hotkey = match key {
            0x08 => self.hotkey_fwd.is_some(),
            0x04 => self.hotkey_bwd.is_some(),
            _ => false,
        };
        let binding = has_hotkey.then_some((key, self.is_typeless(key)));
        for action in self.hotkey_session.start(binding) {
            if debug {
                log(&format!("热键会话: {:?}", action));
            }
            self.apply_hotkey_action(action);
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
        if !self.connect_initial(stop, debug, log) {
            self.disconnect();
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            return Err(anyhow::anyhow!(
                "未找到可用的鼠标音频 HID 接口。请确认鼠标已连接。"
            ));
        }
        log(&format!(
            "已连接 ({}模式), 等待语音键...",
            self.mode_label()
        ));

        let mut last_count_log = 0.0f64;
        let mut last_bat_dbg_log = 0.0f64;
        let mut last_cb_total: usize = 0;
        let mut buf = [0u8; 64];

        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let now = self.start.elapsed().as_secs_f64();

            // ---- 读取按键事件 (非阻塞, 先于音频读取, 避免长语音期间事件堆积) ----
            self.poll_consumer();

            // 周期探测只枚举; 候选稳定且退避结束后才尝试切换。
            self.poll_link(stop, debug, log);
            if stop.load(Ordering::SeqCst) {
                break;
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
                if debug {
                    log("获取启动电量: 重握手一次...");
                }
                self.rearm_command_channel(stop, debug, log, "启动电量刷新");
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
                if debug {
                    log("空闲刷新: 重握手一次以获取电量上报...");
                }
                self.rearm_command_channel(stop, debug, log, "空闲刷新");
                self.apply_ai_config(debug, log);
            }

            if stop.load(Ordering::SeqCst) {
                break;
            }
            // ---- 读取音频包 (优先) ----
            let read_res = match &mut self.audio {
                Some(d) => d.read_timeout(&mut buf, 200),
                None => {
                    if !hid::wait_or_stop(stop, Duration::from_millis(100)) {
                        break;
                    }
                    continue;
                }
            };

            match read_res {
                Ok(n)
                    if n == 64
                        && buf[0] == crate::REPORT_ID
                        && buf[2] == crate::AUDIO_PAYLOAD_LEN =>
                {
                    if self.decoder.is_none() {
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
                    self.session_open = true;
                    self.auto_enter_sent = false;
                    self.voice_ended_at = None;

                    // 不同按键接管语音时先结束旧绑定; 松手后的拖尾仍沿用当前会话。
                    self.start_hotkey_session(debug, log);
                }
                Ok(_) => {}
                Err(_) => {
                    log("鼠标连接中断, 已结束热键会话, 等待设备就绪...");
                    if let Some(path) = self.current_path.clone() {
                        self.link_retry
                            .failed(&path, self.start.elapsed().as_secs_f64());
                    }
                    self.disconnect();
                    self.last_probe = self.start.elapsed().as_secs_f64() - PROBE_INTERVAL;
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
                        "语音包={} 解码OK={} 失败={} 队列≈{} 回调+{}次/秒 最近音频{:.1}s前",
                        self.n_pkts,
                        self.n_dec_ok,
                        self.n_dec_fail,
                        q,
                        cb_delta,
                        t - self.last_audio
                    ));
                }
            }

            // ---- 轮询电池状态上报 (0xFFA0 usage=0x0002, 被动上报) ----
            if let Some(ref bat) = self.battery {
                let mut bbuf = [0u8; 64];
                for _ in 0..CONSUMER_DRAIN_MAX {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match bat.read_timeout(&mut bbuf, 0) {
                        Ok(n) if n > 0 => {
                            if let Some((pct, charging)) = hid::parse_battery(&bbuf[..n]) {
                                let changed = self.battery_pct != Some(pct)
                                    || self.battery_charging != charging;
                                self.battery_pct = Some(pct);
                                self.battery_charging = charging;
                                if changed {
                                    battery_cb(pct, charging);
                                }
                            } else if debug && t - last_bat_dbg_log >= 5.0 {
                                // 调试用: 包到了但解析失败 (格式/阈值对不上), 打印前 24 字节,
                                // 5 秒节流, 只读不写, 不影响鼠标。
                                last_bat_dbg_log = t;
                                let preview: Vec<String> = bbuf[..n.min(24)]
                                    .iter()
                                    .map(|b| format!("{:02X}", b))
                                    .collect();
                                log(&format!(
                                    "电池包解析失败 (len={}): {}",
                                    n,
                                    preview.join(" ")
                                ));
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
            if self.session_open
                && self.start.elapsed().as_secs_f64() - self.last_audio > crate::HOTKEY_IDLE_TIMEOUT
            {
                self.session_open = false;
                self.apply_ai_config(debug, log);
            }

            // ---- 热键松开 (音频停止后) ----
            if self.start.elapsed().as_secs_f64() - self.last_audio > crate::HOTKEY_IDLE_TIMEOUT
                && self.finish_hotkey_session()
            {
                if debug {
                    log("热键会话已结束");
                }
                if self.auto_enter && self.voice_ended_at.is_none() {
                    self.voice_ended_at = Some(Instant::now());
                    if debug {
                        log(&format!(
                            "语音结束, {}秒后自动按 {}",
                            self.auto_enter_delay, self.auto_enter_mode
                        ));
                    }
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
                        if debug {
                            log(&format!("自动回车: {}", self.auto_enter_mode));
                        }
                    }
                }
            }
        }

        // 退出时恢复 AI 键为默认 (禁用)
        self.disconnect();

        if self.audio_started {
            log(&format!("已处理 {} 个语音包。", self.n_pkts));
        }
        log(if stop.load(Ordering::SeqCst) {
            "桥接已正常停止。"
        } else {
            "桥接已退出。"
        });
        Ok(())
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(path: &str, product: &str) -> hid::AjazzDevice {
        hid::AjazzDevice {
            path: path.to_string(),
            product_id: 1,
            product_string: product.to_string(),
        }
    }

    #[test]
    fn typeless_finish_sends_one_paired_toggle() {
        let mut session = HotkeySession::default();
        assert_eq!(
            session.start(Some((0x08, true))),
            vec![HotkeyAction::Tap(0x08)]
        );
        assert!(session.start(Some((0x08, true))).is_empty());
        assert_eq!(session.finish(), Some(HotkeyAction::Tap(0x08)));
        assert_eq!(session.finish(), None);
    }

    #[test]
    fn held_key_is_released_on_finish() {
        let mut session = HotkeySession::default();
        assert_eq!(
            session.start(Some((0x08, false))),
            vec![HotkeyAction::Press(0x08)]
        );
        assert_eq!(session.finish(), Some(HotkeyAction::Release(0x08)));
        assert_eq!(session.finish(), None);
    }

    #[test]
    fn switching_keys_finishes_old_binding_first() {
        let mut session = HotkeySession::default();
        session.start(Some((0x08, false)));
        assert_eq!(
            session.start(Some((0x04, true))),
            vec![HotkeyAction::Release(0x08), HotkeyAction::Tap(0x04)]
        );
        assert_eq!(session.finish(), Some(HotkeyAction::Tap(0x04)));
    }

    #[test]
    fn key_release_during_firmware_tail_keeps_hotkey_held() {
        // 松键事件先于音频拖尾结束到达: 保持按住, 由静默超时统一释放。
        let mut session = HotkeySession::default();
        session.start(Some((0x08, false)));
        assert!(session.start(None).is_empty(), "拖尾期间不得提前释放");
        assert_eq!(session.finish(), Some(HotkeyAction::Release(0x08)));
        assert_eq!(session.finish(), None);
    }

    #[test]
    fn switching_to_voice_only_releases_old_binding() {
        let mut session = HotkeySession::default();
        session.start(Some((0x08, true)));
        assert_eq!(
            session.start(Some((0x04, false))),
            vec![HotkeyAction::Tap(0x08), HotkeyAction::Press(0x04)]
        );
        assert_eq!(session.finish(), Some(HotkeyAction::Release(0x04)));
    }

    #[test]
    fn failed_wired_candidate_does_not_displace_wireless() {
        let candidates = vec![device("wired", "Mouse"), device("wireless", "2.4G")];
        let empty = HashSet::new();
        let mut retry = LinkRetry::default();
        retry.failed("wired", 10.0);
        for now in [10.0, 13.0, 20.0, 39.0] {
            assert_eq!(
                preferred_link(&candidates, Some("wireless"), &retry, now, &empty, &empty)
                    .unwrap()
                    .path,
                "wireless"
            );
        }
        assert_eq!(
            preferred_link(&candidates, Some("wireless"), &retry, 40.0, &empty, &empty)
                .unwrap()
                .path,
            "wired"
        );
    }

    #[test]
    fn equal_priority_does_not_switch_devices() {
        let candidates = vec![device("a", "Mouse"), device("b", "Mouse")];
        let empty = HashSet::new();
        assert_eq!(
            preferred_link(
                &candidates,
                Some("b"),
                &LinkRetry::default(),
                0.0,
                &empty,
                &empty
            )
            .unwrap()
            .path,
            "b"
        );
    }

    #[test]
    fn disappeared_device_loses_failure_backoff() {
        let mut retry = LinkRetry::default();
        retry.failed("wired", 0.0);
        retry.retain_present(&[]);
        assert!(retry.eligible("wired", 1.0));
    }

    #[test]
    fn link_must_remain_stable_before_switching() {
        let mut retry = LinkRetry::default();
        assert!(!retry.ready(Some("wired"), 0.0, 3.0));
        assert!(!retry.ready(Some("wired"), 2.0, 3.0));
        assert!(retry.ready(Some("wired"), 3.0, 3.0));
        assert!(!retry.ready(None, 4.0, 3.0));
        assert!(!retry.ready(Some("wired"), 5.0, 3.0));
        assert!(!retry.ready(Some("wireless"), 8.0, 4.0));
        assert!(retry.ready(Some("wireless"), 12.0, 4.0));
    }
}
