# AJAZZ AJ200 Mousemic

将 AJAZZ 语音鼠标（VID=363C）的蓝牙音频桥接为系统麦克风输入，支持语音实时播放和虚拟麦克风转发。

## 功能

- **扬声器试听模式**：鼠标语音实时通过扬声器/耳机播放
- **虚拟麦克风模式**：桥接到 VB-CABLE 虚拟输入，其他软件可选为麦克风源
- **联动热键**：按住鼠标语音键时自动模拟键盘按键，支持单键和组合键（R_alt+space、R_alt+R_shift、R_alt+R_ctrl 等）
- **Typeless 模式**：前进/后退键可独立启用，按住语音键=短按热键开始，松开后音频结束自动再短按热键结束
- **自动发送**：语音结束后延迟自动按 Enter 或 Ctrl+Enter
- **AI 键管理**：启动时自动开启双键 AI，停止/退出时自动关闭
- **热插拔自动切换**：有线/无线链路自动识别、断线重连
- **系统托盘**：最小化到托盘，开机自启
- **原生 GUI**：Win32 原生控件，内存 ~22MB

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
- **HID 通信**：hidapi，usage_page=0xFFAA
- **音频输出**：cpal (WASAPI)，16kHz→48kHz 线性重采样
- **热键注入**：SendInput 扫描码 / Interception 内核驱动（可选）
- **GUI**：native-windows-gui（Win32 原生控件 + GDI 系统字体）

## License

MIT
