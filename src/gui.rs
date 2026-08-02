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

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use native_windows_gui as nwg;
use native_windows_derive as nwd;
use nwd::NwgUi;
use nwg::NativeUi;

use cpal::traits::{DeviceTrait, HostTrait};
use crate::audio::AudioOutput;
use crate::bridge::Bridge;
use crate::config::{self, Config};
use crate::dialog::show_error_box;
use crate::{hid, single_instance, SAMPLE_RATE};

/// 后台线程 → GUI 主线程的通道消息。
enum Msg {
    Log(String),
    Running(bool),
    Battery(u8, bool), // 电量%, 是否充电中
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

    // 运行态
    running: bool,
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
            running: false,
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

#[derive(Default, NwgUi)]
pub struct GuiApp {
    // 运行态 (无 nwg 属性; 由 RefCell 包裹, 在 &self 回调里 borrow_mut)
    state: RefCell<AppState>,

    // ---- 顶层窗口 ----
    #[nwg_control(size: (580, 826), position: (300, 120), title: "AJAZZ 语音鼠标桥接器", flags: "WINDOW|VISIBLE|MINIMIZE_BOX")]
    #[nwg_events( OnWindowClose: [GuiApp::on_close] )]
    window: nwg::Window,

    // ---- 标题 ----
    #[nwg_control(parent: window, text: "AJAZZ 语音鼠标桥接器", position: (20, 12), size: (540, 28))]
    heading: nwg::Label,

    // ---- 状态 ----
    #[nwg_control(parent: window, text: "● 已停止", position: (20, 46), size: (200, 20))]
    lbl_status: nwg::Label,
    #[nwg_control(parent: window, text: "电量: --", position: (240, 46), size: (320, 20))]
    lbl_battery: nwg::Label,

    // ---- 依赖检查 (Frame 容器 + 子控件) ----
    #[nwg_control(parent: window, size: (540, 214), position: (20, 76), flags: "VISIBLE|BORDER")]
    deps_frame: nwg::Frame,
    #[nwg_control(parent: deps_frame, position: (10, 24), size: (420, 178), flags: "VISIBLE|AUTOVSCROLL|VSCROLL", readonly: true)]
    deps_list: nwg::TextBox,
    #[nwg_control(parent: deps_frame, text: "检查依赖", position: (440, 24), size: (90, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::refresh_deps] )]
    btn_check_deps: nwg::Button,
    #[nwg_control(parent: deps_frame, text: "诊断 HID", position: (440, 60), size: (90, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::diag_hid] )]
    btn_diag_hid: nwg::Button,
    #[nwg_control(parent: deps_frame, text: "列出设备", position: (440, 96), size: (90, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::list_devices_btn] )]
    btn_list_dev: nwg::Button,
    #[nwg_control(parent: deps_frame, text: "安装依赖", position: (440, 132), size: (90, 30))]
    #[nwg_events( OnButtonClick: [GuiApp::install_deps] )]
    btn_install_deps: nwg::Button,

    // ---- 设置 (Frame 容器 + 子控件) ----
    #[nwg_control(parent: window, size: (540, 260), position: (20, 298), flags: "VISIBLE|BORDER")]
    settings_frame: nwg::Frame,

