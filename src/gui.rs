//! 图形界面: native-windows-gui (Win32 原生控件) 面板 + 系统托盘。
//!
//! 移植自 mousemic_gui.py: 启动/停止桥接、联动热键与注入方式、输出模式 (扬声器/虚拟麦克风)、
//! 开机自启 (注册表)、最小化到托盘、依赖检查、日志框。
//!
//! 用 Win32 原生控件 (GDI 渲染, 系统字体), 不像 egui 那样创建 OpenGL 上下文、不把
//! 字体文件读进内存, 运行时内存占用从 ~96MB 降到 ~20MB 量级 (与 Python tkinter 相当)。
//! 受 `gui` feature 控制。
//!
//! 架构: 桥接在独立线程运行 (Bridge 持有 HidDevice, 不跨线程)。GUI 主线程用 mpsc 通道
//! 接收日志/状态, 用 Win32 计时器 (~100ms) 轮询通道刷新 UI。托盘菜单命令也经通道交给
//! 主线程。事件回调是 `&self`, 运行态字段用 `RefCell<AppState>` 提供内部可变性。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use native_windows_derive as nwd;
use native_windows_gui as nwg;
use nwd::NwgUi;
use nwg::NativeUi;

use crate::audio::AudioOutput;
use crate::bridge::Bridge;
use crate::config::{self, Config};
use crate::dialog::show_error_box;
use crate::{hid, single_instance, SAMPLE_RATE};
use cpal::traits::{DeviceTrait, HostTrait};

/// 热键下拉框里的"仅语音"特殊项: 启用该键 AI(长按触发麦克风)但不注入任何按键。
const VOICE_ONLY: &str = "仅语音(无热键)";
const DISABLED_KEY: &str = "禁用语音";

/// 把热键下拉选中项拆成 (联动热键名, 仅语音开关):
/// - "无" → 不联动且禁用 AI;
/// - VOICE_ONLY → 不联动但启用 AI(仅语音);
/// - 热键名 → 联动(绑定热键隐式启用 AI, ai 置 false 由 Config 判定兜底)。
fn split_hotkey_sel(sel: &str) -> (Option<String>, bool) {
    match sel {
        "无" | DISABLED_KEY => (None, false),
        VOICE_ONLY => (None, true),
        other => (Some(other.to_string()), false),
    }
}

/// 自动发送输入始终转为有限的 0..=10 秒; 无效输入沿用上次有效值。
fn parse_auto_enter_delay(text: &str, previous: f64) -> f64 {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|d| d.is_finite())
        .or_else(|| previous.is_finite().then_some(previous))
        .unwrap_or(0.5)
        .clamp(0.0, 10.0)
}

/// 从控件采集的值; 配置转换不依赖窗口, 便于无 GUI 回归测试。
struct ConfigSelections {
    cable_device: String,
    hotkey_fwd: String,
    hotkey_bwd: String,
    driver: String,
    auto_enter: bool,
    auto_enter_mode: Option<String>,
    auto_enter_delay: String,
}

/// 后台线程 → GUI 主线程的通道消息。运行状态/电量只对所属代次有效。
enum Msg {
    Log(String),
    Running {
        generation: u64,
        running: bool,
    },
    Battery {
        generation: u64,
        pct: u8,
        charging: bool,
    },
}

struct RunningUpdate {
    was_running: bool,
    finished_thread: Option<JoinHandle<()>>,
}

/// 运行态 (与 UI 控件分离, 用 RefCell 在 `&self` 回调里修改)。
struct AppState {
    // 配置镜像 (UI 即时态, 改了就写回 JSON)
    mode_play: bool,
    cable_device: String,
    hotkey_fwd: String,
    hotkey_bwd: String,
    driver: String,
    autostart_on: bool,
    minimize_to_tray: bool,
    auto_start_service: bool,
    debug_log: bool,
    wired_pids: Vec<String>,
    wireless_pids: Vec<String>,

    // 运行态: 主动停止/自然结束即作废 active_generation, 丢弃迟到消息。
    running: bool,
    run_generation: u64,
    active_generation: Option<u64>,
    stop: Option<Arc<AtomicBool>>,
    bridge_thread: Option<JoinHandle<()>>,

    // 日志 (内存里保留最近 N 行, 渲染到 log_box)
    log_lines: Vec<String>,

    // 后台 → 主线程通道
    msg_rx: Option<mpsc::Receiver<Msg>>,
    msg_tx: Option<mpsc::Sender<Msg>>, // clone 给后台线程,

    // 下拉框数据 (填充后只读)
    hotkey_items: Vec<String>,
    driver_items: Vec<String>,

    // 自动回车
    auto_enter: bool,
    auto_enter_mode: String,
    auto_enter_delay: f64,

    // typeless 模式 (前进/后退独立)
    typeless_fwd: bool,
    typeless_bwd: bool,

    // 启动后首次收进托盘
    pending_hide: bool,

    // 是否由 --autostart (注册表开机自启) 启动: 该入口应直接起桥接
    autostart_launch: bool,

    // 电池提醒状态 (防重复提醒)
    battery_low_notified: bool,
    battery_full_notified: bool,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            mode_play: true,
            cable_device: "CABLE Input".to_string(),
            hotkey_fwd: "无".to_string(),
            hotkey_bwd: "无".to_string(),
            driver: "sendinput".to_string(),
            autostart_on: false,
            minimize_to_tray: false,
            auto_start_service: false,
            debug_log: false,
            wired_pids: Vec::new(),
            wireless_pids: Vec::new(),
            running: false,
            run_generation: 0,
            active_generation: None,
            stop: None,
            bridge_thread: None,
            log_lines: Vec::new(),
            msg_rx: None,
            msg_tx: None,
            hotkey_items: Vec::new(),
            driver_items: Vec::new(),
            auto_enter: false,
            auto_enter_mode: "enter".to_string(),
            auto_enter_delay: 0.5,
            typeless_fwd: false,
            typeless_bwd: false,
            pending_hide: false,
            autostart_launch: false,
            battery_low_notified: false,
            battery_full_notified: false,
        }
    }
}

impl AppState {
    fn load_config(&mut self, cfg: &Config) {
        self.mode_play = cfg.mode == "play";
        self.cable_device = cfg.cable_device.clone();
        self.hotkey_fwd = match (&cfg.hotkey_forward, cfg.ai_fwd) {
            (Some(h), _) => h.clone(),
            (None, true) => VOICE_ONLY.to_string(),
            (None, false) => DISABLED_KEY.to_string(),
        };
        self.hotkey_bwd = match (&cfg.hotkey_backward, cfg.ai_bwd) {
            (Some(h), _) => h.clone(),
            (None, true) => VOICE_ONLY.to_string(),
            (None, false) => DISABLED_KEY.to_string(),
        };
        self.driver = cfg.driver.clone();
        self.autostart_on = cfg.autostart;
        self.minimize_to_tray = cfg.minimize_to_tray;
        self.auto_start_service = cfg.auto_start_service;
        self.auto_enter = cfg.auto_enter;
        self.auto_enter_mode = cfg.auto_enter_mode.clone();
        self.auto_enter_delay = parse_auto_enter_delay("", cfg.auto_enter_delay);
        self.typeless_fwd = cfg.typeless_fwd;
        self.typeless_bwd = cfg.typeless_bwd;
        self.wired_pids = cfg.wired_pids.clone();
        self.wireless_pids = cfg.wireless_pids.clone();
    }

