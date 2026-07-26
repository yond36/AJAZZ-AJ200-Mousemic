//! AI 键测试 v6: 菜单式交互。
//!  1=AI模式启用  2=AI模式禁用
//!  3=前进AI开启  4=前进AI关闭
//!  5=后退AI开启  6=后退AI关闭
//!  然后按键观察音频。q=退出。

use std::io::{self, Write};
use std::time::Instant;

fn cmd(ctrl: &hidapi::HidDevice, byte2: u8, byte3: u8, byte14: u8, byte15: u8, byte17: u8, byte18: u8) {
    let mut report = [0u8; 64];
    report[0] = 0x0b;
    report[1] = 0x55;
    report[2] = 0x1a;
    report[3] = byte2;
    report[4] = byte3;
    report[5] = 0x01;
    let payload: [u8; 22] = [0,0, 0x02,0,0, 0x04,0,0, 0x08,byte14,byte15, 0x10,byte17,byte18, 0x20,0,0, 0x40,0,0, 0x80,0];
    for (i, b) in payload.iter().enumerate() { report[6 + i] = *b; }
    ctrl.write(&report).ok();
    std::thread::sleep(std::time::Duration::from_millis(300));
    report[4] = 0x01;
    ctrl.write(&report).ok();
    std::thread::sleep(std::time::Duration::from_millis(300));
}

fn show_audio(audio: &hidapi::HidDevice) {
    let mut buf = [0u8; 64];
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    let mut count = 0u32;
    println!("  (10秒窗口, 请按键, 等待中...)");
    while Instant::now() < deadline {
        if let Ok(64) = audio.read_timeout(&mut buf, 100) {
            if buf[0] == 0xB1 && buf[2] == 57 {
                count += 1;
                if count == 1 { print!("  ▶ "); }
                print!(".");
                let _ = io::stdout().flush();
            }
        }
    }
    if count > 0 {
        println!("\n  → 收到 {} 个音频包\n", count);
    } else {
        println!("\n  → 无音频\n");
    }
}

fn main() -> anyhow::Result<()> {
    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID: {}", e))?;
    let con = mousemic_rs::hid::connect_audio(&api, &Default::default(), &Default::default(), None, &|s| eprintln!("  {}", s));
    let Some(con) = con else { anyhow::bail!("无法连接"); };

    let control = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0xffa0 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());

    let Some(ref ctrl) = control else {
        anyhow::bail!("未找到控制端点");
    };

    println!("PID={:#06X} {}\n", con.pid, con.product_string);
    println!("1 = AI启用(两键)   2 = AI禁用(两键)");
    println!("3 = 前进AI开启      4 = 前进AI关闭");
    println!("5 = 后退AI开启      6 = 后退AI关闭");
    println!("q = 退出\n");

    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        let s = s.trim();

        match s {
            // byte2 byte3  byte14 byte15 byte17 byte18
            "1" => { cmd(ctrl, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00); println!("已设置: AI启用(两键)"); show_audio(&con.audio); }
            "2" => { cmd(ctrl, 0x00, 0x00, 0x01, 0x02, 0x01, 0x02); println!("已设置: AI禁用(两键)"); show_audio(&con.audio); }
            "3" => { cmd(ctrl, 0x10, 0x01, 0x00, 0x00, 0x01, 0x02); println!("已设置: 前进AI开启"); show_audio(&con.audio); }
            "4" => { cmd(ctrl, 0x00, 0x00, 0x01, 0x02, 0x01, 0x02); println!("已设置: 前进AI关闭(=全都禁用)"); show_audio(&con.audio); }
            "5" => { cmd(ctrl, 0x08, 0x01, 0x01, 0x02, 0x00, 0x00); println!("已设置: 后退AI开启"); show_audio(&con.audio); }
            "6" => { cmd(ctrl, 0x00, 0x00, 0x01, 0x02, 0x01, 0x02); println!("已设置: 后退AI关闭(=全都禁用)"); show_audio(&con.audio); }
            "q" => { cmd(ctrl, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00); println!("已恢复, 退出"); break; }
            _ => println!("? 输入 1-6 或 q"),
        }
    }
    Ok(())
}
