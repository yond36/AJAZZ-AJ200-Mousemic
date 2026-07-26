//! AI 键测试 v9: 同时读 CMD端口(可能有0x0C按键事件) + 鼠标接口 + 音频

use std::io::{self, Write};
use std::time::Instant;

fn ai_cmd(ctrl: &hidapi::HidDevice, fwd: bool, bwd: bool) {
    let byte2 = (if fwd { 0x10u8 } else { 0x00 }) | (if bwd { 0x08u8 } else { 0x00 });
    let byte3 = if fwd || bwd { 0x01 } else { 0x00 };
    let (b14, b15) = if bwd { (0x00, 0x00) } else { (0x01, 0x02) };
    let (b17, b18) = if fwd { (0x00, 0x00) } else { (0x01, 0x02) };

    let mut report = [0u8; 64];
    report[0] = 0x0b;
    report[1] = 0x55;
    report[2] = 0x1a;
    report[3] = byte2;
    report[4] = byte3;
    report[5] = 0x01;
    let payload: [u8; 22] = [0,0, 0x02,0,0, 0x04,0,0, 0x08,b14,b15, 0x10,b17,b18, 0x20,0,0, 0x40,0,0, 0x80,0];
    for (i, b) in payload.iter().enumerate() { report[6 + i] = *b; }
    ctrl.write(&report).ok();
    std::thread::sleep(std::time::Duration::from_millis(300));
    report[4] = 0x01;
    ctrl.write(&report).ok();
    std::thread::sleep(std::time::Duration::from_millis(300));
}

fn watch(cmd: &hidapi::HidDevice, mouse: Option<&hidapi::HidDevice>, audio: &hidapi::HidDevice) {
    let mut buf = [0u8; 64];
    let deadline = Instant::now() + std::time::Duration::from_secs(15);
    let mut audio_count = 0u32;

    println!("  (15秒窗口, 请按键...)");
    while Instant::now() < deadline {
        // 1. CMD端口 — 0x0C 按键事件可能走这里
        if let Ok(n) = cmd.read_timeout(&mut buf, 5) {
            if n > 0 {
                if buf[0] == 0x0C {
                    let key = buf[1];
                    if key == 0x08 { println!("  ▼ 前进键按下"); }
                    else if key == 0x04 { println!("  ▼ 后退键按下"); }
                    else if key == 0x00 { println!("  ▲ 释放"); }
                    else { println!("  [0x0C {:02X} {:02X}]", key, buf[2]); }
                } else if !(buf[0] == 0 && buf[1] == 0) {
                    print!("  [CMD {:02X}", buf[0]);
                    for i in 1..n.min(6) { print!(" {:02X}", buf[i]); }
                    println!("]");
                }
            }
        }

        // 2. 鼠标输入接口
        if let Some(m) = mouse {
            if let Ok(n) = m.read_timeout(&mut buf, 2) {
                if n > 0 && buf[0] != 0 {
                    print!("  [MOUSE {:02X}", buf[0]);
                    for i in 1..n.min(6) { print!(" {:02X}", buf[i]); }
                    println!("]");
                }
            }
        }

        // 3. 音频
        if let Ok(64) = audio.read_timeout(&mut buf, 2) {
            if buf[0] == 0xB1 && buf[2] == 57 {
                audio_count += 1;
                if audio_count == 1 { print!("  ♫ "); }
                print!(".");
                let _ = io::stdout().flush();
            }
        }
    }
    println!("\n  音频包: {} 个\n", audio_count);
}

fn main() -> anyhow::Result<()> {
    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID: {}", e))?;
    let con = mousemic_rs::hid::connect_audio(&api, &Default::default(), &Default::default(), None, &|s| eprintln!("  {}", s));
    let Some(con) = con else { anyhow::bail!("无法连接音频"); };

    let control = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0xffa0 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());
    let Some(ref ctrl) = control else { anyhow::bail!("无控制端点"); };

    // 尝试打开鼠标输入接口
    let mouse = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0x0001 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());

    println!("CMD端口: ✓  音频: ✓  鼠标输入: {}\n", if mouse.is_some() { "✓" } else { "✗" });
    println!("1=AI开启  2=AI关闭  3=仅前进  4=仅后退  q=退出");
    println!("观察 [CMD ...] 输出 — 0x0C=按键事件\n");

    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        match s.trim() {
            "1" => { ai_cmd(ctrl, true, true);   println!("AI开启(两键)"); watch(&con.cmd, mouse.as_ref(), &con.audio); }
            "2" => { ai_cmd(ctrl, false, false); println!("AI关闭(两键)"); watch(&con.cmd, mouse.as_ref(), &con.audio); }
            "3" => { ai_cmd(ctrl, true, false);  println!("仅前进AI"); watch(&con.cmd, mouse.as_ref(), &con.audio); }
            "4" => { ai_cmd(ctrl, false, true);  println!("仅后退AI"); watch(&con.cmd, mouse.as_ref(), &con.audio); }
            "q" => { ai_cmd(ctrl, true, true); println!("已恢复, 退出"); break; }
            _ => println!("?"),
        }
    }
    Ok(())
}