    fn config_from_selections(&self, selections: ConfigSelections) -> Config {
        let (hotkey_forward, ai_fwd) = split_hotkey_sel(&selections.hotkey_fwd);
        let (hotkey_backward, ai_bwd) = split_hotkey_sel(&selections.hotkey_bwd);
        Config {
            mode: if self.mode_play {
                "play".to_string()
            } else {
                "cable".to_string()
            },
            cable_device: selections.cable_device,
            hotkey_forward,
            hotkey_backward,
            ai_fwd,
            ai_bwd,
            driver: selections.driver,
            minimize_to_tray: self.minimize_to_tray,
            auto_start_service: self.auto_start_service,
            wired_pids: self.wired_pids.clone(),
            wireless_pids: self.wireless_pids.clone(),
            autostart: self.autostart_on,
            auto_enter: selections.auto_enter,
            auto_enter_mode: if selections.auto_enter_mode.as_deref() == Some("Ctrl+Enter") {
                "ctrl_enter".to_string()
            } else {
                "enter".to_string()
            },
            auto_enter_delay: parse_auto_enter_delay(
                &selections.auto_enter_delay,
                self.auto_enter_delay,
            ),
            typeless_fwd: self.typeless_fwd,
            typeless_bwd: self.typeless_bwd,
        }
    }

    fn begin_run(&mut self) -> Option<(u64, Arc<AtomicBool>)> {
        if self.running || self.active_generation.is_some() || self.bridge_thread.is_some() {
            return None;
        }
        self.run_generation = self.run_generation.checked_add(1)?;
        let generation = self.run_generation;
        let stop = Arc::new(AtomicBool::new(false));
        self.active_generation = Some(generation);
        self.running = true;
        self.stop = Some(stop.clone());
        Some((generation, stop))
    }

    fn apply_running(&mut self, generation: u64, running: bool) -> Option<RunningUpdate> {
        if self.active_generation != Some(generation) {
            return None;
        }
        let was_running = self.running;
        self.running = running;
        let finished_thread = if running {
            None
        } else {
            self.active_generation = None;
            self.stop = None;
            self.bridge_thread.take()
        };
        Some(RunningUpdate {
            was_running,
            finished_thread,
        })
    }

    fn request_stop(&mut self) -> Option<JoinHandle<()>> {
        // 先作废消息, 再通知线程停止; join 由调用方在释放状态借用后执行。
        self.active_generation = None;
        self.running = false;
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        self.bridge_thread.take()
    }
}

#[derive(Default, NwgUi)]
pub struct GuiApp {
    // 运行态 (无 nwg 属性; 由 RefCell 包裹, 在 &self 回调里 borrow_mut)
    state: RefCell<AppState>,
    ready: Cell<bool>,
    log_dirty: Cell<bool>,

    // 主题先于 HWND 释放, 以便在窗口销毁前解绑原生消息处理器。
    theme: RefCell<Option<crate::gui_theme::GuiTheme>>,

    #[nwg_resource(family: "Microsoft YaHei UI", size_absolute: 24, weight: 700)]
    title_font: nwg::Font,
    #[nwg_resource(family: "Microsoft YaHei UI", size_absolute: 16, weight: 600)]
    section_font: nwg::Font,
    #[nwg_resource(family: "Microsoft YaHei UI", size_absolute: 13)]
    hint_font: nwg::Font,

    // ---- 顶层窗口 ----
    #[nwg_control(size: (920, 680), center: true, title: "AJAZZ 语音鼠标桥接器", flags: "WINDOW|VISIBLE|MINIMIZE_BOX")]
    #[nwg_events( OnWindowClose: [GuiApp::on_close] )]
    window: nwg::Window,

    #[nwg_control(parent: window, text: "MouseMic", position: (20, 14), size: (620, 34), font: Some(&data.title_font))]
    heading: nwg::Label,
    #[nwg_control(parent: window, text: "AJAZZ 语音鼠标桥接器  /  让鼠标成为你的麦克风", position: (20, 52), size: (630, 22), font: Some(&data.hint_font))]
    lbl_subtitle: nwg::Label,
    #[nwg_control(parent: window, text: "鼠标电量  --", position: (712, 30), size: (188, 24))]
    lbl_battery: nwg::Label,

    // ---- 状态与主操作 ----
    #[nwg_control(parent: window, size: (880, 68), position: (20, 88), flags: "VISIBLE")]
    action_frame: nwg::Frame,
    #[nwg_control(parent: action_frame, text: "已停止", position: (16, 10), size: (480, 24), font: Some(&data.section_font))]
    lbl_status: nwg::Label,
    #[nwg_control(parent: action_frame, text: "选择输出模式，然后启动桥接。", position: (16, 38), size: (500, 20), font: Some(&data.hint_font))]
    lbl_run_hint: nwg::Label,
    #[nwg_control(parent: action_frame, text: "启动桥接", position: (530, 16), size: (120, 36))]
    #[nwg_events( OnButtonClick: [GuiApp::start_bridge] )]
    btn_start: nwg::Button,
    #[nwg_control(parent: action_frame, text: "停止", position: (662, 16), size: (82, 36))]
    #[nwg_events( OnButtonClick: [GuiApp::stop_bridge] )]
    btn_stop: nwg::Button,
    #[nwg_control(parent: action_frame, text: "收起到托盘", position: (756, 16), size: (108, 36))]
    #[nwg_events( OnButtonClick: [GuiApp::hide_to_tray] )]
    btn_hide: nwg::Button,

