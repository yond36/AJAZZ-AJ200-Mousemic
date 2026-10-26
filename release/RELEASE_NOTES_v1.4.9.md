## v1.4.9 — MouseMic 专属图标

- 新图标将鼠标轮廓与麦克风支架结合，沿用界面的蓝白配色。支持 16、24、32、48、64、128、256 像素。
- 图标嵌入 Windows x64 的 `MouseMic.exe`，同时用于资源管理器中的程序文件、窗口标题栏、任务栏和系统托盘；运行时无需额外的图标文件。
- 仓库新增可编辑的 SVG、512 像素 PNG 预览、多尺寸 ICO 和 Pillow 图标生成脚本。Windows GUI 构建通过 `winresource` 编译图标资源；无 GUI 构建不嵌入图标。
- 本次不改变音频、HID、热键、配置格式和原有界面布局。升级时退出旧程序，仅替换 exe，并保留 `mousemic_gui.json`。

### 验证
- 35 项默认特性单元测试、2 项集成测试；29 项无 GUI 单元测试、2 项集成测试全部通过。
- GUI 与无 GUI Clippy 无警告；Windows x64 GUI 与无 GUI 构建通过。
- 校验 EXE 图标资源组 ID 1、7 个 ICO 尺寸和实际窗口 `ICON_SMALL` / `ICON_BIG`，程序正常退出。

### 下载
- `MouseMic.exe`：Windows x64 程序（图标已内嵌）。
- `interception.dll`：仅选择 Interception 注入方式时才需放在 exe 同目录，文件与上一版相同。
- `SHA256SUMS.txt`：以上附件的 SHA-256 校验值。