    #[nwg_control(parent: settings_frame, text: "输出模式", position: (10, 24), size: (75, 22))]
    lbl_mode: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "扬声器试听", position: (100, 24), size: (120, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_mode_play] )]
    rb_play: nwg::RadioButton,
    #[nwg_control(parent: settings_frame, text: "虚拟麦克风", position: (225, 24), size: (120, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_mode_cable] )]
    rb_cable: nwg::RadioButton,
    #[nwg_control(parent: settings_frame, text: "", position: (10, 50), size: (520, 40))]
    lbl_mode_hint: nwg::Label,

    #[nwg_control(parent: settings_frame, text: "设备名", position: (10, 97), size: (55, 22))]
    lbl_dev: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (300, 28), position: (70, 94))]
    cb_cable: nwg::ComboBox<String>,

    #[nwg_control(parent: settings_frame, text: "前进键", position: (10, 131), size: (55, 22))]
    lbl_hotkey_fwd: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (110, 28), position: (70, 128))]
    cb_hotkey_fwd: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "后退键", position: (200, 131), size: (55, 22))]
    lbl_hotkey_bwd: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (110, 28), position: (260, 128))]
    cb_hotkey_bwd: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "注入", position: (380, 131), size: (35, 22))]
    lbl_driver: nwg::Label,
    #[nwg_control(parent: settings_frame, size: (120, 28), position: (415, 128))]
    cb_driver: nwg::ComboBox<String>,

    // ---- Typeless 模式 ----
    #[nwg_control(parent: settings_frame, text: "Typeless模式", position: (10, 164), size: (95, 22))]
    lbl_typeless: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "前进", position: (110, 164), size: (55, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_typeless_fwd: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, text: "后退", position: (170, 164), size: (55, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_typeless_bwd: nwg::CheckBox,

    // ---- 自动回车 ----
    #[nwg_control(parent: settings_frame, text: "自动发送", position: (10, 197), size: (85, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_auto_enter_toggle] )]
    chk_auto_enter: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, size: (110, 28), position: (100, 194))]
    cb_auto_enter_mode: nwg::ComboBox<String>,
    #[nwg_control(parent: settings_frame, text: "延迟", position: (220, 197), size: (38, 22))]
    lbl_auto_delay: nwg::Label,
    #[nwg_control(parent: settings_frame, text: "0.5", position: (260, 196), size: (38, 24))]
    txt_auto_delay: nwg::TextInput,
    #[nwg_control(parent: settings_frame, text: "秒", position: (302, 197), size: (25, 22))]
    lbl_auto_unit: nwg::Label,

    #[nwg_control(parent: settings_frame, text: "开机自启", position: (10, 228), size: (85, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_autostart_change] )]
    chk_autostart: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, text: "启动最小化到托盘", position: (100, 228), size: (155, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_minimize: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, text: "自动起桥接", position: (260, 228), size: (105, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_persist_setting] )]
    chk_autostart_svc: nwg::CheckBox,
    #[nwg_control(parent: settings_frame, text: "调试日志", position: (370, 228), size: (90, 22))]
    #[nwg_events( OnButtonClick: [GuiApp::on_debug_change] )]
    chk_debug: nwg::CheckBox,

    // ---- 启停按钮 (直接在 window 上) ----
    #[nwg_control(parent: window, text: "▶ 启动", position: (20, 568), size: (130, 36))]
    #[nwg_events( OnButtonClick: [GuiApp::start_bridge] )]
    btn_start: nwg::Button,
    #[nwg_control(parent: window, text: "■ 停止", position: (160, 568), size: (130, 36))]  
    #[nwg_events( OnButtonClick: [GuiApp::stop_bridge] )]
    btn_stop: nwg::Button,
    #[nwg_control(parent: window, text: "最小化到托盘", position: (300, 568), size: (140, 36))]
    #[nwg_events( OnButtonClick: [GuiApp::hide_to_tray] )]
    btn_hide: nwg::Button,

    // ---- 日志 (Frame 容器 + 子控件) ----
    #[nwg_control(parent: window, size: (540, 190), position: (20, 618), flags: "VISIBLE|BORDER")]
    log_frame: nwg::Frame,
    #[nwg_control(parent: log_frame, position: (10, 24), size: (520, 150), flags: "VISIBLE|AUTOVSCROLL|VSCROLL", readonly: true)]
    log_box: nwg::TextBox,

    // ---- 系统托盘 ----
    #[nwg_resource(source_system: Some(nwg::OemIcon::Information))]
    icon: nwg::Icon,
    #[nwg_control(parent: window, icon: Some(&data.icon), tip: Some("AJAZZ 语音鼠标"))]
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
    #[nwg_control(parent: window)]
    #[nwg_control(parent: window, interval: 200)]
    #[nwg_events( OnTimerTick: [GuiApp::on_tick] )]
    timer: nwg::AnimationTimer,
}

