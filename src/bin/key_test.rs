//! AI 键区分测试 v4: 验证独立按键配置是否生效。
//!
//! 阶段1: 两个键都禁用AI → 按任何键都不应有音频
//! 阶段2: 仅键1=AI → 只有键1有音频
//! 阶段3: 仅键2=AI → 只有键2有音频
//! 阶段4: 两个键都AI → 两个键都有音频

use std::time::Instant;

fn send_command(control: &hidapi::HidDevice, byte2: u8, byte3: u8, byte14: u8, byte15: u8, byte17: u8, byte18: u8) -> bool {
    let mut report = [0u8; 64];
    report[0] = 0x0b;
    report[1] = 0x55;
    report[2] = 0x1a;
    report[3] = byte2;
    report[4] = byte3;
    report[5] = 0x01;
    let payload: [u8; 22] = [0,0, 0x02,0,0, 0x04,0,0, 0x08,byte14,byte15, 0x10,byte17,byte18, 0x20,0,0, 0x40,0,0, 0x80,0];
    for (i, b) in payload.iter().enumerate() { report[6 + i] = *b; }
    if control.write(&report).unwrap_or(0) != 64 { return false; }
    std::thread::sleep(std::time::Duration::from_millis(50));
    report[4] = 0x01;
    control.write(&report).unwrap_or(0) == 64
}

fn main() -> anyhow::Result<()> {
    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID: {}", e))?;
    let con = mousemic_rs::hid::connect_audio(&api, &Default::default(), &Default::default(), None, &|s| eprintln!("  {}", s));
    let Some(con) = con else { anyhow::bail!("无法连接"); };

    let audio_dev = con.audio;
    let cmd_dev = con.cmd;

    println!("PID={:#06X} {}", con.pid, con.product_string);

    let control = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0xffa0 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());

    let Some(ref ctrl) = control else {
        println!("未找到控制端点, 无法配置");
        return Ok(());
    };

    // 阶段1: 两个键都禁用
    println!("\n=== 阶段1: 两个键都禁用AI (byte2=0x00, byte3=0x00) ===");
    println!("预期: 按任何键都不应有音频");
    send_command(ctrl, 0x00, 0x00, 0x01, 0x02, 0x01, 0x02);
    read_loop(&audio_dev, &cmd_dev, 15, "阶段1");

    // 阶段2: 仅键1=AI
    println!("\n=== 阶段2: 仅键1(前进键)=AI (byte2=0x10, byte3=0x01) ===");
    println!("预期: 按键1有音频, 按键2无音频");
    send_command(ctrl, 0x10, 0x01, 0x00, 0x00, 0x01, 0x02);
    read_loop(&audio_dev, &cmd_dev, 15, "阶段2");

    // 阶段3: 仅键2=AI
    println!("\n=== 阶段3: 仅键2(后退键)=AI (byte2=0x08, byte3=0x01) ===");
    println!("预期: 按键2有音频, 按键1无音频");
    send_command(ctrl, 0x08, 0x01, 0x01, 0x02, 0x00, 0x00);
    read_loop(&audio_dev, &cmd_dev, 15, "阶段3");

    // 阶段4: 两个键都AI
    println!("\n=== 阶段4: 两个键都AI (byte2=0x18, byte3=0x01) ===");
    println!("预期: 两个键都有音频");
    send_command(ctrl, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00);
    read_loop(&audio_dev, &cmd_dev, 15, "阶段4");

    println!("\n完成。");
    Ok(())
}

fn read_loop(audio: &hidapi::HidDevice, cmd: &hidapi::HidDevice, duration_secs: u64, label: &str) {
    let mut audio_buf = [0u8; 64];
    let mut cmd_buf = [0u8; 64];
    let start = Instant::now();
    let deadline = start + std::time::Duration::from_secs(duration_secs);
    let mut audio_count = 0u32;

    while Instant::now() < deadline {
        if let Ok(64) = audio.read_timeout(&mut audio_buf, 50) {
            if audio_buf[0] == 0xB1 && audio_buf[2] == 57 {
                audio_count += 1;
                if audio_count == 1 {
                    let elapsed = Instant::now().duration_since(start).as_secs_f64();
                    print!("\n  [音频开始 t={:.1}s] ", elapsed);
                }
                print!(".");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
        if let Ok(n) = cmd.read_timeout(&mut cmd_buf, 10) {
            if n > 0 {
                print!("\n  [CMD n={}] ", n);
                for i in 0..n.min(16) { print!("{:02X} ", cmd_buf[i]); }
                println!();
            }
        }
    }
    if audio_count > 0 {
        println!("\n  {}: 收到 {} 个音频包", label, audio_count);
    } else {
        println!("\n  {}: 无音频 ✓", label);
    }
}