    // ---- 音频与按键设置 ----
    #[nwg_control(parent: window, size: (550, 332), position: (20, 172), flags: "VISIBLE")]
    settings_frame: nwg::Frame,
    #[nwg_control(parent: settings_frame, text: "音频输出", position: (16, 12), size: (240, 24), font: Some(&data.section_font))]
    lbl_audio_heading: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "输出模式", position: (16, 46), size: (72, 22))]
    lbl_mode: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "扬声器试听", position: (94, 44), size: (144, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_mode_play] )]
    rb_play: nwg::RadioButton,
    #[nwg_control(parent: settings_frame, text: "虚拟麦克风", position: (264, 44), size: (144, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_mode_cable] )]
    rb_cable: nwg::RadioButton,
    #[nwg_control(parent: settings_frame, text: "", position: (16, 76), size: (518, 36), font: Some(&data.hint_font))]
    lbl_mode_hint: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "输出设备", position: (16, 118), size: (72, 22))]
    lbl_dev: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (440, 28), position: (94, 114))]
    #[nwg_events( OnComboxBoxSelection: [GuiApp::on_persist_setting] )]
    cb_cable: nwg::ComboBox<String>,

    #[nwg_control(parent: settings_frame, text: "语音与按键", position: (16, 156), size: (240, 24), font: Some(&data.section_font))]
    lbl_keys_heading: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "前进键", position: (16, 192), size: (60, 22))]
    lbl_hotkey_fwd: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (164, 28), position: (78, 188))]
    #[nwg_events( OnComboxBoxSelection: [GuiApp::on_persist_setting] )]
    cb_hotkey_fwd: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "后退键", position: (280, 192), size: (60, 22))]
    lbl_hotkey_bwd: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (194, 28), position: (340, 188))]
    #[nwg_events( OnComboxBoxSelection: [GuiApp::on_persist_setting] )]
    cb_hotkey_bwd: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "注入方式", position: (16, 230), size: (62, 22))]
    lbl_driver: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (164, 28), position: (78, 226))]
    #[nwg_events( OnComboxBoxSelection: [GuiApp::on_persist_setting] )]
    cb_driver: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "Typeless", position: (280, 230), size: (78, 22))]
    lbl_typeless: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "前进", position: (364, 228), size: (78, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_typeless_fwd: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, text: "后退", position: (450, 228), size: (78, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_typeless_bwd: nwg::CheckBox,

    #[nwg_control(parent: settings_frame, text: "自动发送", position: (16, 266), size: (96, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_auto_enter_toggle] )]
    chk_auto_enter: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, size: (126, 28), position: (118, 264))]
    #[nwg_events( OnComboxBoxSelection: [GuiApp::on_persist_setting] )]
    cb_auto_enter_mode: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "延迟", position: (280, 268), size: (40, 22))]
    lbl_auto_delay: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "0.5", position: (324, 264), size: (64, 26))]
    #[nwg_events( OnTextInput: [GuiApp::on_delay_change] )]
    txt_auto_delay: nwg::TextInput,
    #[nwg_control(parent: settings_frame, text: "秒（0–10）", position: (398, 268), size: (130, 22), font: Some(&data.hint_font))]
    lbl_auto_unit: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "仅语音：长按录音，不触发热键；禁用：关闭该键语音。", position: (16, 306), size: (518, 20), font: Some(&data.hint_font))]
    lbl_hotkey_hint: nwg::Label,

    // ---- 环境自检 ----
    #[nwg_control(parent: window, size: (314, 220), position: (586, 172), flags: "VISIBLE")]
    deps_frame: nwg::Frame,
    #[nwg_control(parent: deps_frame, text: "环境自检", position: (16, 12), size: (282, 24), font: Some(&data.section_font))]
    lbl_deps_heading: nwg::Label,
    #[nwg_control(parent: deps_frame, position: (16, 46), size: (282, 80), flags: "VISIBLE|AUTOVSCROLL|VSCROLL|TAB_STOP", readonly: true)]
    deps_list: nwg::TextBox,
    #[nwg_control(parent: deps_frame, text: "重新检查", position: (16, 138), size: (134, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::refresh_deps] )]
    btn_check_deps: nwg::Button,
    #[nwg_control(parent: deps_frame, text: "刷新音频设备", position: (164, 138), size: (134, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::list_devices_btn] )]
    btn_list_dev: nwg::Button,
    #[nwg_control(parent: deps_frame, text: "诊断 HID", position: (16, 178), size: (134, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::diag_hid] )]
    btn_diag_hid: nwg::Button,
    #[nwg_control(parent: deps_frame, text: "依赖下载说明", position: (164, 178), size: (134, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::install_deps] )]
    btn_install_deps: nwg::Button,

    #[nwg_control(parent: window, size: (314, 96), position: (586, 408), flags: "VISIBLE")]
    preferences_frame: nwg::Frame,
    #[nwg_control(parent: preferences_frame, text: "启动与后台", position: (16, 8), size: (282, 24), font: Some(&data.section_font))]
    lbl_preferences_heading: nwg::Label,
    #[nwg_control(parent: preferences_frame, text: "开机自启", position: (16, 36), size: (134, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_autostart_change] )]
    chk_autostart: nwg::CheckBox,
    #[nwg_control(parent: preferences_frame, text: "启动收起到托盘", position: (164, 36), size: (134, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_minimize: nwg::CheckBox,
    #[nwg_control(parent: preferences_frame, text: "启动自动桥接", position: (16, 64), size: (134, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_autostart_svc: nwg::CheckBox,
    #[nwg_control(parent: preferences_frame, text: "调试日志", position: (164, 64), size: (134, 24))]
    #[nwg_events( OnButtonClick: [GuiApp::on_debug_change] )]
    chk_debug: nwg::CheckBox,

    // ---- 日志 ----
    #[nwg_control(parent: window, size: (880, 140), position: (20, 520), flags: "VISIBLE")]
    log_frame: nwg::Frame,
    #[nwg_control(parent: log_frame, text: "运行日志", position: (16, 10), size: (132, 24), font: Some(&data.section_font))]
    lbl_log_heading: nwg::Label,
    #[nwg_control(parent: log_frame, text: "自动滚动", position: (666, 8), size: (108, 24), check_state: nwg::CheckBoxState::Checked)]
    #[nwg_events( OnButtonClick: [GuiApp::on_log_follow] )]
    chk_log_follow: nwg::CheckBox,
    #[nwg_control(parent: log_frame, text: "清空", position: (790, 6), size: (74, 28))]
    #[nwg_events( OnButtonClick: [GuiApp::clear_log] )]
    btn_clear_log: nwg::Button,
    #[nwg_control(parent: log_frame, position: (16, 42), size: (848, 86), flags: "VISIBLE|AUTOVSCROLL|VSCROLL|TAB_STOP", readonly: true, limit: 262144)]
    log_box: nwg::TextBox,

    // ---- 系统托盘 ----
    #[nwg_resource]
    resources: nwg::EmbedResource,
    #[nwg_resource(source_embed: Some(&data.resources), source_embed_id: 1, size: Some((32, 32)))]
    icon: nwg::Icon,
    #[nwg_resource(source_embed: Some(&data.resources), source_embed_id: 1, size: Some((16, 16)))]
    small_icon: nwg::Icon,
    #[nwg_control(parent: window, icon: Some(&data.small_icon), tip: Some("MouseMic · AJAZZ 语音鼠标"))]
    #[nwg_events( MousePressLeftUp: [GuiApp::tray_show_win], OnContextMenu: [GuiApp::tray_show_menu] )]
    tray: nwg::TrayNotification,
    #[nwg_control(parent: window, popup: true)]
    tray_menu: nwg::Menu,
    #[nwg_control(parent: tray_menu, text: "显示主窗口")]
    #[nwg_events( OnMenuItemSelected: [GuiApp::tray_show_win] )]
    m_show: nwg::MenuItem,
    #[nwg_control(parent: tray_menu, text: "启动桥接")]
    #[nwg_events( OnMenuItemSelected: [GuiApp::tray_start] )]
    m_start: nwg::MenuItem,
    #[nwg_control(parent: tray_menu, text: "停止桥接")]
    #[nwg_events( OnMenuItemSelected: [GuiApp::tray_stop] )]
    m_stop: nwg::MenuItem,
    #[nwg_control(parent: tray_menu, text: "退出")]
    #[nwg_events( OnMenuItemSelected: [GuiApp::exit_app] )]
    m_exit: nwg::MenuItem,

    // ---- 计时器: 轮询后台通道 (100ms) ----
    #[nwg_control(parent: window, interval: std::time::Duration::from_millis(200))]
    #[nwg_events( OnTimerTick: [GuiApp::on_tick] )]
    timer: nwg::AnimationTimer,
}

impl GuiApp {
    /// 在 build_ui 之前初始化运行态 (读配置, 建通道, 准备下拉框数据)。
    fn pre_build(state: &mut AppState, autostart: bool) {
        let cfg = Config::load();
        state.load_config(&cfg);

        // 热键下拉框数据 (无 + 仅语音 + HOTKEY_NAMES)
        let mut hk = vec![DISABLED_KEY.to_string(), VOICE_ONLY.to_string()];
        hk.extend(crate::hotkey::HOTKEY_NAMES.iter().map(|s| s.to_string()));
        state.hotkey_items = hk;
        state.driver_items = vec!["sendinput".to_string(), "interception".to_string()];

        // 建通道
        let (tx, rx) = mpsc::channel::<Msg>();
        state.msg_tx = Some(tx);
        state.msg_rx = Some(rx);

        state.autostart_launch = autostart;
        if autostart || state.minimize_to_tray {
            state.pending_hide = true;
        }
    }

