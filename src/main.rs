//! MouseMic-RS 图形应用入口。
//!
//! 普通启动显示 GUI；开机自启 (`--autostart`) 自动启动桥接并收进托盘。

// 发布版 (release) 设为 Windows GUI 子系统: 双击不再弹出黑控制台窗口。
// 调试版 (debug) 仍保留控制台, 方便看日志。错误通过 dialog 模块的 MessageBox 弹出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 任何未捕获 panic 都弹窗, 而不是在发布版(无控制台)里“无声消失”。
    mousemic_rs::dialog::install_panic_hook();

    let mut autostart = false;
    for arg in std::env::args_os().skip(1) {
        if arg == "--autostart" {
            autostart = true;
        } else {
            mousemic_rs::dialog::show_error_box(&format!(
                "不支持的启动参数: {}\n请直接打开 MouseMic 使用图形界面。",
                arg.to_string_lossy()
            ));
            std::process::exit(2);
        }
    }

    // 核心模块的诊断日志输出到调试控制台; GUI 使用自身日志框。
    simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .ok();

    mousemic_rs::gui::run(autostart);
}
