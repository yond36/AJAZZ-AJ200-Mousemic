# MouseMic (Rust) 构建指南

本目录是用 Rust 对原 Python 版 MouseMic（AJAZZ 语音鼠标的 mSBC→PCM 桥接）的完整重写。
本文记录**已验证可编译/测试/构建**的环境与步骤，并收录编译期踩过的 API 坑，避免重建时重蹈覆辙。

当前版本：**v1.4.3**（与 `Cargo.toml` / `Cargo.lock` / git tag 一致）。

## 1. 先决条件（本机 Windows）

- **Rust 工具链**：`cargo` / `rustc`（本机实测 `cargo 1.97.1 stable-x86_64-pc-windows-msvc`）。
  - cargo 默认不在 `PATH`，每次开新 shell 需：`export PATH="$HOME/.cargo/bin:$PATH"`
    （Windows Git Bash 下；PowerShell 用 `$env:PATH = "$HOME\.cargo\bin;$env:PATH"`）。
- **MSVC 构建工具 + Windows SDK**：安装 Visual Studio Build Tools 勾选
  “使用 C++ 的桌面开发”，以及 Windows SDK（实测 10.0.26100.0）。
  `cpal`(WASAPI) / `windows` crate / `cc`(编译 vendored sbc) 都需要它。
- **真机硬件**（仅运行/测试需要，非编译需要）：
  - AJAZZ 语音鼠标（VID `0x363C`，音频 HID usage_page `0xFFAA`）。
  - 虚拟声卡（如 VB-CABLE），用于接收桥接出的 PCM。

## 2. mSBC 解码器：vendored BlueZ sbc（静态编译，无外部 DLL）

mSBC 解码**不再依赖 `libsbc.dll`，也不用 `mini_sbc` crate**。实现改为：

- 源码 vendored 在 `vendor/sbc/`（nefarius/libsbc fork，即 BlueZ 上游 sbc 的 Windows/MSVC 适配版，
  与原 Python 版调用的 `libsbc.dll` 是**同一份 C 源码**，行为逐字节一致）。
- `build.rs` 用 `cc` crate 把 `sbc.c` + `sbc_primitives.c` 静态编译进二进制，
  只编译纯 C 参考实现（不启用 SSE/MMX SIMD，语音解码性能足够）。
- Rust 侧 `src/msbc.rs` 用 `extern "C"` FFI 调用 `sbc_init_msbc` / `sbc_decode` / `sbc_finish`。

解码参数（HFP 标准 mSBC）：8 子带 / 15 块 / 16kHz / 单声道 / 16-bit，
每帧 **57 字节** mSBC（含 0xAD 同步字 + CRC8）→ **240 字节** s16le PCM（120 样本）。

**无需分发任何 DLL**，`target/release/MouseMic.exe` 即为完整可发布文件
（仅当使用 Interception 热键驱动时才需要额外把 `interception.dll` 放到 exe 同目录，见 §5）。

## 3. 构建 / 测试命令

```sh
cd rust
# 仅构建无 GUI 的核心库 + CLI（用于快速校验 / 无头运行）
cargo build --release --no-default-features
cargo test  --no-default-features      # 单元测试 + golden 向量测试

# 完整 GUI 版（native-windows-gui，默认 feature）
cargo build --release                  # 产物 target/release/MouseMic.exe (~1.4MB)
cargo test                             # 含 GUI 的完整测试
```

编译目标：`x86_64-pc-windows-msvc`。

**测试内容**（共 12 个，全部通过）：
- 单元测试 10 个：`devices`（PID 白名单）、`hid`（电池解析）、`msbc`（解码常量/空帧/坏帧）、
  `config`（PID 字符串解析、AI 键启用判定）。
- 集成测试 2 个（`tests/golden.rs`）：
  - `decode_golden_vectors`：用 vendored sbc 解码 `tests/data/msbc_frames.bin`（240 帧，
    含静音/1kHz 正弦/白噪声/混合段），与 `pcm_ref.s16le`（由 `tests/data/gen_vectors.py`
    用 libsbc.dll 生成）**逐字节比对，diff 必须为 0**——sbc 与 libsbc.dll 同源，
    任何字节差异都视为回归。
  - `decoder_constants_sane`：校验 framelen=57 / codesize=240 / 解码器可用。

注：若 `target/release/MouseMic.exe` 正在运行，`cargo build --release` 会因 exe 被占用
报 `failed to remove file ... 拒绝访问 (os error 5)`——先退出运行中的实例再重新构建。

## 4. 运行（无头诊断模式，无需 GUI / 显示器）

