# MouseMic (Rust) 构建指南

本目录是用 Rust 对原 Python 版 MouseMic（AJAZZ 语音鼠标的 mSBC→PCM 桥接）的完整重写。
本文记录**已验证可编译/测试/构建**的环境与步骤，并收录编译期踩过的 API 坑，避免重建时重蹈覆辙。

## 1. 先决条件（本机 Windows）

- **Rust 工具链**：`cargo` / `rustc`（本机实测 `cargo 1.97.1 stable-x86_64-pc-windows-msvc`）。
  - cargo 默认不在 `PATH`，每次开新 shell 需：`export PATH="$HOME/.cargo/bin:$PATH"`
    （Windows Git Bash 下；PowerShell 用 `$env:PATH = "$HOME\.cargo\bin;$env:PATH"`）。
- **MSVC 构建工具 + Windows SDK**：安装 Visual Studio Build Tools 勾选
  “使用 C++ 的桌面开发”，以及 Windows SDK（实测 10.0.26100.0）。
  `cpal`(WASAPI) / `windows` crate 需要它。
- **真机硬件**（仅运行/测试需要，非编译需要）：
  - AJAZZ 语音鼠标（VID `0x363C`，音频 HID usage_page `0xFFAA`）。
  - 虚拟声卡（如 VB-CABLE），用于接收桥接出的 PCM。

## 2. 解码器：mini_sbc (纯 Rust, 无外部 DLL)

mSBC 解码现在使用 `mini_sbc = "0.1"` (纯 Rust), 不再依赖 `libsbc.dll`。
- 解码参数: 8 子带 / 15 块 / 16kHz / 单声道
- 每帧: 57 字节 mSBC → 240 字节 s16le PCM (120 样本)
- `mini_sbc` v0.1.7 在极端 SBC 参数下偶发整数溢出 (不影响鼠标的固定 mSBC 参数), 已在测试中 catch。
- 无需外部 DLL, `target/release/MouseMic.exe` 即为完整可发布文件。

## 3. 构建 / 测试命令

```sh
cd rust
# 仅构建无 GUI 的核心库 + CLI（用于快速校验 / 无头运行）
cargo build --release --no-default-features
cargo test  --no-default-features      # 单元测试 + golden 向量测试

# 完整 GUI 版（eframe/egui + tray-icon）
cargo build --release                   # 产物 target/release/MouseMic.exe
cargo test                             # 含 GUI 的测试
```

编译目标：`x86_64-pc-windows-msvc`。

## 4. 运行（无头诊断模式，无需 GUI / 显示器）

```sh
# 列出音频输出设备（虚拟声卡通常叫 "CABLE Input (VB-Audio Virtual Cable)"）
MouseMic.exe --list

# 列出 HID 设备，识别 AJAZZ 鼠标的音频/命令通道
MouseMic.exe --list-hid

# 把真鼠标语音桥接到指定输出设备
MouseMic.exe --cable "CABLE Input"

# 直接默认播放设备
MouseMic.exe --play

# 从已抓取的 mSBC 帧文件回放（调试用）
MouseMic.exe --file capture.bin

# 仅启动桥接并注入热键（无播放）
MouseMic.exe --hotkey
```

无参数运行 `MouseMic.exe` 启动 GUI（eframe 窗口 + 系统托盘）。

## 5. 编译期踩坑记录（已修复，留档备查）

依赖版本：`hidapi 2.6.6` / `cpal 0.15.3` / `windows 0.58` / `egui+eframe 0.27.2` /
`tray-icon 0.19.3 (muda 0.15.3)` / `libloading 0.8.9` / `anyhow`。

### anyhow
- `anyhow!` 宏需 crate 级可用：`lib.rs` 顶部 `#[macro_use] extern crate anyhow;`
  `error.rs` 改为 `pub use anyhow::{Context, Result};`（不再单独再导出 `anyhow`）。

### windows 0.58
- `CreateMutexW` 需要 feature：`Cargo.toml` 的 windows features 加 `"Win32_Security"`。
- `SendInput` 签名是 `SendInput(&[INPUT], i32)`（传 slice，不是 3 个裸参数）。
- `KEYBDINPUT.wVk` 类型是 `VIRTUAL_KEY`，裸 `0` 要写成 `VIRTUAL_KEY(0)`。
- 联合 `INPUT { r#type: INPUT_TYPE, Anonymous: INPUT_0 }`，键盘键用 `Anonymous.ki`。

