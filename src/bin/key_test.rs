//! 按键区分测试: 只检测 CMD/MOUSE 端口的 0x0C 按键事件。

use std::io::{self, Write};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID: {}", e))?;
    let con = mousemic_rs::hid::connect_audio(&api, &Default::default(), &Default::default(), None, &|s| eprintln!("  {}", s));
    let Some(con) = con else { anyhow::bail!("无法连接"); };

    // 鼠标输入接口
    let mouse = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C && d.product_id() == con.pid
            && d.usage_page() == 0x0001 && d.usage() == 0x0002)
        .find_map(|d| api.open_path(d.path()).ok());

    println!("CMD端口: ✓  鼠标接口: {}\n", if mouse.is_some() { "✓" } else { "✗" });
    println!("按前进和后退键, 看是否有 0x0C 事件。Ctrl+C 退出。\n");

    let mut buf = [0u8; 64];
    loop {
        // CMD端口
        if let Ok(n) = con.cmd.read_timeout(&mut buf, 10) {
            if n > 0 {
                print!("CMD: ");
                for i in 0..n.min(8) { print!("{:02X} ", buf[i]); }
                if buf[0] == 0x0C {
                    match buf[1] {
                        0x08 => println!("← 前进键按下"),
                        0x04 => println!("← 后退键按下"),
                        0x00 => println!("← 释放"),
                        _ => println!(),
                    }
                } else {
                    println!();
                }
            }
        }
        // 鼠标接口
        if let Some(ref m) = mouse {
            if let Ok(n) = m.read_timeout(&mut buf, 5) {
                if n > 0 {
                    print!("MOUSE: ");
                    for i in 0..n.min(8) { print!("{:02X} ", buf[i]); }
                    if buf[0] == 0x0C {
                        match buf[1] {
                            0x08 => println!("← 前进键按下"),
                            0x04 => println!("← 后退键按下"),
                            0x00 => println!("← 释放"),
                            _ => println!(),
                        }
                    } else {
                        println!();
                    }
                }
            }
        }
    }
}
