//! MouseMic-RS 可执行入口。
//!
//! 行为 (单一二进制, 兼容 Python 的 mousemic.py + mousemic_gui.py):
//! - 带 `--list` / `--list-hid` / `--play` / `--cable <NAME>` / `--file <WAV>` 时, 以无界面
//!   (headless) 模式运行桥接 (按住鼠标语音键说话, Ctrl+C 退出)。
//! - 不带这些参数时, 启动图形界面 (GUI feature 开启时); 开机自启 (`--autostart`) 会直接起桥接并收进托盘。
//!
//! 单一二进制 = 双击打开 GUI, 命令行带参数则走 CLI。

// 发布版 (release) 设为 Windows GUI 子系统: 双击不再弹出黑控制台窗口。
// 调试版 (debug) 仍保留控制台, 方便看日志。错误通过 dialog 模块的 MessageBox 弹出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait};
use mousemic_rs::{
    bridge::Bridge, config::Config, hid, hotkey::HOTKEY_NAMES, single_instance, SAMPLE_RATE,
};

#[derive(Parser)]
#[command(
    name = "MouseMic",
    about = "AJAZZ 语音鼠标桥接为系统麦克风 (Rust 重写)"
)]
struct Cli {
    /// 列出可用音频输出设备。
    #[arg(long)]
    list: bool,
    /// 列出全部 HID 设备 (排错: 插线后看鼠标真实身份)。
    #[arg(long)]
    list_hid: bool,
    /// 扬声器试听: 按住语音键直接播放到默认输出。
    #[arg(long)]
    play: bool,
    /// 转发到虚拟麦克风 (VB-CABLE): 参数为虚拟声卡输入设备名, 如 "CABLE Input"。
    #[arg(long, value_name = "NAME")]
    cable: Option<String>,
    /// 录音到 WAV 文件 (按住语音键录音, Ctrl+C 结束)。
    #[arg(long, value_name = "WAV")]
    file: Option<String>,
    /// 联动热键名 (按住语音键 = 按住该键, 同时作用于前进/后退键)。可选值见 HOTKEY_NAMES;
    /// 不指定则两键为"仅语音"模式(长按触发麦克风, 不注入按键)。
    #[arg(long, value_name = "NAME")]
    hotkey: Option<String>,
    /// 热键注入方式: sendinput (默认) 或 interception。
    #[arg(long, default_value = "sendinput")]
    driver: String,
    /// 开机自启入口: 启动 GUI 后自动起桥接并收进托盘 (由注册表调用)。
    #[arg(long)]
    autostart: bool,
}

fn main() {
    // 任何未捕获 panic 都弹窗, 而不是在发布版(无控制台)里“无声消失”。
    mousemic_rs::dialog::install_panic_hook();

    let cli = Cli::parse();

    // 初始化日志 (同时给到文件? 这里仅控制台; GUI 用自身日志框)
    simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .ok();

    // 发布版是 Windows GUI 子系统 (双击不弹黑窗), 但从控制台启动 CLI 时必须
    // 重新挂回父控制台, 否则 stdout/stderr 丢失、Ctrl+C 无法送达 (WAV 模式将
    // 无法干净收尾)。双击启动 (无父控制台) 时 AttachConsole 失败, 无副作用。
    let cli_mode =
        cli.list || cli.list_hid || cli.play || cli.cable.is_some() || cli.file.is_some();
    if cli_mode {
        attach_parent_console();
    }

    if cli.list {
        list_devices();
        return;
    }
    if cli.list_hid {
        hid::list_hid(&|m: &str| println!("{}", m));
        return;
    }

    // headless 模式: 必须三者其一
    let headless = cli.play || cli.cable.is_some() || cli.file.is_some();
    if headless {
        if let Err(e) = run_headless(&cli) {
            eprintln!("错误: {}", e);
            std::process::exit(1);
        }
        return;
    }

    // 否则启动 GUI
    #[cfg(feature = "gui")]
    {
        mousemic_rs::gui::run(cli.autostart);
    }
    #[cfg(not(feature = "gui"))]
    {
        eprintln!("未指定 --list/--play/--cable/--file, 且 GUI 未启用 (--no-default-features)。");
        eprintln!("可用: --list | --play | --cable NAME | --file WAV");
        std::process::exit(2);
    }
}