    /// build_ui 之后填充下拉框/复选框/模式提示, 并启动计时器/按需起桥接。
    fn post_build(&self) {
        // 标题栏带上版本号 (nwg_control 属性是静态字面量, 这里运行时设置,
        // 与 single_instance 查找用的 WINDOW_TITLE 同一来源, 不会失配)。
        self.window.set_text(crate::WINDOW_TITLE);
        self.window.set_icon(Some(&self.small_icon));
        if let Some(raw) = self.window.handle.hwnd() {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, ICON_BIG, WM_SETICON};
            unsafe {
                SendMessageW(
                    HWND(raw as _),
                    WM_SETICON,
                    WPARAM(ICON_BIG as usize),
                    LPARAM(self.icon.handle as isize),
                );
            }
        }
        match crate::gui_theme::GuiTheme::install(
            &self.window.handle,
            &[
                &self.action_frame.handle,
                &self.settings_frame.handle,
                &self.deps_frame.handle,
                &self.preferences_frame.handle,
                &self.log_frame.handle,
            ],
            &self.btn_start.handle,
            &[
                &self.btn_stop.handle,
                &self.btn_hide.handle,
                &self.btn_check_deps.handle,
                &self.btn_list_dev.handle,
                &self.btn_diag_hid.handle,
                &self.btn_install_deps.handle,
                &self.btn_clear_log.handle,
            ],
            &[
                &self.lbl_subtitle.handle,
                &self.lbl_battery.handle,
                &self.lbl_run_hint.handle,
                &self.lbl_mode_hint.handle,
                &self.lbl_hotkey_hint.handle,
                &self.lbl_auto_unit.handle,
            ],
            &[
                &self.heading.handle,
                &self.lbl_status.handle,
                &self.lbl_audio_heading.handle,
                &self.lbl_keys_heading.handle,
                &self.lbl_deps_heading.handle,
                &self.lbl_preferences_heading.handle,
                &self.lbl_log_heading.handle,
                &self.lbl_mode.handle,
                &self.lbl_dev.handle,
                &self.lbl_hotkey_fwd.handle,
                &self.lbl_hotkey_bwd.handle,
                &self.lbl_driver.handle,
                &self.lbl_typeless.handle,
                &self.lbl_auto_delay.handle,
            ],
        ) {
            Ok(theme) => *self.theme.borrow_mut() = Some(theme),
            Err(e) => self.append_log(&format!("主题加载失败，使用系统样式: {}", e)),
        }

        let st = self.state.borrow();

        // 填充下拉框
        self.cb_hotkey_fwd.set_collection(st.hotkey_items.clone());
        let fwd_idx = st
            .hotkey_items
            .iter()
            .position(|h| h == &st.hotkey_fwd)
            .unwrap_or(0);
        self.cb_hotkey_fwd.set_selection(Some(fwd_idx));

        self.cb_hotkey_bwd.set_collection(st.hotkey_items.clone());
        let bwd_idx = st
            .hotkey_items
            .iter()
            .position(|h| h == &st.hotkey_bwd)
            .unwrap_or(0);
        self.cb_hotkey_bwd.set_selection(Some(bwd_idx));

        self.cb_driver.set_collection(st.driver_items.clone());
        let drv_idx = st
            .driver_items
            .iter()
            .position(|d| d == &st.driver)
            .unwrap_or(0);
        self.cb_driver.set_selection(Some(drv_idx));

        // 设备名下拉框: 列出 cpal 输出设备 (含 VB-CABLE Input, 若用户装了)
        let devices: Vec<String> = cpal::default_host()
            .output_devices()
            .map(|d| d.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();
        let cable_items = if devices.is_empty() {
            let mut v = vec!["CABLE Input".to_string()];
            if !st.cable_device.is_empty() && st.cable_device != "CABLE Input" {
                v.push(st.cable_device.clone());
            }
            v
        } else {
            let mut v = devices;
            if !st.cable_device.is_empty() && !v.iter().any(|d| d == &st.cable_device) {
                v.push(st.cable_device.clone());
            }
            v
        };
        self.cb_cable.set_collection(cable_items.clone());
        let cable_idx = cable_items
            .iter()
            .position(|d| d == &st.cable_device)
            .unwrap_or(0);
        self.cb_cable.set_selection(Some(cable_idx));
        self.cb_cable.set_enabled(!st.mode_play);

        // 单选按钮: 同步 mode_play (两个都必须显式设, nwg 不会自动互斥)
        if st.mode_play {
            self.rb_play.set_check_state(nwg::RadioButtonState::Checked);
            self.rb_cable
                .set_check_state(nwg::RadioButtonState::Unchecked);
        } else {
            self.rb_cable
                .set_check_state(nwg::RadioButtonState::Checked);
            self.rb_play
                .set_check_state(nwg::RadioButtonState::Unchecked);
        }

        // 复选框
        self.chk_autostart.set_check_state(if st.autostart_on {
            nwg::CheckBoxState::Checked
        } else {
            nwg::CheckBoxState::Unchecked
        });
        self.chk_minimize.set_check_state(if st.minimize_to_tray {
            nwg::CheckBoxState::Checked
        } else {
            nwg::CheckBoxState::Unchecked
        });
        self.chk_autostart_svc
            .set_check_state(if st.auto_start_service {
                nwg::CheckBoxState::Checked
            } else {
                nwg::CheckBoxState::Unchecked
            });

        // 自动回车
        self.chk_auto_enter.set_check_state(if st.auto_enter {
            nwg::CheckBoxState::Checked
        } else {
            nwg::CheckBoxState::Unchecked
        });
        self.cb_auto_enter_mode
            .set_collection(vec!["Enter".to_string(), "Ctrl+Enter".to_string()]);
        let ae_idx = if st.auto_enter_mode == "ctrl_enter" {
            1
        } else {
            0
        };
        self.cb_auto_enter_mode.set_selection(Some(ae_idx));
        self.txt_auto_delay
            .set_text(&format!("{}", st.auto_enter_delay));
        self.chk_typeless_fwd.set_check_state(if st.typeless_fwd {
            nwg::CheckBoxState::Checked
        } else {
            nwg::CheckBoxState::Unchecked
        });
        self.chk_typeless_bwd.set_check_state(if st.typeless_bwd {
            nwg::CheckBoxState::Checked
        } else {
            nwg::CheckBoxState::Unchecked
        });

        // CBS_DROPDOWNLIST 自带下拉箭头, 不再手动 dropdown(true) 避免启动时弹出列表

        self.update_mode_hint(&st);

        drop(st);
        self.update_auto_enter_controls();
        self.update_run_state_ui(false);
        self.ready.set(true);
        self.btn_start.set_focus();

        self.append_log("AJAZZ 语音鼠标桥接器已启动。");
        self.refresh_deps();
        self.timer.start();

        // 收进托盘: --autostart 或勾选"启动最小化到托盘"
        // 起桥接: 仅 --autostart 或勾选"自动起桥接"。两者独立, 收托盘不应隐含起桥接
        // (旧代码里 minimize_to_tray 会错误地自动启动桥接)。
        let (auto_start, autostart_launch, pending_hide) = {
            let st = self.state.borrow();
            (st.auto_start_service, st.autostart_launch, st.pending_hide)
        };
        if pending_hide {
            self.tray.set_visibility(true);
        }
        if autostart_launch || auto_start {
            self.start_bridge();
        }
    }