```sh
# 列出音频输出设备（虚拟声卡通常叫 "CABLE Input (VB-Audio Virtual Cable)"）
MouseMic.exe --list

# 列出 HID 设备，识别 AJAZZ 鼠标的音频/命令通道/电池接口
MouseMic.exe --list-hid

# 把真鼠标语音桥接到指定虚拟声卡输入（虚拟麦克风模式）
# 不传 --hotkey → 前进/后退键为"仅语音"模式：长按触发麦克风，不注入按键（纯麦克风用法）
MouseMic.exe --cable "CABLE Input"

# 直接播放到系统默认扬声器（扬声器试听模式）
MouseMic.exe --play

# 录音到 WAV 文件（按住语音键录音，Ctrl+C 结束）
MouseMic.exe --file capture.wav

# 联动热键（须与 --play / --cable / --file 搭配；单个 --hotkey 同时作用于前进/后退两键）
MouseMic.exe --cable "CABLE Input" --hotkey R_alt+space
MouseMic.exe --play --hotkey f9 --driver interception

# 开机自启入口：启动 GUI 后自动起桥接并收进托盘（由注册表调用，勿手动运行）
MouseMic.exe --autostart
```

可选参数：
- `--hotkey NAME`：联动热键名，可选 `L_alt R_alt R_ctrl R_shift f9 f10 space grave capslock
  R_alt+space R_alt+R_shift R_alt+R_ctrl`。
- `--driver sendinput|interception`：热键注入方式，默认 `sendinput`；
  `interception` 需要安装 Interception 驱动且把 `interception.dll` 放在 exe 同目录。

> **仅语音模式（v1.4.4+）**：CLI 不传 `--hotkey` 时，前进/后退键都启用 AI 语音
> （长按触发麦克风）但不注入任何按键，`--cable` / `--play` / `--file` 可直接当纯麦克风用。
> GUI 里在对应键的热键下拉选 **“仅语音(无热键)”** 即可对该键单独启用。
> 配置字段：`ai_fwd` / `ai_bwd`（独立于热键绑定的 AI 开关，默认 false 保持旧行为）。

无参数运行 `MouseMic.exe` 启动 GUI（native-windows-gui 原生 Win32 窗口 + 系统托盘）。

## 5. 架构速览（模块划分）

```
src/
├── main.rs          入口：CLI 解析 + headless 桥接 / GUI 启动；内嵌极简 WAV 写入器
├── lib.rs           crate 根：模块声明 + 常量（VID、usage_page、ReportID、16kHz 采样率）
├── hid.rs           HID 枚举、激活握手 (ARM_SEQ)、命令通道自动探测、有线/无线分类、
│                    AI 键配置 (set_ai_keys)、电池上报解析
├── msbc.rs          mSBC 解码器 FFI（vendored sbc，57B 帧 → 240B s16le PCM）
├── audio.rs         cpal/WASAPI 输出 + 线性重采样器（16k → 设备率，默认 f32 路径）
├── bridge.rs        主桥接循环：收帧→解码→输出→热键联动→断线重连→自动回车
├── hotkey.rs        SendInput / Interception 双后端热键注入（组合键、typeless 短按）
├── config.rs        JSON 配置 (mousemic_gui.json，向后兼容旧配置) + 注册表自启；
│                    前进/后退独立 AI 开关 (ai_fwd/ai_bwd，仅语音模式)
├── devices.rs       AJ200 系列 PID 白名单设备注册表（21 个 PID / 22 条记录）
├── gui.rs           native-windows-gui 面板 + 系统托盘（受 gui feature 控制）
├── single_instance.rs  命名互斥量 (Global\mousemic_single) 防多开
├── dialog.rs        MessageBox 错误弹窗 + panic hook（发布版无控制台时兜底）
└── error.rs         anyhow 错误别名

vendor/sbc/          BlueZ sbc 源码（nefarius/libsbc fork），build.rs 静态编译
tests/golden.rs      黄金向量回归测试（与 libsbc.dll 参考逐字节比对）
tests/data/          msbc_frames.bin / pcm_ref.s16le / gen_vectors.py / vectors.json
```

核心数据流：

```
鼠标 HID 音频包 (report 0xB1, 64B, 载荷 57B mSBC)
  → msbc 解码 (240B s16le PCM, 16kHz 单声道)
  → 线性重采样 (16k → 44.1k/48k 设备默认率)
  → cpal/WASAPI 输出 → VB-CABLE 虚拟麦克风 或 默认扬声器
```

## 6. 编译期踩坑记录（已修复，留档备查）

依赖版本：`hidapi 2.6` / `cpal 0.15` / `windows 0.58` / `libloading 0.8` /
`native-windows-gui 1.0` / `clap 4` / `anyhow 1` / `winreg 0.52`。