/// 无界面桥接: 创建输出 (声卡或 WAV), 跑桥接循环, Ctrl+C 干净退出。
fn run_headless(cli: &Cli) -> anyhow::Result<()> {
    if !single_instance::try_acquire() {
        single_instance::bring_existing_to_front();
        return Err(anyhow::anyhow!("mousemic 已在运行中, 已尝试激活已有窗口。"));
    }

    // 校验热键名
    if let Some(hk) = &cli.hotkey {
        if !HOTKEY_NAMES.contains(&hk.as_str()) {
            return Err(anyhow::anyhow!(
                "未知热键名: {} (可选: {:?})",
                hk,
                HOTKEY_NAMES
            ));
        }
    }

    let log = |m: &str| println!("{}", m);

    // 输出 sink + 收尾
    if cli.play || cli.cable.is_some() {
        let device_name = if cli.play {
            None
        } else {
            Some(cli.cable.as_ref().unwrap().as_str())
        };
        let audio = mousemic_rs::audio::AudioOutput::new(device_name, SAMPLE_RATE)?;
        log(&format!(
            "输出设备: {}",
            device_name.unwrap_or("(系统默认扬声器)")
        ));
        if let Some(hk) = &cli.hotkey {
            log(&format!("联动热键: {} [{}]", hk, cli.driver));
        } else {
            log("前进/后退键: 仅语音模式(长按触发麦克风, 不注入按键)");
        }
        log("按住鼠标语音键开始说话, Ctrl+C 退出。");
        let stop = install_ctrl_c();
        let mut sink = |s: &[i16]| audio.push_pcm(s);
        let cfg = bridge_config(
            if cli.play { "play" } else { "cable" },
            cli.cable.as_deref().unwrap_or("CABLE Input"),
            &cli.hotkey,
            &cli.driver,
        );
        let mut bridge = Bridge::new(&cfg, &log)?;
        let noop_battery = |_: u8, _: bool| {};
        bridge.run(
            &mut sink,
            &|| (0usize, 0usize),
            &stop,
            true,
            &log,
            &noop_battery,
        )?;
    } else if let Some(path) = &cli.file {
        if let Some(hk) = &cli.hotkey {
            log(&format!("联动热键: {} [{}]", hk, cli.driver));
        }
        log("按住鼠标语音键录音, Ctrl+C 结束。");
        let stop = install_ctrl_c();
        let cfg = bridge_config("file", "CABLE Input", &cli.hotkey, &cli.driver);
        let mut bridge = Bridge::new(&cfg, &log)?;
        // 桥接初始化成功后才创建文件, 且绝不覆盖已有录音 (create_new)。
        let wav = Arc::new(std::sync::Mutex::new(WavWriter::new(path, SAMPLE_RATE, 1)?));
        log(&format!("录音到: {}", path));
        let write_failed = Arc::new(AtomicBool::new(false));
        let sink = {
            let wav = wav.clone();
            let write_failed = write_failed.clone();
            let stop_flag = stop.clone();
            move |s: &[i16]| {
                if write_failed.load(Ordering::SeqCst) {
                    return;
                }
                let mut w = wav.lock().unwrap();
                if let Err(e) = w.write_samples(s) {
                    write_failed.store(true, Ordering::SeqCst);
                    // 磁盘满/设备拔出: 立刻停止桥接, 收尾仍会回填已写数据的头。
                    stop_flag.store(true, Ordering::SeqCst);
                    eprintln!("WAV 写入失败: {}", e);
                }
            }
        };
        let mut sink = sink;
        let noop_battery = |_: u8, _: bool| {};
        let r = bridge.run(
            &mut sink,
            &|| (0usize, 0usize),
            &stop,
            true,
            &log,
            &noop_battery,
        );
        // 无论桥接结果如何都完成 WAV 收尾 (回填 data_len), 再汇报各自错误。
        let wav_close = match Arc::try_unwrap(wav) {
            Ok(mutex) => match mutex.into_inner() {
                Ok(w) => w
                    .close()
                    .map_err(|e| anyhow::anyhow!("WAV 收尾失败: {}", e)),
                Err(_) => Err(anyhow::anyhow!("WAV 写入端未释放")),
            },
            Err(_) => Err(anyhow::anyhow!("WAV 写入端未释放")),
        };
        if let Err(e) = &wav_close {
            eprintln!("错误: {}", e);
        } else if !write_failed.load(Ordering::SeqCst) {
            log(&format!("已保存 {}", path));
        }
        r?;
        wav_close?;
    }
    Ok(())
}