    // ---------- 窗口/托盘 ----------

    fn on_close(&self) {
        // 服务运行中: 关闭按钮 = 最小化到托盘; 停止状态: 退出程序。
        let running = self.state.borrow().running;
        if running {
            self.window.set_visible(false);
            self.tray.set_visibility(true);
        } else {
            self.stop_bridge();
            nwg::stop_thread_dispatch();
        }
    }

    fn tray_show_win(&self) {
        self.window.set_visible(true);
        self.window.set_focus();
    }

    fn tray_show_menu(&self) {
        let (x, y) = nwg::GlobalCursor::position();
        self.tray_menu.popup(x, y);
    }

    fn tray_start(&self) {
        self.window.set_visible(true);
        self.start_bridge();
    }

    fn tray_stop(&self) {
        self.stop_bridge();
    }

    fn hide_to_tray(&self) {
        self.window.set_visible(false);
        self.tray.set_visibility(true);
    }

    fn exit_app(&self) {
        self.stop_bridge();
        nwg::stop_thread_dispatch();
    }

    // ---------- 设置变更 ----------

    fn on_mode_play(&self) {
        self.set_mode(true);
    }
    fn on_mode_cable(&self) {
        self.set_mode(false);
    }

    fn set_mode(&self, play: bool) {
        // nwg RadioButton 未设 GROUP 标志, 不会自动互斥;
        // 必须显式同步两个单选钮, 否则点击后旧按钮仍保持勾选, 且"虚拟麦克风"
        // 永远无法生效 (旧代码只读 rb_play 状态)。
        self.rb_play.set_check_state(if play {
            nwg::RadioButtonState::Checked
        } else {
            nwg::RadioButtonState::Unchecked
        });
        self.rb_cable.set_check_state(if play {
            nwg::RadioButtonState::Unchecked
        } else {
            nwg::RadioButtonState::Checked
        });
        {
            let mut st = self.state.borrow_mut();
            st.mode_play = play;
        }
        self.cb_cable.set_enabled(!play);
        self.update_mode_hint(&self.state.borrow());
        self.save_settings();
    }

    fn update_mode_hint(&self, st: &AppState) {
        let hint = if st.mode_play {
            "通过系统默认扬声器 / 耳机试听，无需虚拟声卡。\r\n下方的输出设备仅用于虚拟麦克风模式。"
        } else {
            "将声音送入 VB-CABLE；在会议或输入法软件中，\r\n把 CABLE Output 选为麦克风即可使用。"
        };
        self.lbl_mode_hint.set_text(hint);
    }

    fn on_autostart_change(&self) {
        let on = self.chk_autostart.check_state() == nwg::CheckBoxState::Checked;
        if let Err(e) = config::set_autostart(on) {
            let previous = self.state.borrow().autostart_on;
            self.chk_autostart.set_check_state(if previous {
                nwg::CheckBoxState::Checked
            } else {
                nwg::CheckBoxState::Unchecked
            });
            self.append_log(&format!("开机自启设置失败: {}", e));
            return;
        }
        self.state.borrow_mut().autostart_on = on;
        self.append_log(&format!(
            "开机自启: {}",
            if on { "已开启" } else { "已关闭" }
        ));
        self.persist_config();
    }

    fn on_persist_setting(&self) {
        if !self.ready.get() {
            return;
        }
        let mut st = self.state.borrow_mut();
        st.minimize_to_tray = self.chk_minimize.check_state() == nwg::CheckBoxState::Checked;
        st.auto_start_service = self.chk_autostart_svc.check_state() == nwg::CheckBoxState::Checked;
        // 自动发送由 current_config 统一读取, 不依赖变更事件先后顺序。
        st.typeless_fwd = self.chk_typeless_fwd.check_state() == nwg::CheckBoxState::Checked;
        st.typeless_bwd = self.chk_typeless_bwd.check_state() == nwg::CheckBoxState::Checked;
        drop(st);
        self.save_settings();
    }

    fn save_settings(&self) {
        let saved = self.persist_config();
        self.state.borrow_mut().cable_device = saved.cable_device;
    }

    fn update_auto_enter_controls(&self) {
        let enabled = self.chk_auto_enter.check_state() == nwg::CheckBoxState::Checked;
        self.cb_auto_enter_mode.set_enabled(enabled);
        self.txt_auto_delay.set_enabled(enabled);
        self.lbl_auto_delay.set_enabled(enabled);
        self.lbl_auto_unit.set_enabled(enabled);
    }

    fn on_auto_enter_toggle(&self) {
        self.update_auto_enter_controls();
        self.on_persist_setting();
    }

    fn on_delay_change(&self) {
        if self.ready.get() {
            self.save_settings();
        }
    }

    fn on_debug_change(&self) {
        let on = self.chk_debug.check_state() == nwg::CheckBoxState::Checked;
        self.state.borrow_mut().debug_log = on;
        self.append_log(if on {
            "已选择调试日志：下次启动桥接时输出语音包/解码/队列统计。"
        } else {
            "已取消调试日志：下次启动桥接时生效。"
        });
    }

    // ---------- 计时器: 轮询通道 ----------

    fn on_tick(&self) {
        // post_build 已在 run() 里构建后立即调用, 不在这里重复。

        // 首次收进托盘 (-autostart)
        let pending_hide = {
            let mut st = self.state.borrow_mut();
            if st.pending_hide {
                st.pending_hide = false;
                true
            } else {
                false
            }
        };
        if pending_hide {
            self.window.set_visible(false);
            self.tray.set_visibility(true);
            return;
        }

        // 托盘命令由托盘菜单事件直接调用方法 (见 m_start/m_stop 的 nwg_events),
        // 无需走通道。

        // 后台日志/状态
        let rx_msg = self.state.borrow().msg_rx.is_some();
        if rx_msg {
            let st = self.state.borrow();
            let rx = st.msg_rx.as_ref().unwrap();
            let mut msgs = Vec::new();
            while let Ok(m) = rx.try_recv() {
                msgs.push(m);
            }
            drop(st);
            for m in msgs {
                match m {
                    Msg::Log(line) => self.append_log(&line),
                    Msg::Running {
                        generation,
                        running,
                    } => {
                        let update = self.state.borrow_mut().apply_running(generation, running);
                        if let Some(update) = update {
                            if let Some(handle) = update.finished_thread {
                                let _ = handle.join();
                            }
                            if !running && update.was_running {
                                self.append_log("桥接已停止。");
                            }
                            self.update_run_state_ui(running);
                        }
                    }
                    Msg::Battery {
                        generation,
                        pct,
                        charging,
                    } => {
                        if self.state.borrow().active_generation != Some(generation) {
                            continue;
                        }
                        let txt = if charging {
                            format!("鼠标电量  {}% · 充电中", pct)
                        } else {
                            format!("鼠标电量  {}%", pct)
                        };
                        self.lbl_battery.set_text(&txt);
                        self.on_battery_update(pct, charging);
                    }
                }
            }
        }
        if self.log_dirty.get() && self.chk_log_follow.check_state() == nwg::CheckBoxState::Checked
        {
            self.render_log();
        }
    }

