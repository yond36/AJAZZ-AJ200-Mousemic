//! 语音键识别测试: 同时监听音频端点 + 命令端点, 看能否区分两个语音键。
//!
//! 运行: cargo run --bin KeyTest [PID]
//!   不指定 PID 则自动找所有 AJAZZ 设备。
//!   按 Ctrl+C 退出。

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let _target_pid: Option<u16> = std::env::args()
        .nth(1)
        .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).ok());

    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID init: {}", e))?;

    // 用 bridge 的连接逻辑找到并打开音频+命令端点
    let con = mousemic_rs::hid::connect_audio(
        &api,
        &Default::default(),
        &Default::default(),
        None,
        &|s| eprintln!("  {}", s),
    );
    let Some(con) = con else {
        anyhow::bail!("无法连接鼠标音频接口");
    };

    let audio_dev = con.audio;
    let cmd_dev = con.cmd;

    println!(
        "已连接: path={}  PID={:#06X}  {}",
        con.path, con.pid, con.product_string
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
        let cmd_n = cmd_dev.read_timeout(&mut cmd_buf, 0).ok();

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
