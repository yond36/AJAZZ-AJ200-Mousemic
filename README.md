# AJAZZ AJ200 Mousemic

将 AJAZZ 语音鼠标（VID=363C）的蓝牙音频桥接为系统麦克风输入，支持语音实时播放和虚拟麦克风转发。

## 功能

- **扬声器试听模式**：鼠标语音实时通过扬声器/耳机播放
- **虚拟麦克风模式**：桥接到 VB-CABLE 虚拟输入，其他软件可选为麦克风源
- **联动热键**：按住鼠标语音键时自动模拟键盘按键，支持单键和组合键（R_alt+space、R_alt+R_shift、R_alt+R_ctrl 等）
- **仅语音模式**：前进/后退键可独立设为“仅语音”——长按直接触发麦克风，**不注入任何按键**（纯麦克风用法，无需绑定热键；GUI 热键下拉选“仅语音(无热键)”，CLI 不传 `--hotkey` 即默认启用）
- **Typeless 模式**：前进/后退键可独立启用，按住语音键=短按热键开始，松开后音频结束自动再短按热键结束
- **自动发送**：语音结束后延迟自动按 Enter 或 Ctrl+Enter
- **AI 键管理**：启动时自动开启双键 AI，停止/退出时自动关闭
- **热插拔自动切换**：有线/无线链路自动识别、断线重连
- **电池状态显示**：顶部实时显示电量百分比与充电状态（被动上报，不增加鼠标耗电）；电量 ≤10% 或充满时托盘气泡提醒
- **设备识别（PID 白名单）**：仅识别 AJ200 系列已知 PID，检查依赖时显示识别到的鼠标型号、传感器、飞轮及 PID 信息
- **系统托盘**：运行中关闭窗口最小化到托盘，停止状态关闭即退出；支持开机自启
- **调试日志**：勾选后输出语音包/解码/AI 键配置等详细统计，日志框自动滚动到底部
- **原生 GUI**：Win32 原生控件，内存 ~22MB

## 支持设备

AJ200 系列（仅列主要型号，完整列表见 `src/devices.rs`）：

| 型号 | 传感器 | 飞轮 |
|------|--------|------|
| AJ200 NL AI MC | PAW3311 | ❌ |
| AJ200 NL AI PRO+ | PAW3395 | ❌ |
| AJ200P NL AI ULTRA | PAW3950 / PAW3955 | ❌ |
| AJ200P NL AI ULTRA+ | PAW3950 | ✅ |
| AJ200P NL AI ULTRA-3955 | PAW3955 | ✅ |
| AJ200P AI MASTER | PAW3955 | ✅ |
| AJ200P NL AI S ULTRA | PAW3955 | ✅ |

## 系统要求

- Windows 10 / 11
- [VB-CABLE](https://vb-audio.com/Cable/index.htm)（虚拟麦克风模式需要）
- [Interception](https://github.com/oblitum/Interception)（可选，仅当模拟按键在特定程序中失效时需要）

## 下载

从 [Releases](../../releases) 下载最新 `MouseMic.exe`，放到任意目录运行即可。

## 构建

```bash
# 需要 Rust 1.75+ 和 MSVC 编译工具链
git clone https://github.com/yond36/AJAZZ-AJ200-Mousemic.git
cd AJAZZ-AJ200-Mousemic
cargo build --release
```

产物：`target/release/MouseMic.exe`（约 1.3MB）

## 技术细节

- **语言**：Rust（原版 Python/tkinter，Rust 版内存更低、启动更快）
- **音频解码**：mSBC（蓝牙低功耗音频编解码），基于 BlueZ sbc 库静态编译
- **HID 通信**：hidapi，音频接口 usage_page=0xFFAA；电池状态被动上报于 0xFFA0 usage=0x0002（`[0x0A,0x13,...]`，buf[17]=充电标志、buf[18]=电量%）
- **音频输出**：cpal (WASAPI)，16kHz→48kHz 线性重采样
- **热键注入**：SendInput 扫描码 / Interception 内核驱动（可选）
- **GUI**：native-windows-gui（Win32 原生控件 + GDI 系统字体）
- **托盘提醒**：Shell_NotifyIconW 气泡（NIF_INFO）

## License

MIT