impl GuiApp {
    /// 在 build_ui 之前初始化运行态 (读配置, 建通道, 准备下拉框数据)。
    fn pre_build(state: &mut AppState, autostart: bool) {
        let cfg = Config::load();
        state.mode_play = cfg.mode == "play";
        state.cable_device = cfg.cable_device.clone();
        state.hotkey_fwd = cfg.hotkey_forward.clone().unwrap_or_else(|| "无".to_string());
        state.hotkey_bwd = cfg.hotkey_backward.clone().unwrap_or_else(|| "无".to_string());
        state.driver = cfg.driver.clone();
        state.autostart_on = cfg.autostart;
        state.minimize_to_tray = cfg.minimize_to_tray;
        state.auto_start_service = cfg.auto_start_service;
        state.auto_enter = cfg.auto_enter;
        state.auto_enter_mode = cfg.auto_enter_mode.clone();
        state.auto_enter_delay = cfg.auto_enter_delay;
        state.typeless_fwd = cfg.typeless_fwd;
        state.typeless_bwd = cfg.typeless_bwd;

        // 热键下拉框数据 (无 + HOTKEY_NAMES)
        let mut hk = vec!["无".to_string()];
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
        let st = self.state.borrow();

        // 填充下拉框
        self.cb_hotkey_fwd.set_collection(st.hotkey_items.clone());
        let fwd_idx = st.hotkey_items.iter().position(|h| h == &st.hotkey_fwd).unwrap_or(0);
        self.cb_hotkey_fwd.set_selection(Some(fwd_idx));

        self.cb_hotkey_bwd.set_collection(st.hotkey_items.clone());
        let bwd_idx = st.hotkey_items.iter().position(|h| h == &st.hotkey_bwd).unwrap_or(0);
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
        let cable_idx = cable_items.iter().position(|d| d == &st.cable_device).unwrap_or(0);
        self.cb_cable.set_selection(Some(cable_idx));
        self.cb_cable.set_enabled(!st.mode_play);

        // 单选按钮: 同步 mode_play (两个都必须显式设, nwg 不会自动互斥)
        if st.mode_play {
            self.rb_play.set_check_state(nwg::RadioButtonState::Checked);
            self.rb_cable.set_check_state(nwg::RadioButtonState::Unchecked);
        } else {
            self.rb_cable.set_check_state(nwg::RadioButtonState::Checked);
            self.rb_play.set_check_state(nwg::RadioButtonState::Unchecked);
        }

        // 复选框
        self.chk_autostart.set_check_state(if st.autostart_on { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
        self.chk_minimize.set_check_state(if st.minimize_to_tray { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
        self.chk_autostart_svc.set_check_state(if st.auto_start_service { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });

        // 自动回车
        self.chk_auto_enter.set_check_state(if st.auto_enter { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
        self.cb_auto_enter_mode.set_collection(vec!["Enter".to_string(), "Ctrl+Enter".to_string()]);
        let ae_idx = if st.auto_enter_mode == "ctrl_enter" { 1 } else { 0 };
        self.cb_auto_enter_mode.set_selection(Some(ae_idx));
        self.txt_auto_delay.set_text(&format!("{}", st.auto_enter_delay));
        self.chk_typeless_fwd.set_check_state(if st.typeless_fwd { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
        self.chk_typeless_bwd.set_check_state(if st.typeless_bwd { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });

        // CBS_DROPDOWNLIST 自带下拉箭头, 不再手动 dropdown(true) 避免启动时弹出列表

        self.update_mode_hint(&st);

        drop(st);

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

    fn on_mode_play(&self) { self.set_mode(true); }
    fn on_mode_cable(&self) { self.set_mode(false); }

    fn set_mode(&self, play: bool) {
        // nwg RadioButton 未设 GROUP 标志, 不会自动互斥;
        // 必须显式同步两个单选钮, 否则点击后旧按钮仍保持勾选, 且"虚拟麦克风"
        // 永远无法生效 (旧代码只读 rb_play 状态)。
        self.rb_play.set_check_state(if play { nwg::RadioButtonState::Checked } else { nwg::RadioButtonState::Unchecked });
        self.rb_cable.set_check_state(if play { nwg::RadioButtonState::Unchecked } else { nwg::RadioButtonState::Checked });
        {
            let mut st = self.state.borrow_mut();
            st.mode_play = play;
        }
        self.cb_cable.set_enabled(!play);
        self.update_mode_hint(&self.state.borrow());
        self.persist_config();
    }

    fn update_mode_hint(&self, st: &AppState) {
        let hint = if st.mode_play {
            "提示: 扬声器试听直接经扬声器/耳机播放, 用于确认鼠标确实在出声。"
        } else {
            "提示: 虚拟麦克风把声音送入 VB-CABLE 输入;\r\n在其他软件把 'CABLE Output' 选为麦克风, 或开启该设备监听才能听到。"
        };
        self.lbl_mode_hint.set_text(hint);
    }

    fn on_autostart_change(&self) {
        let on = self.chk_autostart.check_state() == nwg::CheckBoxState::Checked;
        let _ = config::set_autostart(on);
        self.state.borrow_mut().autostart_on = on;
        self.append_log(&format!("开机自启: {}", if on { "已开启" } else { "已关闭" }));
        self.persist_config();
    }

    fn on_persist_setting(&self) {
        let mut st = self.state.borrow_mut();
        st.minimize_to_tray = self.chk_minimize.check_state() == nwg::CheckBoxState::Checked;
        st.auto_start_service = self.chk_autostart_svc.check_state() == nwg::CheckBoxState::Checked;
        // 自动回车
        st.auto_enter = self.chk_auto_enter.check_state() == nwg::CheckBoxState::Checked;
        st.auto_enter_mode = if self.cb_auto_enter_mode.selection_string().as_deref() == Some("Ctrl+Enter") { "ctrl_enter".to_string() } else { "enter".to_string() };
        if let Ok(d) = self.txt_auto_delay.text().parse::<f64>() {
            st.auto_enter_delay = d.clamp(0.0, 10.0);
        }
        st.typeless_fwd = self.chk_typeless_fwd.check_state() == nwg::CheckBoxState::Checked;
        st.typeless_bwd = self.chk_typeless_bwd.check_state() == nwg::CheckBoxState::Checked;
        drop(st);
        self.persist_config();
    }

    fn on_auto_enter_toggle(&self) {
        self.on_persist_setting();
    }

    fn on_debug_change(&self) {
        let on = self.chk_debug.check_state() == nwg::CheckBoxState::Checked;
        self.state.borrow_mut().debug_log = on;
        self.append_log(if on {
            "调试日志已开启 (每秒输出语音包/解码/队列/回调统计数据)"
        } else {
            "调试日志已关闭"
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
            let mut running_changed = None;
            for m in msgs {
                match m {
                    Msg::Log(line) => self.append_log(&line),
                    Msg::Running(r) => running_changed = Some(r),
                    Msg::Battery(pct, charging) => {
                        let txt = if charging {
                            format!("电量: {}% 充电中", pct)
                        } else {
                            format!("电量: {}%", pct)
                        };
                        self.lbl_battery.set_text(&txt);
                        self.on_battery_update(pct, charging);
                    }
                }
            }
            if let Some(r) = running_changed {
                let was_running = self.state.borrow().running;
                self.state.borrow_mut().running = r;
                if !r {
                    self.state.borrow_mut().stop = None;
                    // 主动停止时 stop_bridge 已打过日志; 这里只在后台线程自行退出时补一条
                    if was_running {
                        self.append_log("桥接已停止。");
                    }
                }
                self.update_run_state_ui(r);
            }
        }
    }

    fn update_run_state_ui(&self, running: bool) {
        self.lbl_status.set_text(if running { "● 运行中" } else { "● 已停止" });
        self.btn_start.set_enabled(!running);
        self.btn_stop.set_enabled(running);
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
            let flags = nwg::TrayNotificationFlags::INFO_ICON | nwg::TrayNotificationFlags::LARGE_ICON;
            self.tray.show(&body, Some(&title), Some(flags), Some(&self.icon));
        }
    }

    // ---------- 日志 ----------

    fn append_log(&self, msg: &str) {
        self.state.borrow_mut().log_lines.push(msg.to_string());
        // 限长
        let text = {
            let mut st = self.state.borrow_mut();
            if st.log_lines.len() > 1000 {
                let drop_n = st.log_lines.len() - 800;
                st.log_lines.drain(..drop_n);
            }
            st.log_lines.join("\r\n")
        };
        self.log_box.set_text(&text);
        self.scroll_log_to_bottom();
    }

    /// 把日志框滚动到最后一行 (EM_LINESCROLL, 保证新日志可见)。
    fn scroll_log_to_bottom(&self) {
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
        use windows::Win32::Foundation::{WPARAM, LPARAM};
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
        items.push(("VB-CABLE".to_string(), if found_cable { "已安装".into() } else { "未检测到".into() }));

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
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_SHARE_MODE, FILE_CREATION_DISPOSITION,
                FILE_FLAGS_AND_ATTRIBUTES,
            };
            use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
            use windows::core::PCWSTR;
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
                    Ok(h) => { let _ = CloseHandle(h); true }
                    Err(_) => false,
                }
            }
        };
        let status = if driver && dll { "已安装".into() }
            else if driver { "缺 DLL".into() }
            else if dll { "缺驱动".into() }
            else { "未安装".into() };
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
        let tx = self.state.borrow().msg_tx.clone().unwrap();
        let _ = tx.send(Msg::Log("可用输出设备:".into()));
        if let Ok(devs) = cpal::default_host().output_devices() {
            for (i, d) in devs.enumerate() {
                if let Ok(name) = d.name() {
                    let _ = tx.send(Msg::Log(format!("  [{}] {}", i, name)));
                }
            }
        }
    }

    fn install_deps(&self) {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, SHOW_WINDOW_CMD, MB_YESNO, MB_OK, MB_ICONINFORMATION, IDYES};
        use windows::core::PCWSTR;

        let open_url = |url: &str| {
            let u: Vec<u16> = format!("{}\0", url).encode_utf16().collect();
            unsafe { let _ = ShellExecuteW(None, None, PCWSTR(u.as_ptr()), None, None, SHOW_WINDOW_CMD(5)); }
        };

        let msg = |t: &str, b: &str, flags: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE| -> bool {
            let tv: Vec<u16> = format!("{}\0", t).encode_utf16().collect();
            let bv: Vec<u16> = format!("{}\0", b).encode_utf16().collect();
            unsafe { MessageBoxW(None, PCWSTR(bv.as_ptr()), PCWSTR(tv.as_ptr()), flags) == IDYES }
        };

        // 说明
        let _ = msg("安装依赖",
            "VB-CABLE (虚拟音频设备, 必须)\nhttps://vb-audio.com/Cable/index.htm\n\nInterception (键盘驱动, 非必须)\nhttps://github.com/oblitum/Interception\n\n提示: Interception 仅当某些程序模拟按键无法正常使用时才需要安装, 如豆包输入法。",
            MB_OK | MB_ICONINFORMATION);

        // 逐项询问是否打开链接
        if msg("VB-CABLE", "在浏览器中打开 VB-CABLE 下载页?", MB_YESNO | MB_ICONINFORMATION) {
            open_url("https://vb-audio.com/Cable/index.htm");
        }
        if msg("Interception", "在浏览器中打开 Interception 项目页?\n(非必须, 仅当模拟按键异常时需要)", MB_YESNO | MB_ICONINFORMATION) {
            open_url("https://github.com/oblitum/Interception");
        }
    }

    // ---------- 配置持久化 ----------

    fn current_config(&self) -> Config {
        let st = self.state.borrow();
        let hotkey_fwd = self
            .cb_hotkey_fwd.selection()
            .and_then(|i| st.hotkey_items.get(i).cloned())
            .unwrap_or_else(|| "无".to_string());
        let hotkey_bwd = self
            .cb_hotkey_bwd.selection()
            .and_then(|i| st.hotkey_items.get(i).cloned())
            .unwrap_or_else(|| "无".to_string());
        let driver = self
            .cb_driver.selection()
            .and_then(|i| st.driver_items.get(i).cloned())
            .unwrap_or_else(|| "sendinput".to_string());
        Config {
            mode: if st.mode_play { "play".to_string() } else { "cable".to_string() },
            cable_device: self.cb_cable.selection_string().unwrap_or_else(|| st.cable_device.clone()),
            hotkey_forward: if hotkey_fwd == "无" { None } else { Some(hotkey_fwd) },
            hotkey_backward: if hotkey_bwd == "无" { None } else { Some(hotkey_bwd) },
            driver,
            minimize_to_tray: st.minimize_to_tray,
            auto_start_service: st.auto_start_service,
            wired_pids: vec![],
            wireless_pids: vec![],
            autostart: st.autostart_on,
            auto_enter: st.auto_enter,
            auto_enter_mode: st.auto_enter_mode.clone(),
            auto_enter_delay: st.auto_enter_delay,
            typeless_fwd: st.typeless_fwd,
            typeless_bwd: st.typeless_bwd,
        }
    }

    fn persist_config(&self) {
        let _ = self.current_config().save();
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

        // 持久化之前把配置写入 Config 里 (没有运行态借用)
        self.persist_config();
        let cfg = self.current_config();

        // 标记运行态、准备停止信号与日志起始行
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        {
            let mut st = self.state.borrow_mut();
            st.running = true;
            st.stop = Some(stop.clone());
            st.log_lines.push(format!(
                "已启动: 模式={} 前进={} 后退={} 驱动={}",
                cfg.mode,
                cfg.hotkey_forward.as_deref().unwrap_or("无"),
                cfg.hotkey_backward.as_deref().unwrap_or("无"),
                cfg.driver
            ));
        }

        // 刷新 UI (借用仅限此次表达式)
        self.log_box.set_text(&self.state.borrow().log_lines.join("\r\n"));
        self.update_run_state_ui(true);

        let handle = std::thread::spawn(move || {
            let log_tx2 = msg_tx.clone();
            let log = move |m: &str| {
                let _ = log_tx2.send(Msg::Log(m.to_string()));
            };
            let bat_tx = msg_tx.clone();
            let battery_cb = move |pct: u8, charging: bool| {
                let _ = bat_tx.send(Msg::Battery(pct, charging));
            };
            let _ = msg_tx.send(Msg::Running(true));

            let device_name = if cfg.mode == "play" { None } else { Some(cfg.cable_device.as_str()) };
            match AudioOutput::new(device_name, SAMPLE_RATE) {
                Ok(audio) => {
                    log(&format!("音频输出设备: {}", audio.device_name()));
                    let mut sink = |s: &[i16]| audio.push_pcm(s);
                    match Bridge::new(&cfg, &log) {
                        Ok(mut bridge) => {
                            if let Err(e) = bridge.run(&mut sink, &|| audio.diagnostics(), &stop_clone, debug, &log, &battery_cb) {
                                log(&format!("桥接错误: {}", e));
                            }
                        }
                        Err(e) => log(&format!("连接失败: {}", e)),
                    }
                }
                Err(e) => log(&format!("音频设备错误: {}", e)),
            }
            let _ = msg_tx.send(Msg::Running(false));
        });

        self.state.borrow_mut().bridge_thread = Some(handle);
    }

    fn stop_bridge(&self) {
        let (stop, handle) = {
            let mut st = self.state.borrow_mut();
            let stop = st.stop.take();
            let handle = st.bridge_thread.take();
            (stop, handle)
        };
        if let Some(stop) = &stop {
            stop.store(true, Ordering::SeqCst);
        }
        // 等待桥接线程退出 (确保 disconnect/ai_off 执行完毕)
        if let Some(h) = handle {
            let _ = h.join();
        }
        self.state.borrow_mut().running = false;
        self.update_run_state_ui(false);
        self.lbl_battery.set_text("电量: --");
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
    // 全局默认字体: 系统 UI 字体 (默认大小)。控件宽度已针对中文加宽,
    // 字体不改小也能正常显示。
    let _ = nwg::Font::set_global_family("Segoe UI");

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
    // 不在这里调用 set_size — MAIN_WINDOW 标志会让系统使用标准重叠窗口,
    // 不再出现 130x44 异常尺寸。尺寸由 on_min_max 锁死。
    // post_build 立即触发 (填充下拉框/复选框/启动计时器)。
    // post_build 立即触发 (填充下拉框/复选框/启动计时器), 不要等计时器首 tick
    // (计时器同样需要 post_build 启动, 会引发鸡生蛋问题)
    _ui.post_build();
    // _ui 必须保留到 dispatch 结束 (持有 GuiApp 的所有权)。
    nwg::dispatch_thread_events();
}
