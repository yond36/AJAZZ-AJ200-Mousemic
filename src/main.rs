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
    config::Config, hid, hotkey::HOTKEY_NAMES, bridge::Bridge, single_instance, SAMPLE_RATE,
};

#[derive(Parser)]
#[command(name = "MouseMic", about = "AJAZZ 语音鼠标桥接为系统麦克风 (Rust 重写)")]
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
    /// 联动热键名 (按住语音键 = 按住该键)。可选值见 HOTKEY_NAMES。
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
        return;
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
        let audio =
            mousemic_rs::audio::AudioOutput::new(device_name, SAMPLE_RATE)?;
        log(&format!(
            "输出设备: {}",
            device_name.unwrap_or("(系统默认扬声器)")
        ));
        if let Some(hk) = &cli.hotkey {
            log(&format!("联动热键: {} [{}]", hk, cli.driver));
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
        bridge.run(&mut sink, &|| (0usize, 0usize), &stop, true, &log)?;
    } else if let Some(path) = &cli.file {
        let mut wav = WavWriter::new(path, SAMPLE_RATE, 1)?;
        log(&format!("录音到: {}", path));
        if let Some(hk) = &cli.hotkey {
            log(&format!("联动热键: {} [{}]", hk, cli.driver));
        }
        log("按住鼠标语音键开始说话, Ctrl+C 结束。");
        let stop = install_ctrl_c();
        let mut sink = |s: &[i16]| {
            let _ = wav.write_samples(s);
        };
        let cfg = bridge_config("file", "CABLE Input", &cli.hotkey, &cli.driver);
        let mut bridge = Bridge::new(&cfg, &log)?;
        let r = bridge.run(&mut sink, &|| (0usize, 0usize), &stop, true, &log);
        wav.close()?;
        log(&format!("已保存 {}", path));
        r?;
    }
    Ok(())
}

fn bridge_config(mode: &str, cable_device: &str, hotkey: &Option<String>, driver: &str) -> Config {
    Config {
        mode: mode.to_string(),
        cable_device: cable_device.to_string(),
        hotkey_a: hotkey.clone(),
        hotkey: hotkey.clone(),
        driver: driver.to_string(),
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
    ctrlc::set_handler(move || {
        s.store(true, Ordering::SeqCst);
    })
    .ok(); // 忽略重复安装错误
    stop
}

// ----------------------------------------------------------------------------
// 极简 WAV 写出 (16-bit PCM, 单声道)。避免引入额外依赖。
// ----------------------------------------------------------------------------

use std::io::{Seek, SeekFrom, Write};
use std::fs::File;

struct WavWriter {
    f: File,
    data_len: u32,
}

impl WavWriter {
    fn new(path: &str, _sample_rate: u32, _channels: u16) -> std::io::Result<Self> {
        let mut f = File::create(path)?;
        // 先写占位头 (data_len=0), 关闭时回填真实长度。
        f.write_all(&wav_header(_sample_rate, _channels, 0))?;
        Ok(WavWriter { f, data_len: 0 })
    }

    fn write_samples(&mut self, samples: &[i16]) -> std::io::Result<()> {
        for &s in samples {
            self.f.write_all(&s.to_le_bytes())?;
            self.data_len += 2;
        }
        Ok(())
    }

    fn close(mut self) -> std::io::Result<()> {
        self.f.seek(SeekFrom::Start(0))?;
        self.f.write_all(&wav_header(SAMPLE_RATE, 1, self.data_len))?;
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
