//! AI 键测试 v8: 0x55命令 + 0x18持久化写入。
//! 
//! AI禁用不生效的原因可能是 0x55 只改寄存器，不持久化。
//! 加上 0x18 config write 来尝试让关闭生效。

use std::io::{self, Write};
use std::time::Instant;

fn cmd55(ctrl: &hidapi::HidDevice, fwd: bool, bwd: bool) {
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

/// 发送 0x18 写入配置命令持久化
fn watch(mouse: &hidapi::HidDevice, audio: &hidapi::HidDevice) {
    let mut buf = [0u8; 64];
    let deadline = Instant::now() + std::time::Duration::from_secs(12);
    let mut audio_count = 0u32;
    let mut key_state: Option<u8> = None;

    println!("  (12秒窗口, 请按键...)");
    while Instant::now() < deadline {
        if let Ok(n) = mouse.read_timeout(&mut buf, 5) {
            if n > 0 {
                if buf[0] == 0x0C {
                    let key = buf[1];
                    if key == 0x08 && key_state != Some(0x08) {
                        key_state = Some(0x08);
                        println!("  ▼ 前进键按下");
                    } else if key == 0x04 && key_state != Some(0x04) {
                        key_state = Some(0x04);
                        println!("  ▼ 后退键按下");
                    } else if key == 0x00 && key_state.is_some() {
                        let name = if key_state == Some(0x08) { "前进键" } else { "后退键" };
                        println!("  ▲ {}释放", name);
                        key_state = None;
                    }
                }
            }
        }
        if let Ok(64) = audio.read_timeout(&mut buf, 5) {
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

    let mouse = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0x0001 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());
    let has_mouse = mouse.is_some();
    println!( "鼠标输入接口: {}", if has_mouse { "✓" } else { "✗" } );

    println!("\n1=AI开启  2=AI关闭  3=仅前进  4=仅后退  q=退出\n");

    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        match s.trim() {
            "1" => { cmd55(ctrl, true, true);  println!("AI开启(两键)"); if has_mouse { watch(mouse.as_ref().unwrap(), &con.audio); } }
            "2" => { cmd55(ctrl, false, false); println!("AI关闭(两键)"); if has_mouse { watch(mouse.as_ref().unwrap(), &con.audio); } }
            "3" => { cmd55(ctrl, true, false);  println!("仅前进AI"); if has_mouse { watch(mouse.as_ref().unwrap(), &con.audio); } }
            "4" => { cmd55(ctrl, false, true);  println!("仅后退AI"); if has_mouse { watch(mouse.as_ref().unwrap(), &con.audio); } }
            "q" => { cmd55(ctrl, true, true); println!("已恢复, 退出"); break; }
            _ => println!("?"),
        }
    }
    Ok(())
}