/// 发布版 (GUI 子系统) 从控制台启动 CLI 时挂回父控制台, 恢复 stdout/stderr
/// 与 Ctrl+C 信号。双击启动时无父控制台, 静默跳过。
#[cfg(windows)]
fn attach_parent_console() {
    use std::sync::atomic::AtomicBool;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_CREATION_DISPOSITION, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    static ATTACHED: AtomicBool = AtomicBool::new(false);
    if ATTACHED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        let conout: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let Ok(console) = CreateFileW(
            PCWSTR(conout.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            None,
            FILE_CREATION_DISPOSITION(OPEN_EXISTING.0),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) else {
            return;
        };
        let inherited_stdout = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
        let inherited_stderr = GetStdHandle(STD_ERROR_HANDLE).unwrap_or_default();
        // 启动时已带有效 std 句柄 (重定向到管道/文件) 的保持不动;
        // 只有交互式控制台启动 (GUI 子系统拿到的是无效句柄) 才指向 CONOUT$。
        let mut took_console = false;
        if inherited_stdout.is_invalid() {
            took_console |= SetStdHandle(STD_OUTPUT_HANDLE, console).is_ok();
        }
        if inherited_stderr.is_invalid() {
            took_console |= SetStdHandle(STD_ERROR_HANDLE, console).is_ok();
        }
        if !took_console {
            let _ = CloseHandle(console);
        }
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn bridge_config(mode: &str, cable_device: &str, hotkey: &Option<String>, driver: &str) -> Config {
    let hotkey_forward = hotkey.clone();
    let hotkey_backward = hotkey.clone();
    // 未指定 --hotkey → 两键"仅语音": 启用 AI 长按触发麦克风, 但不注入任何按键;
    // 指定了 → 联动模式(绑定热键隐式启用 AI)。
    let voice_only = hotkey.is_none();
    Config {
        mode: mode.to_string(),
        cable_device: cable_device.to_string(),
        hotkey_forward,
        hotkey_backward,
        driver: driver.to_string(),
        ai_fwd: voice_only,
        ai_bwd: voice_only,
        ..Config::default()
    }
}

/// 列出 cpal 输出设备 (移植自 Python list_devices)。
fn list_devices() {
    let host = cpal::default_host();
    println!("可用音频输出设备:");
    match host.output_devices() {
        Ok(devs) => {
            for (i, d) in devs.enumerate() {
                if let Ok(name) = d.name() {
                    let rate = d
                        .default_output_config()
                        .map(|c| c.sample_rate().0)
                        .unwrap_or(0);
                    println!("  [{}] {}  (rate={})", i, name, rate);
                }
            }
        }
        Err(e) => println!("枚举设备失败: {}", e),
    }
}

/// 安装 Ctrl+C 处理器, 返回可被桥接循环轮询的停止标志。
fn install_ctrl_c() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        s.store(true, Ordering::SeqCst);
    }) {
        // 注册失败 (如重复安装) 时提示; 桥接仍可运行, 但需结束进程强退。
        eprintln!("警告: Ctrl+C 处理注册失败 ({})", e);
    }
    stop
}

// ----------------------------------------------------------------------------
// 极简 WAV 写出 (16-bit PCM, 单声道)。避免引入额外依赖。
// ----------------------------------------------------------------------------

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

struct WavWriter {
    f: File,
    data_len: u32,
    sample_rate: u32,
    channels: u16,
}

impl WavWriter {
    fn new(path: &str, sample_rate: u32, channels: u16) -> std::io::Result<Self> {
        // create_new: 已存在的文件报错而不是截断——绝不覆盖用户此前的录音。
        let mut f = File::options().write(true).create_new(true).open(path)?;
        // 先写占位头 (data_len=0), 关闭时回填真实长度。
        f.write_all(&wav_header(sample_rate, channels, 0))?;
        Ok(WavWriter {
            f,
            data_len: 0,
            sample_rate,
            channels,
        })
    }

    fn write_samples(&mut self, samples: &[i16]) -> std::io::Result<()> {
        // RIFF 尺寸字段是 u32; 4GB 之后写入必然产生非法文件, 直接报错停录。
        let bytes = samples
            .len()
            .checked_mul(2)
            .filter(|bytes| self.data_len as u64 + *bytes as u64 <= u32::MAX as u64)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "WAV 超过 4GB 上限")
            })?;
        self.data_len += bytes as u32;
        for &s in samples {
            self.f.write_all(&s.to_le_bytes())?;
        }
        Ok(())
    }

    fn close(mut self) -> std::io::Result<()> {
        self.f.seek(SeekFrom::Start(0))?;
        self.f
            .write_all(&wav_header(self.sample_rate, self.channels, self.data_len))?;
        self.f.flush()
    }
}

fn wav_header(sample_rate: u32, channels: u16, data_len: u32) -> [u8; 44] {
    let byte_rate = sample_rate * channels as u32 * 2; // 16-bit
    let block_align = channels * 2;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}
