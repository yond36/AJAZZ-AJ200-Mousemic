//! AI 键区分测试 v5: 逐键逐模式测试。
//!
//! 每次只测一个键的一种状态，按提示操作。

use std::time::Instant;
use std::io::{self, Write};

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

fn wait_key() {
    print!("按回车继续...");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
}

struct TestCase {
    name: &'static str,
    byte2: u8, byte3: u8, byte14: u8, byte15: u8, byte17: u8, byte18: u8,
    key: &'static str,
    expect: &'static str,
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
        println!("未找到控制端点");
        return Ok(());
    };

    let tests = [
        TestCase { name: "键1(前进) AI开启",  byte2: 0x10, byte3: 0x01, byte14: 0x01, byte15: 0x02, byte17: 0x00, byte18: 0x00, key: "前进键",  expect: "有音频" },
        TestCase { name: "键1(前进) AI关闭",  byte2: 0x00, byte3: 0x00, byte14: 0x01, byte15: 0x02, byte17: 0x01, byte18: 0x02, key: "前进键",  expect: "无音频" },
        TestCase { name: "键2(后退) AI开启",  byte2: 0x08, byte3: 0x01, byte14: 0x00, byte15: 0x00, byte17: 0x01, byte18: 0x02, key: "后退键",  expect: "有音频" },
        TestCase { name: "键2(后退) AI关闭",  byte2: 0x00, byte3: 0x00, byte14: 0x01, byte15: 0x02, byte17: 0x01, byte18: 0x02, key: "后退键",  expect: "无音频" },
    ];

    for t in &tests {
        println!("\n══════════════════════════════════════");
        println!("测试: {}", t.name);
        println!("配置: byte2={:#04X} byte3={:#04X}", t.byte2, t.byte3);
        println!("操作: 长按【{}】", t.key);
        println!("预期: {}", t.expect);
        send_command(ctrl, t.byte2, t.byte3, t.byte14, t.byte15, t.byte17, t.byte18);
        wait_key();
        read_loop(&audio_dev, &cmd_dev, 8, t.name);
    }

    // 恢复
    send_command(ctrl, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00);
    println!("\n完成。已恢复两个键AI模式。");
    Ok(())
}

fn read_loop(audio: &hidapi::HidDevice, cmd: &hidapi::HidDevice, duration_secs: u64, label: &str) {
    let mut audio_buf = [0u8; 64];
    let mut cmd_buf = [0u8; 64];
    let deadline = Instant::now() + std::time::Duration::from_secs(duration_secs);
    let mut audio_count = 0u32;

    println!("  (8秒测试窗口, 请长按)");

    while Instant::now() < deadline {
        if let Ok(64) = audio.read_timeout(&mut audio_buf, 50) {
            if audio_buf[0] == 0xB1 && audio_buf[2] == 57 {
                audio_count += 1;
                if audio_count == 1 { print!("  音频: "); }
                print!(".");
                let _ = io::stdout().flush();
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
        println!("\n  结果: 收到 {} 个音频包 — {}", audio_count, if label.contains("关闭") { "❌ 异常(应有音频)" } else { "✓ 正常" });
    } else {
        println!("\n  结果: 无音频 — {}", if label.contains("关闭") { "✓ 正常" } else { "❌ 异常(应有音频)" });
    }
}
