//! 语音键识别测试: 同时监听音频端点 + 命令端点, 看能否区分两个语音键。
//!
//! 运行: cargo run --bin KeyTest [PID]
//!   默认 PID=0xED03 (AJAZZ AJ200 无线模式常用 PID)。
//!   按 Ctrl+C 退出。

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let target_pid: Option<u16> = std::env::args()
        .nth(1)
        .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).ok());

    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID init: {}", e))?;

    // 搜索所有 usage_page=0xFFAA 设备, 可选按 PID 过滤
    let devices: Vec<_> = api
        .device_list()
        .filter(|d| d.usage_page() == 0xFFAA && target_pid.map_or(true, |pid| d.product_id() == pid))
        .collect();

    if devices.is_empty() {
        // 列出所有 usage_page=0xFFAA 设备帮助诊断
        let all: Vec<_> = api.device_list().filter(|d| d.usage_page() == 0xFFAA).collect();
        if all.is_empty() {
            anyhow::bail!("未找到任何 usage_page=0xFFAA 设备。请确认鼠标已连接(无线接收器或数据线)。");
        }
        println!("找到 {} 个 usage_page=0xFFAA 设备, 但 PID 不匹配:", all.len());
        for d in &all {
            println!(
                "  PID={:#06X}  usage={:#06X}  iface={}  {}",
                d.product_id(), d.usage(), d.interface_number(),
                d.product_string().unwrap_or("?")
            );
        }
        println!("\n请用正确的 PID 重试: cargo run --bin KeyTest -- <PID>");
        println!("例如: cargo run --bin KeyTest -- 0x{:04X}", all[0].product_id());
        return Ok(());
    }

    println!("找到 {} 个匹配设备:", devices.len());
    for d in &devices {
        println!(
            "  path={}  PID={:#06X}  usage={:#06X}  iface={}  {}",
            d.path().to_string_lossy(),
            d.product_id(),
            d.usage(),
            d.interface_number(),
            d.product_string().unwrap_or("?")
        );
    }

    // 尝试打开音频 + 命令端点
    // 通常同一个物理设备有 2 个接口: usage=0x13 (audio) 和 usage=0x01 (cmd)
    // 但不同 PID/固件可能不同, 遍历所有 interface_number 尝试。
    let mut audio = None;
    let mut cmd = None;

    for d in &devices {
        match d.interface_number() {
            0 => audio = Some(d),
            1 => cmd = Some(d),
            _ => {}
        }
    }
    // 兜底: 只找到 1 个时就当只有音频
    if audio.is_none() && devices.len() == 1 {
        audio = Some(&devices[0]);
    }

    let audio_dev = audio.and_then(|d| api.open_path(d.path()).ok());
    let cmd_dev = cmd.and_then(|d| api.open_path(d.path()).ok());

    let audio_path_str = audio.map(|d| d.path().to_string_lossy().to_string()).unwrap_or_default();
    let cmd_path_str = cmd.map(|d| d.path().to_string_lossy().to_string()).unwrap_or_default();

    let Some(audio_dev) = audio_dev else {
        anyhow::bail!("无法打开音频端点");
    };

    println!(
        "已打开: audio={}, cmd={}",
        audio_path_str, cmd_path_str
    );
    println!("准备就绪。按住鼠标语音键, 注意区分两个键。Ctrl+C 退出。\n");

    let mut audio_buf = [0u8; 64];
    let mut cmd_buf = [0u8; 64];
    let mut last_audio = Instant::now();
    let mut in_session = false;
    let mut session_count = 0u32;

    let mut session_pkts = 0u32;
    loop {
        // 读音频 (200ms 超时)
        let audio_n = match audio_dev.read_timeout(&mut audio_buf, 200) {
            Ok(n) => n,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        let now = Instant::now();
        let silence = now - last_audio;

        // 读命令端点 (0 超时 = 非阻塞轮询)
        let cmd_n = cmd_dev
            .as_ref()
            .and_then(|c| c.read_timeout(&mut cmd_buf, 0).ok());

        // 判断音频包是否有效 (report=0xB1, payload_len=57)
        let has_audio = audio_n == 64
            && audio_buf[0] == 0xB1
            && audio_buf[2] == 57;

        if has_audio {
            last_audio = now;
            if !in_session {
                session_count += 1;
                in_session = true;
                session_pkts = 0;
                println!("\n── 语音段 #{} 开始 ({:.3}s 静默) ──", session_count, silence.as_secs_f64());
            }
            if session_pkts < 3 {
                print!("\n  包#{}: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
                    session_pkts, audio_buf[0], audio_buf[1], audio_buf[2], audio_buf[3], audio_buf[4], audio_buf[5]);
                session_pkts += 1;
            }
            print!(".");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }

        // 有新命令数据 → 打印
        if let Some(n) = cmd_n {
            if n > 0 {
                print!("\n  [CMD n={}] ", n);
                for i in 0..n.min(16) {
                    print!("{:02X} ", cmd_buf[i]);
                }
                if n > 16 {
                    print!("...");
                }
                println!();
            }
        }

        // 静默 > 1.5s → 语音段结束, 打印该段统计
        if in_session && silence.as_secs_f64() > 1.5 {
            in_session = false;
            println!("\n── 语音段 #{} 结束 (长度 {:.2}s) ──", session_count, silence.as_secs_f64());
        }
    }
}
