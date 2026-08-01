//! 按键区分测试: 打开所有 AJAZZ 接口，异步方式读 0x0C 事件。
//! 关键: JS 用 dev.on('data') 异步模式，这里用独立线程 + blocking read。

use std::io::{self};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

fn main() -> anyhow::Result<()> {
    let api = hidapi::HidApi::new().map_err(|e| anyhow::anyhow!("HID: {}", e))?;

    // 找到所有 AJAZZ 接口
    let paths: Vec<_> = api.device_list()
        .filter(|d| d.vendor_id() == 0x363C)
        .map(|d| (d.path().to_string_lossy().into_owned(),
                  d.usage_page(), d.usage(),
                  d.product_id()))
        .collect();

    println!("找到 {} 个 AJAZZ 接口:", paths.len());
    for (p, up, u, pid) in &paths {
        println!("  up=0x{:04X} u=0x{:04X} pid=0x{:04X} {}", up, u, pid, p.split('#').nth(1).unwrap_or("?"));
    }
    println!();

    let running = Arc::new(AtomicBool::new(true));
    let _r = running.clone();

    // Ctrl+C 处理
    let r2 = running.clone();
    thread::spawn(move || {
        let mut s = String::new();
        let _ = io::stdin().read_line(&mut s);
        r2.store(false, Ordering::SeqCst);
    });

    // 对每个接口创建读线程
    let mut handles = Vec::new();

    for (path, up, u, pid) in &paths {
        // 跳过音频接口 (只产音频)
        if *up == 0xFFAA { continue; }

        let path = path.clone();
        let up = *up;
        let u = *u;
        let pid = *pid;
        let running = running.clone();

        let handle = thread::spawn(move || {
            if let Ok(a) = hidapi::HidApi::new() {
                match a.open_path(&std::ffi::CString::new(path.as_bytes()).unwrap()) {
                    Ok(dev) => {
                        let mut buf = [0u8; 64];
                        while running.load(Ordering::SeqCst) {
                            match dev.read_timeout(&mut buf, 100) {
                                Ok(n) if n > 0 => {
                                    if buf[0] != 0 {
                                        let label = format!("up=0x{:04X} u=0x{:04X} pid=0x{:04X}", up, u, pid);
                                        if buf[0] == 0x0C {
                                            match buf[1] {
                                                0x08 => println!("[{}] ▼ 前进键按下", label),
                                                0x04 => println!("[{}] ▼ 后退键按下", label),
                                                0x00 => println!("[{}] ▲ 释放", label),
                                                _ => println!("[{}] 0x0C {:02X} {:02X}", label, buf[1], buf[2]),
                                            }
                                        } else {
                                            print!("[{}] ", label);
                                            for &b in buf.iter().take(n.min(8)) { print!("{:02X} ", b); }
                                            println!();
                                        }
                                    }
                                }
                                Err(_) => break,
                                _ => {}
                            }
                        }
                    }
                    Err(e) => println!("无法打开 up=0x{:04X} u=0x{:04X}: {}", up, u, e),
                }
            }
        });
        handles.push((up, u, pid, handle));
    }

    println!("已开启 {} 个读线程。按两个AI键，看哪个接口有 0x0C 事件。回车退出。\n", handles.len());

    for (_, _, _, h) in handles {
        let _ = h.join();
    }
    Ok(())
}
