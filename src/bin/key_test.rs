//! 语音键识别测试 v2: 同时轮询音频(EP82) + 命令(EP81)端点。
//!
//! 运行: cargo run --bin KeyTest
//!   按 Ctrl+C 退出。

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID init: {}", e))?;

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
    println!("准备就绪。按语音键, 注意区分两个键。Ctrl+C 退出。\n");

    let mut audio_buf = [0u8; 64];
    let mut cmd_buf = [0u8; 64];
    let mut last_audio = Instant::now();
    let mut in_session = false;
    let mut session_count = 0u32;
    let mut session_pkts = 0u32;
    let start = Instant::now();

    loop {
        // 读音频 (100ms 超时)
        let audio_n = audio_dev.read_timeout(&mut audio_buf, 100).ok();

        // 读命令端点 (50ms 超时, 确保能捕获)
        let cmd_n = cmd_dev.read_timeout(&mut cmd_buf, 50).ok();

        let now = Instant::now();
        let silence = now - last_audio;

        // 音频包
        if let Some(64) = audio_n {
            if audio_buf[0] == 0xB1 && audio_buf[2] == 57 {
                if !in_session {
                    session_count += 1;
                    in_session = true;
                    session_pkts = 0;
                    println!("\n── 语音段 #{} 开始 ({:.3}s 静默, t={:.3}s) ──",
                        session_count, silence.as_secs_f64(), now.duration_since(start).as_secs_f64());
                }
                if session_pkts < 2 {
                    print!("  AUDIO[{}]: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}\n",
                        session_pkts,
                        audio_buf[0], audio_buf[1], audio_buf[2], audio_buf[3],
                        audio_buf[4], audio_buf[5], audio_buf[6], audio_buf[7]);
                    session_pkts += 1;
                }
                print!(".");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                last_audio = now;
            }
        }

        // 命令端点数据 — 每次有数据都打印
        if let Some(n) = cmd_n {
            if n > 0 && n <= 64 {
                let t = now.duration_since(start).as_secs_f64();
                print!("\n  [CMD t={:.3}s n={}] ", t, n);
                for i in 0..n.min(32) {
                    print!("{:02X} ", cmd_buf[i]);
                }
                println!();
            }
        }

        // 静默 > 2s → 语音段结束
        if in_session && silence.as_secs_f64() > 2.0 {
            in_session = false;
            println!("\n── 语音段 #{} 结束 ──", session_count);
        }
    }
}
