//! 构建 sbc (BlueZ SBC/mSBC codec) 为静态库, 供 `msbc` 模块 FFI 调用。
//!
//! 源码 vendored 自 nefarius/libsbc (bluez sbc 的 Windows/MSVC 适配 fork),
//! 见 `vendor/sbc/NOTICE`。用 `cc` crate 编译, 无需外部 DLL 或系统库。
//!
//! 仅编译解码所需的纯 C 实现 (sbc.c + sbc_primitives.c), 不启用 SIMD 优化
//! (SSE/MMX 在 MSVC 下需额外 intrinsics 适配; 纯 C 参考实现已足够语音解码性能,
//! 每帧 57 字节解码耗时微秒级)。ARM/NEON/iWMMXT 头文件保留 (被 sbc_primitives.c
//! include, 但其实现由 #ifdef 守卫, x86 下不参与编译)。

fn main() {
    let mut b = cc::Build::new();
    b.include("vendor/sbc")
        // 不定义 HAVE_CONFIG_H → sbc.c 跳过 autotools 的 <config.h>
        .file("vendor/sbc/sbc.c")
        .file("vendor/sbc/sbc_primitives.c");

    // MSVC 警告抑制 (sbc.c 是 C89 风格 C, 有大量隐式转换/未用参数):
    //   4244: int32_t -> int16_t 截断 (sbc 内部已知, 不影响解码正确性)
    //   4267: size_t -> int 转换
    //   4146: 对 unsigned 一元负号 (crc 计算, C 标准定义行为)
    //   4100: 未使用形参
    //   4456: 变量名遮蔽
    for f in ["/wd4244", "/wd4267", "/wd4146", "/wd4100", "/wd4456"] {
        b.flag_if_supported(f);
    }
    // GCC/Clang: 同样抑制, 保持构建干净。
    for f in ["-Wno-conversion", "-Wno-unused-parameter", "-Wno-sign-compare"] {
        b.flag_if_supported(f);
    }

    b.compile("sbc");

    // 源码变更时重新编译。
    for f in std::fs::read_dir("vendor/sbc").unwrap().flatten() {
        println!("cargo:rerun-if-changed={}", f.path().display());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
