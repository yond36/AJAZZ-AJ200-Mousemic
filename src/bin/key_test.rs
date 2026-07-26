//! AI 键区分测试 v3: 利用驱动协议配置两个键不同模式。
//!
//! 策略: 键1(前进)=AI自定义, 键2(后退)=默认行为
//! 然后看 EP81 是否有不同的按键事件。
//!
//! 运行: cargo run --bin KeyTest

use std::time::Instant;

fn send_command(control: &hidapi::HidDevice, byte2: u8, byte3: u8, byte14: u8, byte15: u8, byte17: u8, byte18: u8) -> bool {
    let mut report = [0u8; 64];
    report[0] = 0x0b;
    report[1] = 0x55;
    report[2] = 0x1a;
    report[3] = byte2;   // 按键掩码
    report[4] = byte3;   // AI 总开关
    report[5] = 0x01;
    // payload: 00 00 02 00 00 04 00 00 08 ...
    let payload: [u8; 22] = [0,0, 0x02,0,0, 0x04,0,0, 0x08,byte14,byte15, 0x10,byte17,byte18, 0x20,0,0, 0x40,0,0, 0x80,0];
    for (i, b) in payload.iter().enumerate() { report[6 + i] = *b; }
    // 发送两次
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

    // 找控制端点
    let control = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0xffa0 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());

    // 阶段1: 两个键都启用AI, 都设为AI自定义模式
    if let Some(ref ctrl) = control {
        println!("\n=== 阶段1: 两个键都AI模式 ===");
        send_command(ctrl, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00);
        println!("已配置: 键1=AI, 键2=AI");
        println!("请分别短按和长按两个键各一次。10秒后自动进入阶段2...");
        read_loop(&audio_dev, &cmd_dev, 10);
    }

    // 阶段2: 键1=AI, 键2=默认
    if let Some(ref ctrl) = control {
        println!("\n=== 阶段2: 键1=AI自定义, 键2=默认行为 ===");
        send_command(ctrl, 0x10, 0x01, 0x00, 0x00, 0x01, 0x02);
        println!("已配置: 键1=AI, 键2=默认(非AI)");
        println!("请分别短按和长按两个键。10秒后结束...");
        read_loop(&audio_dev, &cmd_dev, 10);
    }

    // 阶段3: 键1=默认, 键2=AI
    if let Some(ref ctrl) = control {
        println!("\n=== 阶段3: 键1=默认, 键2=AI自定义 ===");
        send_command(ctrl, 0x08, 0x01, 0x01, 0x02, 0x00, 0x00);
        println!("已配置: 键1=默认, 键2=AI");
        println!("请分别短按和长按两个键。10秒后结束...");
        read_loop(&audio_dev, &cmd_dev, 10);
    }

    // 恢复: 两个键都AI
    if let Some(ref ctrl) = control {
        send_command(ctrl, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00);
    }

    Ok(())
}

fn read_loop(audio: &hidapi::HidDevice, cmd: &hidapi::HidDevice, duration_secs: u64) {
    let mut audio_buf = [0u8; 64];
    let mut cmd_buf = [0u8; 64];
    let deadline = Instant::now() + std::time::Duration::from_secs(duration_secs);

    while Instant::now() < deadline {
        if let Ok(64) = audio.read_timeout(&mut audio_buf, 50) {
            if audio_buf[0] == 0xB1 && audio_buf[2] == 57 {
                let t = Instant::now().duration_since(Instant::now()).as_secs_f64(); // approx
                print!(".");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
        if let Ok(n) = cmd.read_timeout(&mut cmd_buf, 10) {
            if n > 0 {
                print!("\n  [CMD n={}] ", n);
                for i in 0..n.min(32) { print!("{:02X} ", cmd_buf[i]); }
                println!();
            }
        }
    }
    println!();
}