### anyhow
- `anyhow!` 宏需 crate 级可用：`lib.rs` 顶部 `#[macro_use] extern crate anyhow;`
  `error.rs` 改为 `pub use anyhow::{Context, Result};`（不再单独再导出 `anyhow`）。

### windows 0.58
- `CreateMutexW` 需要 feature：`Cargo.toml` 的 windows features 加 `"Win32_Security"`。
- `SendInput` 签名是 `SendInput(&[INPUT], i32)`（传 slice，不是 3 个裸参数）。
- `KEYBDINPUT.wVk` 类型是 `VIRTUAL_KEY`，裸 `0` 要写成 `VIRTUAL_KEY(0)`。
- 联合 `INPUT { r#type: INPUT_TYPE, Anonymous: INPUT_0 }`，键盘键用 `Anonymous.ki`。
- `windows_subsystem = "windows"`（`main.rs` 顶部 `#![cfg_attr(not(debug_assertions), ...)]`）：
  发布版双击不再弹黑控制台；debug 版保留控制台方便看日志。

### hidapi 2.6
- 读取超时：`read(&mut buf)` 无超时参数 → 用 `read_timeout(&mut buf, 300)`。
- 按路径打开：`open_path(&CStr)`（不是 `&String`）；用 `CString::new(path)?.as_c_str()`。
- 关闭：无 `close()` 方法，drop 即关闭，删掉 `dev.close()` 调用。
- `interface_number()` 返回 `i32`（不再是 `Option<i32>`）。
- `DeviceInfo.path()` 返回 `&CStr`（不是 `&str`），转 String 需 `to_string_lossy()`。

### cpal 0.15
- 必须 `use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};` 才能调用
  `build_output_stream` / `output_devices` / `default_output_device` / `play`。
- WASAPI 共享模式默认混合格式是 f32，输出回调统一走 f32 路径；
  极少数仅支持 i16 的设备退回 i16 回调（`audio.rs::build::<T>` 泛型双路径）。
- 缓冲用 `BufferSize::Fixed(960)`（20ms @48kHz）：`Fixed(480)` 在部分机器上
  建流成功但不触发回调，`Default` 的巨量缓冲又会导致“填充一次后长时间静音”，960 最稳。

### libloading 0.8
- `Library::get` 是 `unsafe`，所有 `lib.get::<Sym>(b"...")` 需包在 `unsafe {}` 里。
- Interception 键盘 stroke 的 `code` 字段是 u16（小端），scancode(u8) 必须先 cast 成
  u16 再 `to_le_bytes()` 写入，否则 1 字节拷给 2 字节切片会 panic（`hotkey.rs`）。

### cc 编译 vendored sbc（build.rs）
- MSVC 下 sbc.c 是 C89 风格，需抑制告警：
  `/wd4244`（int32→int16 截断，sbc 内部已知）、`/wd4267`（size_t→int）、
  `/wd4146`（unsigned 一元负号，CRC 计算）、`/wd4100`（未用形参）、`/wd4456`（变量遮蔽）。
- GCC/Clang 下用 `-Wno-conversion -Wno-unused-parameter -Wno-sign-compare`。
- 不定义 `HAVE_CONFIG_H` → sbc.c 跳过 autotools 的 `<config.h>`。

### native-windows-gui 1.0（GUI）
- 单选按钮（`nwg::RadioButton`）默认无 `WS_GROUP` 互斥，点击“虚拟麦克风”后
  `rb_play` 仍保持勾选、模式实际从未切换。修法：两个单选按钮由 `set_mode()` 显式同步
  （v1.4.3 修复，`gui.rs::set_mode`）。
- 托盘隐藏与桥接启动必须互相独立：只有 `--autostart`（注册表入口）或勾选
  “自动起桥接”才启动服务，收进托盘不再隐含起桥接（v1.4.3 修复）。
- 桥接在独立线程运行（Bridge 持有 HidDevice，不跨线程）；GUI 主线程用 mpsc 通道
  收日志/状态，Win32 计时器（~100ms）轮询刷新 UI；事件回调是 `&self`，
  运行态字段用 `RefCell<AppState>` 提供内部可变性（避免嵌套 `borrow_mut` 死锁，
  见 `gui.rs::start_bridge` 先取值释放借用再处理的写法）。
- 全局默认字体用系统 UI 字体（`nwg::Font::set_global_family("Segoe UI")`），
  GDI 原生控件直接渲染系统字体，**不把字体文件读进内存**，无 egui 的中文 tofu 问题。
- 热键下拉框含特殊项 **“仅语音(无热键)”**（`gui.rs::VOICE_ONLY`）：选中后该键
  配置为 `hotkey=None + ai=true`，实现“长按触发麦克风但不注入按键”。
  下拉选中项 ↔ Config 的双向映射集中在 `gui.rs::split_hotkey_sel`（选中→配置）与
  `pre_build`（配置→选中项）；`current_config` 生成 Config 时经它拆分，
  不要绕开该函数直接比较“无”字符串（旧代码只认“无”/热键名两态）。