    fn update_run_state_ui(&self, running: bool) {
        self.lbl_status.set_text(if running {
            "桥接服务运行中"
        } else {
            "桥接已停止"
        });
        self.lbl_run_hint.set_text(if running {
            "设备连接详情见日志；修改设置后需重新启动桥接。"
        } else {
            "选择输出模式，然后启动桥接。"
        });
        self.btn_start.set_enabled(!running);
        self.btn_stop.set_enabled(running);
        self.m_start.set_enabled(!running);
        self.m_stop.set_enabled(running);
        if !running {
            self.lbl_battery.set_text("鼠标电量  --");
        }
    }

    /// 电池状态更新: 低电量(<10%)与充满(100%充电中)时弹托盘气泡提醒,
    /// 每轮只提醒一次, 状态恢复后重新武装。
    fn on_battery_update(&self, pct: u8, charging: bool) {
        // 先在借用内决定动作, 借用结束后再执行 (避免嵌套 borrow_mut)
        let mut actions: Vec<(String, String, String)> = Vec::new(); // (日志, 标题, 正文)
        {
            let mut st = self.state.borrow_mut();
            // 低电量提醒: 低于/等于 10% 提醒一次, 回到 10% 以上后重新武装
            if pct <= 10 && !st.battery_low_notified {
                st.battery_low_notified = true;
                actions.push((
                    format!("低电量提醒: {}%", pct),
                    "鼠标电量不足".to_string(),
                    format!("电量仅剩 {}%，请及时充电。", pct),
                ));
            } else if pct > 10 {
                st.battery_low_notified = false;
            }
            // 充满提醒: 充电中且 100% 提醒一次, 拔掉充电线后重新武装
            if charging && pct >= 100 && !st.battery_full_notified {
                st.battery_full_notified = true;
                actions.push((
                    "充电完成提醒: 电池已充满".to_string(),
                    "鼠标已充满".to_string(),
                    "电池已充满，可以拔掉充电线。".to_string(),
                ));
            } else if !charging && pct < 100 {
                st.battery_full_notified = false;
            }
        }
        for (log_line, title, body) in actions {
            self.append_log(&log_line);
            // 托盘气泡提醒 (无需注册 AUMID, 可靠)
            let flags =
                nwg::TrayNotificationFlags::INFO_ICON | nwg::TrayNotificationFlags::LARGE_ICON;
            self.tray
                .show(&body, Some(&title), Some(flags), Some(&self.icon));
        }
    }

    // ---------- 日志 ----------

    fn append_log(&self, msg: &str) {
        let mut st = self.state.borrow_mut();
        st.log_lines.push(msg.to_string());
        if st.log_lines.len() > 1000 {
            let drop_n = st.log_lines.len() - 800;
            st.log_lines.drain(..drop_n);
        }
        self.log_dirty.set(true);
    }

    fn render_log(&self) {
        self.log_box
            .set_text(&self.state.borrow().log_lines.join("\r\n"));
        self.scroll_log_to_bottom();
        self.log_dirty.set(false);
    }

    fn on_log_follow(&self) {
        let follow = self.chk_log_follow.check_state() == nwg::CheckBoxState::Checked;
        self.lbl_log_heading.set_text(if follow {
            "运行日志"
        } else {
            "日志 · 暂停刷新"
        });
        if follow {
            self.render_log();
        }
    }

    fn clear_log(&self) {
        self.state.borrow_mut().log_lines.clear();
        self.log_box.set_text("");
        self.log_dirty.set(false);
    }