### hidapi 2.6.6
- 读取超时：`read(&mut buf)` 无超时参数 → 用 `read_timeout(&mut buf, 300)`。
- 按路径打开：`open_path(&CStr)`（不是 `&String`）；用 `CString::new(path)?.as_c_str()`。
- 关闭：无 `close()` 方法，drop 即关闭，删掉 `dev.close()` 调用。
- 阻塞模式：`set_nonblocking(false)` 已删除 → `set_blocking_mode(true)`。
- `interface_number()` 返回 `i32`（不再是 `Option<i32>`）。

### cpal 0.15.3
- 必须 `use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};` 才能调用
  `build_output_stream` / `output_devices` / `default_output_device` / `play`。

### libloading 0.8.9
- `Library::get` 是 `unsafe`，所有 `lib.get::<Sym>(b"...")` 需包在 `unsafe {}` 里。

### egui / eframe 0.27 + tray-icon 0.19
- 视口命令：`send_viewport_command` → `send_viewport_cmd`；
  `ViewportCommand::Show/Hide` → `ViewportCommand::Visible(bool)`。
- 关闭请求：`ctx.input(|i| i.viewport().close_requested())`（闭包，不是 `ctx.input().viewport()`）。
- 禁用输入框：`ui.add_enabled(false, egui::TextEdit::multiline(...))`（TextEdit 无 `.enabled()`）。
- 托盘菜单：`TrayIconBuilder::with_menu(Box::new(menu))`；
  在移动 menu 进 builder 前先 `.id().clone()` 抓出各 MenuItem 的 `MenuId`，
  再在 `set_event_handler` 闭包里用 `event.id == id` 匹配。
- `run_native` 闭包直接 `Box::new(move |cc| Box::new(GuiApp::new(cc, autostart)))`（不是 `Ok(...)` 包裹）。
- `tray_icon.set_visible(true)` 返回 `Result`，用 `let _ =` 消未使用告警。

## 6. 仍待真机验证
- 真机端到端音频试听（`--cable "CABLE Input"` 按真鼠标，确认 VB-CABLE 出声）。
- GUI 窗口 + 托盘真实启动（此前仅验证编译 + 无头 CLI；无显示环境跑不了 GUI）。

## 7. GUI 启动排错（已修复的经验）
- **发布版是纯 GUI 子系统**（`windows_subsystem = "windows"`），双击不再弹黑控制台框。
  调试版（`cargo build`，非 release）仍保留控制台，方便看日志。
- **任何失败都弹红色错误框**：`src/dialog.rs` 的 `install_panic_hook` + `show_error_box`
  （`MessageBoxW`）会把未捕获 panic / `eframe::run_native` 错误 / 单实例拦截都弹窗显示，
  不会再“无声消失”。双击后若没窗口、反而弹红框，把框内文字发回来即可定位。
- **托盘创建失败不再致命**：`build_tray` 返回 `Option`，托盘建不出只记日志，GUI 窗口照常出。
- 渲染后端显式用 `Renderer::Glow`（OpenGL），不依赖 Vulkan/DX12，兼容性更好。若个别机器
  OpenGL 也不可用，红框会报 `No available graphics adapter` 之类，那时再考虑切 `Renderer::Wgpu`。
- 若双击“完全没反应、连红框都没有”：多半是 `libsbc.dll` 没放在 exe 同目录（解码器加载失败会
  在依赖面板显示“缺少”，但不会阻止窗口；真·加载崩溃才弹框）。确认 `target/release/` 下有
  `libsbc.dll` 与 `MouseMic.exe` 并列。

### eframe 隐藏窗口的经典陷阱（已踩并已修）
- **不要在启动时靠 `minimize_to_tray` 自动收托盘**：会导致窗口只渲染一帧就被 `Visible(false)`
  藏掉，表现成“闪一下就没了”。`minimize_to_tray` 默认已改为 `false`，普通启动总是显示窗口；
  仅 `--autostart`（注册表登录）才收进托盘。
- **窗口隐藏后 eframe 不再 tick `update()`**，所以托盘命令若只在 `update()` 里读 mpsc 通道处理，
  就会“显示主窗口”点了没反应。修法：托盘的 `显示主窗口`/`退出` 改用克隆的 `egui::Context`
  （`cc.egui_ctx.clone()`）直接 `send_viewport_cmd(Visible(true)/Close)`，绕过 `update()`，
  隐藏时也能唤回。这是 eframe + tray-icon 的推荐做法。