## 7. GUI 启动排错（已修复的经验）

- **发布版是纯 GUI 子系统**（`windows_subsystem = "windows"`），双击不再弹黑控制台框。
  调试版（`cargo build`，非 release）仍保留控制台，方便看日志。
- **任何失败都弹红色错误框**：`src/dialog.rs` 的 `install_panic_hook` + `show_error_box`
  （`MessageBoxW`）会把未捕获 panic / GUI 初始化失败 / 单实例拦截都弹窗显示，
  不会再“无声消失”。双击后若没窗口、反而弹红框，把框内文字发回来即可定位。
- **托盘创建失败不再致命**：托盘建不出只记日志，GUI 窗口照常出。
- **单实例**：第二个实例会被 `Global\mousemic_single` 互斥量拦下，并尝试
  `FindWindowW` 激活已有窗口（找不到窗口——如已最小化到托盘——则什么都不做）。
- **开机自启**：注册表 `HKCU\...\CurrentVersion\Run` 写 `"exe路径" --autostart`；
  `--autostart` 入口启动后直接起桥接并收进托盘，普通启动总是显示窗口
  （不要默认 `minimize_to_tray`，会导致“闪一下就没了”）。

## 8. 端到端试听排查（按语音键没声音）

GUI 启动后按住鼠标语音键却无声，按优先级排查：
1. **先看 GUI 日志面板**（每按一次键/每秒都会打）：
   - 出现 `音频输出设备: <名>` → 音频设备已成功打开；若显示 `音频设备错误: 找不到输出设备...`
     说明设备名没匹配上（默认 cable 模式找 `CABLE Input`，用 `--list` 或 GUI“列出设备”核对真实名）。
   - 按住语音键后出现 `语音包=... 解码OK=... 失败=... 队列≈...` 且 `语音包` 在涨 →
      **HID 数据在流、解码在工作**，问题在音频输出/监听；若 `语音包` 始终为 0 →
     鼠标没在发音频包（ARM 激活失败 / 链路不对 / 键位不对）。
   - `已处理 N 个语音包` 等结尾信息也能佐证。
2. **模式混淆（最常见）**：默认 `mode=cable` 把声音送进 **VB-CABLE 输入端**，**默认不直接出声**；
   要么在会议/录音软件里把 “CABLE Output” 选作麦克风，要么在 Windows 声音设置开启该设备“监听”。
   想直接听到声音做验证，先把输出模式切到 **“扬声器试听”(play)** 再按语音键。
3. **CLI 直连扬声器验证（绕过 GUI）**：`MouseMic.exe --play`，从命令行窗口看日志、按住键听扬声器。
   若 `--play` 有声而 GUI cable 无声 → 纯属第 2 点的监听问题，不是 bug。

### “有语音包但只一下杂音后静音” 的精确定位（日志诊断）

若日志里 `语音包` 在涨、却只有按下瞬间一下杂音，说明**收到合法包但解码后续帧失败**。
新版日志已拆成：`语音包=N 解码OK=X 解码失败=Y 队列≈Z样本 最近音频Ws前`，按下列分支判断：

- **`解码失败` 持续涨、`解码OK` 几乎停在第一帧** → mSBC 解码器失步（sbc 是有状态解码器）。
  代码已加“连续 2 次失败即重建解码器”自恢复（`bridge.rs`，重建后恢复帧同步）；
  若仍稀声（每几帧才成功一次），根因是**帧边界系统性错位**，需把逐帧解码改成
  “字节累积 + 扫描 0xAD 同步字”的 Pump 实现（参考 Python 版 `msbc_decoder.py`）。
- **`解码OK` 持续涨、但 `队列≈0` 且仍静音** → 供给被消费后无新数据，问题在音频输出/流侧
  （非解码）：检查 `音频输出设备` 名是否正确、是否真的在播放（换 `play` 模式、调系统音量）。
- **`队列≈` 持续涨到满（≈32000）** → 消费跟不上（重采样/回调频率异常），检查输出设备采样率
  是否为 44.1k/48k，或 cpal 回调是否被系统节流。

## 9. 发布

- `cargo build --release` 产物 `target/release/MouseMic.exe`（约 1.4MB，静态链接 sbc，无 DLL 依赖）。
- 发布包 = exe + 可选 `interception.dll`（仅用 Interception 驱动时）+
  `RELEASE_NOTES_v*.md`（版本说明，`release/` 目录有历史归档）。
- 配置在 exe 同目录的 `mousemic_gui.json`（自动生成，与 Python 版 schema 兼容）。
