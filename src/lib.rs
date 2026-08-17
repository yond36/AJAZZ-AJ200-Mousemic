//! MouseMic-RS — AJAZZ 语音鼠标桥接器的 Rust 重写版。
//!
//! 把鼠标 HID 包里的 mSBC 帧 (57 字节/帧, 16kHz 单声道) 解码为 PCM, 经虚拟声卡
//! (如 VB-CABLE) 输出为系统"麦克风", 供会议/输入法软件使用。
//!
//! 模块划分:
//! - [`hid`]: HID 设备枚举、激活握手 (ARM_SEQ)、命令通道自动探测、链路分类/切换。
//! - [`msbc`]: mSBC 解码 (vendored BlueZ sbc, 57 字节帧 -> 240 字节 s16le PCM, 静态编译无需 DLL)。
//! - [`audio`]: cpal/WASAPI 输出到虚拟声卡 (含 16k->设备率 重采样)。
//! - [`hotkey`]: SendInput/Interception 联动热键注入。
//! - [`bridge`]: 主桥接循环 (收帧->解码->输出->热键->断线重连)。
//! - [`config`]: JSON 配置 + 注册表自启。
//! - [`single_instance`]: 命名互斥量, 防多开。

// 让 `anyhow!` 宏在整个 crate 内可用 (各模块无需单独 import)。
#[macro_use]
extern crate anyhow;

pub mod audio;
pub mod bridge;
pub mod config;
pub mod devices;
pub mod dialog;
pub mod error;
pub mod hid;
pub mod hotkey;
pub mod msbc;
pub mod single_instance;

#[cfg(feature = "gui")]
pub mod gui;

/// 鼠标厂商/音频常量 (与 Python 版保持一致)。
pub const VID: u16 = 0x363C;
/// 主窗口标题 (含版本号; GUI 标题栏与单实例窗口查找共用同一来源)。
pub const WINDOW_TITLE: &str = concat!("AJAZZ 语音鼠标桥接器 v", env!("CARGO_PKG_VERSION"));
/// 音频输入接口 usage_page (Col07)。
pub const AUDIO_USAGE_PAGE: u16 = 0xFFAA;
/// 命令通道 usage_page 候选 (同系列不同 PID 鼠标可能不同)。
pub const CMD_USAGE_PAGES: [u16; 3] = [0xFFA0, 0xFFB1, 0xFFDF];
/// 音频中断包 Report ID。
pub const REPORT_ID: u8 = 0xB1;
/// 音频采样率。
pub const SAMPLE_RATE: u32 = 16000;
/// 音频帧载荷长度字段值 (rep[2] == 0x39 == 57)。
pub const AUDIO_PAYLOAD_LEN: u8 = 0x39;
/// 语音键松开判定: 音频流消失多久后认为松手 (固件有约 0.8s 拖尾)。
pub const HOTKEY_IDLE_TIMEOUT: f64 = 0.6;