    /// 把日志框滚动到最后一行 (EM_LINESCROLL, 保证新日志可见)。
    fn scroll_log_to_bottom(&self) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
        const EM_GETLINECOUNT: u32 = 0x00BA;
        const EM_LINESCROLL: u32 = 0x00B6;
        if let Some(raw) = self.log_box.handle.hwnd() {
            unsafe {
                let h = windows::Win32::Foundation::HWND(raw as _);
                let lines = SendMessageW(h, EM_GETLINECOUNT, WPARAM(0), LPARAM(0)).0;
                if lines > 1 {
                    SendMessageW(h, EM_LINESCROLL, WPARAM(0), LPARAM(lines - 1));
                }
            }
        }
    }

    // ---------- 依赖检查 ----------

    /// 检测 AJ200 系列鼠标。返回 (状态行, 附加信息行)。
    fn detect_mouse() -> (String, Vec<String>) {
        use hidapi::HidApi;
        let api = match HidApi::new() {
            Ok(a) => a,
            Err(_) => return ("检测失败 (HID 初始化错误)".into(), vec![]),
        };
        // 按 PID 去重收集命中的支持设备 (任一接口命中即算)
        let mut seen: Vec<u16> = Vec::new();
        for d in api.device_list() {
            if d.vendor_id() == crate::VID
                && crate::devices::is_supported(d.product_id())
                && !seen.contains(&d.product_id())
            {
                seen.push(d.product_id());
            }
        }
        if seen.is_empty() {
            return ("未检测到 (AJ200 系列)".into(), vec![]);
        }
        seen.sort();
        let mut names: Vec<String> = Vec::new();
        let mut info: Vec<String> = Vec::new();
        for pid in &seen {
            let desc = crate::devices::describe_pid(*pid).unwrap_or_default();
            names.push(desc.clone());
            info.push(format!("{}  PID=0x{:04X}", desc, pid));
        }
        (names.join(" / "), info)
    }

    fn refresh_deps(&self) {
        let mut items: Vec<(String, String)> = Vec::new();
        let mut extra: Vec<String> = Vec::new();

        // 鼠标 (AJ200 系列, PID 白名单识别)
        let (mouse_status, mouse_info) = Self::detect_mouse();
        items.push(("鼠标".to_string(), mouse_status));
        extra.extend(mouse_info);

        // VB-CABLE 虚拟音频设备
        let mut found_cable = false;
        if let Ok(devs) = cpal::default_host().output_devices() {
            for d in devs {
                if let Ok(name) = d.name() {
                    if name.contains("CABLE") {
                        found_cable = true;
                        break;
                    }
                }
            }
        }
        items.push((
            "VB-CABLE".to_string(),
            if found_cable {
                "已安装".into()
            } else {
                "未安装（虚拟麦克风需安装）".into()
            },
        ));

        // Interception 驱动 (可选) — 检查 DLL + 内核驱动
        let mut dll = false;
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                if dir.join("interception.dll").exists() {
                    dll = true;
                }
            }
        }
        if !dll && std::path::Path::new("interception.dll").exists() {
            dll = true;
        }
        let driver = {
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_CREATION_DISPOSITION, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
            };
            let dev: Vec<u16> = "\\\\.\\interception00\0".encode_utf16().collect();
            unsafe {
                match CreateFileW(
                    PCWSTR(dev.as_ptr()),
                    GENERIC_READ.0,
                    FILE_SHARE_MODE(0),
                    None,
                    FILE_CREATION_DISPOSITION(3), // OPEN_EXISTING
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                ) {
                    Ok(h) => {
                        let _ = CloseHandle(h);
                        true
                    }
                    Err(_) => false,
                }
            }
        };
        let status = if driver && dll {
            "已安装".into()
        } else if driver {
            "缺 DLL（可选）".into()
        } else if dll {
            "缺驱动（可选）".into()
        } else {
            "未安装（可选）".into()
        };
        items.push(("Interception".to_string(), status));

        let mut s = String::new();
        for (k, v) in &items {
            s.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !extra.is_empty() {
            s.push_str("\r\n");
            for line in &extra {
                s.push_str(&format!("{}\r\n", line));
            }
        }
        self.deps_list.set_text(&s);
    }

    fn diag_hid(&self) {
        let tx = self.state.borrow().msg_tx.clone().unwrap();
        let lines = std::cell::RefCell::new(Vec::new());
        hid::list_hid(&|m: &str| {
            lines.borrow_mut().push(m.to_string());
        });
        for l in lines.borrow().iter() {
            let _ = tx.send(Msg::Log(l.clone()));
        }
    }

    fn list_devices_btn(&self) {
        let selected = self
            .cb_cable
            .selection_string()
            .unwrap_or_else(|| self.state.borrow().cable_device.clone());
        match cpal::default_host().output_devices() {
            Ok(devs) => {
                let mut names: Vec<String> = devs.filter_map(|d| d.name().ok()).collect();
                self.append_log("已刷新音频设备:");
                if names.is_empty() {
                    self.append_log("  未检测到输出设备；保留当前设置。");
                }
                for (i, name) in names.iter().enumerate() {
                    self.append_log(&format!("  [{}] {}", i, name));
                }
                if !names.contains(&selected) {
                    names.push(selected.clone());
                }
                let index = names.iter().position(|name| name == &selected);
                self.cb_cable.set_collection(names);
                self.cb_cable.set_selection(index);
            }
            Err(e) => self.append_log(&format!("刷新音频设备失败: {}", e)),
        }
    }

    fn install_deps(&self) {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONINFORMATION, MB_OK, MB_YESNO, SHOW_WINDOW_CMD,
        };

        let open_url = |url: &str| {
            let u: Vec<u16> = format!("{}\0", url).encode_utf16().collect();
            unsafe {
                let _ = ShellExecuteW(
                    None,
                    None,
                    PCWSTR(u.as_ptr()),
                    None,
                    None,
                    SHOW_WINDOW_CMD(5),
                );
            }
        };

        let msg = |t: &str,
                   b: &str,
                   flags: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE|
         -> bool {
            let tv: Vec<u16> = format!("{}\0", t).encode_utf16().collect();
            let bv: Vec<u16> = format!("{}\0", b).encode_utf16().collect();
            unsafe { MessageBoxW(None, PCWSTR(bv.as_ptr()), PCWSTR(tv.as_ptr()), flags) == IDYES }
        };

        // 说明
        let _ = msg("安装依赖",
            "VB-CABLE (仅虚拟麦克风模式需要, 扬声器试听无需安装)\nhttps://vb-audio.com/Cable/index.htm\n\nInterception (键盘驱动, 非必须)\nhttps://github.com/oblitum/Interception\n\n提示: Interception 仅当某些程序模拟按键无法正常使用时才需要安装, 如豆包输入法。",
            MB_OK | MB_ICONINFORMATION);

        // 逐项询问是否打开链接
        if msg(
            "VB-CABLE",
            "在浏览器中打开 VB-CABLE 下载页?",
            MB_YESNO | MB_ICONINFORMATION,
        ) {
            open_url("https://vb-audio.com/Cable/index.htm");
        }
        if msg(
            "Interception",
            "在浏览器中打开 Interception 项目页?\n(非必须, 仅当模拟按键异常时需要)",
            MB_YESNO | MB_ICONINFORMATION,
        ) {
            open_url("https://github.com/oblitum/Interception");
        }
    }

    // ---------- 配置持久化 ----------

    fn current_config(&self) -> Config {
        let st = self.state.borrow();
        let hotkey_fwd = self
            .cb_hotkey_fwd
            .selection()
            .and_then(|i| st.hotkey_items.get(i).cloned())
            .unwrap_or_else(|| "无".to_string());
        let hotkey_bwd = self
            .cb_hotkey_bwd
            .selection()
            .and_then(|i| st.hotkey_items.get(i).cloned())
            .unwrap_or_else(|| "无".to_string());
        let driver = self
            .cb_driver
            .selection()
            .and_then(|i| st.driver_items.get(i).cloned())
            .unwrap_or_else(|| "sendinput".to_string());
        st.config_from_selections(ConfigSelections {
            cable_device: self
                .cb_cable
                .selection_string()
                .unwrap_or_else(|| st.cable_device.clone()),
            hotkey_fwd,
            hotkey_bwd,
            driver,
            auto_enter: self.chk_auto_enter.check_state() == nwg::CheckBoxState::Checked,
            auto_enter_mode: self.cb_auto_enter_mode.selection_string(),
            auto_enter_delay: self.txt_auto_delay.text(),
        })
    }

    fn persist_config(&self) -> Config {
        let cfg = self.current_config();
        self.state.borrow_mut().auto_enter_delay = cfg.auto_enter_delay;
        match cfg.save() {
            Ok(()) => self.lbl_run_hint.set_text(if self.state.borrow().running {
                "设置已保存；音频、热键和发送选项在下次启动桥接时生效。"
            } else {
                "设置已保存，启动桥接即可使用。"
            }),
            Err(e) => {
                self.lbl_run_hint
                    .set_text("设置保存失败，详情见日志；本次启动仍可使用当前设置。");
                self.append_log(&format!("配置保存失败 (本次设置仍可用于桥接): {}", e));
            }
        }
        cfg
    }

    // ---------- 桥接启停 ----------

    fn start_bridge(&self) {
        // 先取所有运行态/配置值, 一并释放借用, 再处理桥接启动 (避免嵌套 borrow_mut)。
        let (msg_tx_opt, debug) = {
            let st = self.state.borrow();
            if st.running {
                return;
            }
            (st.msg_tx.clone(), st.debug_log)
        };
        let msg_tx = match msg_tx_opt {
            Some(t) => t,
            None => return,
        };

        // 保存与本次启动使用同一份控件快照。
        let cfg = self.persist_config();

        let Some((generation, stop)) = self.state.borrow_mut().begin_run() else {
            return;
        };
        {
            let mut st = self.state.borrow_mut();
            // 键位描述: 绑了热键 → 热键名; 仅语音 → "仅语音"; 都没开 → "禁用"
            let key_desc = |name: &Option<String>, ai: bool| -> String {
                match (name, ai) {
                    (Some(n), _) => n.clone(),
                    (None, true) => "仅语音".to_string(),
                    (None, false) => "禁用".to_string(),
                }
            };
            st.log_lines.push(format!(
                "已启动: 模式={} 前进={} 后退={} 驱动={}",
                cfg.mode,
                key_desc(&cfg.hotkey_forward, cfg.ai_fwd),
                key_desc(&cfg.hotkey_backward, cfg.ai_bwd),
                cfg.driver
            ));
        }

        self.log_dirty.set(true);
        self.update_run_state_ui(true);
        self.lbl_status.set_text("正在启动桥接…");
        self.lbl_run_hint
            .set_text("正在初始化音频输出与桥接服务，请稍候。");

        let handle = std::thread::spawn(move || {
            let log_tx2 = msg_tx.clone();
            let log = move |m: &str| {
                let _ = log_tx2.send(Msg::Log(m.to_string()));
            };
            let bat_tx = msg_tx.clone();
            let battery_cb = move |pct: u8, charging: bool| {
                let _ = bat_tx.send(Msg::Battery {
                    generation,
                    pct,
                    charging,
                });
            };
            let device_name = if cfg.mode == "play" {
                None
            } else {
                Some(cfg.cable_device.as_str())
            };
            match AudioOutput::new(device_name, SAMPLE_RATE) {
                Ok(audio) => {
                    log(&format!("音频输出设备: {}", audio.device_name()));
                    let mut sink = |s: &[i16]| audio.push_pcm(s);
                    match Bridge::new(&cfg, &log) {
                        Ok(mut bridge) => {
                            let _ = msg_tx.send(Msg::Running {
                                generation,
                                running: true,
                            });
                            if let Err(e) = bridge.run(
                                &mut sink,
                                &|| audio.diagnostics(),
                                &stop,
                                debug,
                                &log,
                                &battery_cb,
                            ) {
                                log(&format!("桥接错误: {}", e));
                            }
                        }
                        Err(e) => log(&format!("连接失败: {}", e)),
                    }
                }
                Err(e) => log(&format!("音频设备错误: {}", e)),
            }
            let _ = msg_tx.send(Msg::Running {
                generation,
                running: false,
            });
        });

        self.state.borrow_mut().bridge_thread = Some(handle);
    }

    fn stop_bridge(&self) {
        // 作废代次 → 置停止标志 → 释放借用后 join, 迟到的旧代消息全部被丢弃。
        let handle = self.state.borrow_mut().request_stop();
        if let Some(h) = handle {
            let _ = h.join();
        }
        self.update_run_state_ui(false);
        self.lbl_battery.set_text("鼠标电量  --");
        {
            let mut st = self.state.borrow_mut();
            st.battery_low_notified = false;
            st.battery_full_notified = false;
        }
        self.append_log("桥接已停止。");
    }
}