### 中文显示为方块 (tofu)（已修复）
- **现象**：窗口能出来，但所有中文都是方块/豆腐。
- **根因**：egui 默认字体只含拉丁字形，中文找不到 glyph。
- **修法**：`gui.rs::setup_fonts()` 在 `GuiApp::new` 里调用，从系统字体目录加载含 CJK 字形的字体
  （候选 `msyh.ttc`/`msyhbd.ttc`/`simsun.ttc`/`malgun.ttf`/`simhei.ttf`），用
  `egui::FontData::from_owned(bytes)` 插到 `FontDefinitions.families[Proportional/Monospace]` 最前，
  再 `ctx.set_fonts()` 注册；emoji 等仍走后续默认回退字体，不影响图标。
- **注意**：这依赖本机存在上述中文字体（中文/多语言 Windows 几乎必有 `msyh.ttc`）。若真机完全没有这些
  字体，会 `eprintln!` 告警并退回默认字体（仍方块）。发布时若要绝对保险，可把字体文件随 exe 打包并
  `include_bytes!` 内嵌，彻底摆脱对系统字体的依赖（目前未做，按需再加）。

## 8. 端到端试听排查（按语音键没声音）
GUI 启动后按住鼠标语音键却无声，按优先级排查：
1. **先看 GUI 日志面板**（每按一次键/每秒都会打）：
   - 出现 `音频输出设备: <名>` → 音频设备已成功打开；若显示 `音频设备错误: 找不到输出设备...`
     说明设备名没匹配上（默认 cable 模式找 `CABLE Input`，用 `--list` 或 GUI“列出设备”核对真实名）。
   - 按住语音键后出现 `语音包累计: N (最近音频 Xs 前)` 且 N 在涨 → **HID 数据在流、解码在工作**，
     问题在音频输出/监听；若 N 始终为 0 → 鼠标没在发音频包（ARM 激活失败 / 链路不对 / 键位不对）。
   - `已处理 N 个语音包` 等结尾信息也能佐证。
2. **模式混淆（最常见）**：默认 `mode=cable` 把声音送进 **VB-CABLE 输入端**，**默认不直接出声**；
   要么在会议/录音软件里把 “CABLE Output” 选作麦克风，要么在 Windows 声音设置开启该设备“监听”。
   想直接听到声音做验证，先把输出模式切到 **“扬声器试听”(play)** 再按语音键。
3. **音频缓冲**：`audio.rs` 已用 `BufferSize::Default`（WASAPI 共享模式最稳）；早期曾用
   `Fixed(480)`，部分机器建流成功却完全无声，已改。
4. CLI 直连扬声器验证（绕过 GUI）：`MouseMic.exe --play`，从命令行窗口看日志、按住键听扬声器。
   若 `--play` 有声而 GUI cable 无声 → 纯属第 2 点的监听问题，不是 bug。

### “有语音包但只一下杂音后静音” 的精确定位（日志诊断）
若日志里 `语音包累计` 在涨、却只有按下瞬间一下杂音，说明**收到合法包但解码后续帧失败**
（Rust 的“语音包”计数只证明收到包头匹配的包，不保证解码成功并 push 了 PCM）。新版日志已拆成：
`语音包=N 解码OK=X 解码失败=Y 队列≈Z样本 最近音频Ws前`，按下列分支判断：
- **`解码失败` 持续涨、`解码OK` 几乎停在第一帧** → 确认 mSBC 解码器偶发失步（libsbc 有状态）。
  代码已加“连续 2 次失败即重建解码器”自恢复；若仍稀声（每几帧才成功一次），根因是
  **帧边界系统性错位**，需把逐帧解码改成“字节累积 + 扫描 0xAD 同步字”的 Pump 实现
  （Python 版 `MsbcPump` 也等价，参考 `msbc_decoder.py`）。
- **`解码OK` 持续涨、但 `队列≈0` 且仍静音** → 供给被消费后无新数据，问题在音频输出/流侧
  （非解码）：检查 `音频输出设备` 名是否正确、是否真的在播放（换 `play` 模式、调系统音量）。
- **`队列≈` 持续涨到满（≈32000）** → 消费跟不上（重采样/回调频率异常），检查输出设备采样率
  是否为 44.1k/48k，或 cpal 回调是否被系统节流。