/// GUI 入口 (由 main 在启用 gui feature 时调用)。
pub fn run(autostart: bool) {
    if !single_instance::try_acquire() {
        single_instance::bring_existing_to_front();
        return;
    }
    if let Err(e) = nwg::init() {
        show_error_box(&format!("GUI 初始化失败:\n{}", e));
        return;
    }
    let mut font = nwg::Font::default();
    if nwg::Font::builder()
        .family("Microsoft YaHei UI")
        .size_absolute(14)
        .build(&mut font)
        .is_ok()
    {
        nwg::Font::set_global_default(Some(font));
    }

    // pre_build: 读配置 + 建通道 + 准备下拉框数据 (控件尚未创建)
    let mut default_state = AppState::default();
    GuiApp::pre_build(&mut default_state, autostart);

    // build_ui: 创建控件 (Default 给出 RefCell<AppState> 的初值; 但我们要用 pre_build 的状态)
    // NwgUi::build_ui 接收 data 并消费之, 故把默认 state 换成我们的:
    let app = GuiApp {
        state: RefCell::new(default_state),
        ..Default::default()
    };
    let _ui = match GuiApp::build_ui(app) {
        Ok(ui) => ui,
        Err(e) => {
            show_error_box(&format!("GUI 构建失败:\n{}", e));
            return;
        }
    };
    // 计时器由 post_build 启动, 初始化不能依赖首次 tick。
    _ui.post_build();
    // _ui 必须保留到 dispatch 结束 (持有 GuiApp 的所有权)。
    nwg::dispatch_thread_events();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_channel() -> AppState {
        let (tx, rx) = mpsc::channel::<Msg>();
        AppState {
            msg_tx: Some(tx),
            msg_rx: Some(rx),
            ..AppState::default()
        }
    }

    #[test]
    fn stale_running_false_cannot_clear_new_run_stop() {
        let mut st = state_with_channel();
        let (gen1, stop1) = st.begin_run().unwrap();

        // 模拟: 旧线程退出前 UI 已主动停止并重启 (gen2)。
        st.request_stop();
        let (gen2, stop2) = st.begin_run().unwrap();
        assert_ne!(gen1, gen2);
        assert!(st.running);

        // 旧代 Running(false) 到达: 不得影响新代运行态或停止令牌。
        assert!(st.apply_running(gen1, false).is_none());
        assert!(st.running);
        assert!(!stop2.load(Ordering::SeqCst));

        // 新代 Running(true) 随后到达: 状态保持运行。
        assert!(st.apply_running(gen2, true).is_some());
        assert!(st.running);

        // 旧代停止令牌翻转也不得干扰新代。
        stop1.store(true, Ordering::SeqCst);
        assert!(!stop2.load(Ordering::SeqCst));

        // 新代正常结束: 返回线程收尾句柄并清空令牌。
        st.bridge_thread = Some(std::thread::spawn(|| {}));
        let update = st.apply_running(gen2, false).unwrap();
        assert!(update.finished_thread.is_some());
        assert!(!st.running);
        assert!(st.stop.is_none());
    }

    #[test]
    fn request_stop_invalidates_generation_and_signals_worker() {
        let mut st = state_with_channel();
        let (_, stop) = st.begin_run().unwrap();
        st.request_stop();
        assert!(!st.running);
        assert!(st.active_generation.is_none());
        assert!(stop.load(Ordering::SeqCst));
        // 已停止后不得立刻重启残留状态。
        assert!(st.begin_run().is_some());
    }

    #[test]
    fn begin_run_rejects_concurrent_or_lingering_runs() {
        let mut st = state_with_channel();
        assert!(st.begin_run().is_some());
        assert!(st.begin_run().is_none(), "运行中不得重复启动");
        st.request_stop();
        assert!(st.begin_run().is_some(), "完全收尾后允许再次启动");
    }

    #[test]
    fn auto_enter_delay_clamps_and_falls_back() {
        assert_eq!(parse_auto_enter_delay("2", 0.5), 2.0);
        assert_eq!(parse_auto_enter_delay("99", 0.5), 10.0);
        assert_eq!(parse_auto_enter_delay("-5", 0.5), 0.0);
        assert_eq!(parse_auto_enter_delay("abc", 0.5), 0.5);
        assert_eq!(parse_auto_enter_delay("NaN", 3.0), 3.0);
        assert_eq!(parse_auto_enter_delay("inf", 0.5), 0.5);
    }

    #[test]
    fn hotkey_labels_preserve_voice_only_and_legacy_disabled_semantics() {
        assert_eq!(split_hotkey_sel(DISABLED_KEY), (None, false));
        assert_eq!(split_hotkey_sel("无"), (None, false));
        assert_eq!(split_hotkey_sel(VOICE_ONLY), (None, true));
        assert_eq!(split_hotkey_sel("R_alt"), (Some("R_alt".into()), false));

        let mut st = AppState::default();
        st.load_config(&Config {
            hotkey_forward: None,
            hotkey_backward: None,
            ai_fwd: false,
            ai_bwd: true,
            ..Config::default()
        });
        assert_eq!(st.hotkey_fwd, DISABLED_KEY);
        assert_eq!(st.hotkey_bwd, VOICE_ONLY);
    }

    #[test]
    fn config_from_selections_reads_current_controls() {
        let mut st = state_with_channel();
        st.wired_pids = vec!["0xED03".to_string()];
        st.auto_enter_delay = 9.0;
        let cfg = st.config_from_selections(ConfigSelections {
            cable_device: "CABLE Input".to_string(),
            hotkey_fwd: "无".to_string(),
            hotkey_bwd: "仅语音(无热键)".to_string(),
            driver: "sendinput".to_string(),
            auto_enter: true,
            auto_enter_mode: Some("Ctrl+Enter".to_string()),
            auto_enter_delay: "0.25".to_string(),
        });
        assert_eq!(cfg.hotkey_forward, None);
        assert!(!cfg.ai_fwd);
        assert!(cfg.ai_bwd);
        assert!(cfg.auto_enter);
        assert_eq!(cfg.auto_enter_mode, "ctrl_enter");
        assert_eq!(cfg.auto_enter_delay, 0.25);
        assert_eq!(
            cfg.wired_pids,
            vec!["0xED03".to_string()],
            "手工 PID 配置不得被保存抹除"
        );
    }
}
