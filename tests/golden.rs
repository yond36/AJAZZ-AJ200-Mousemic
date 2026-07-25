//! 黄金向量测试: 用 vendored BlueZ sbc 解码 mSBC, 与 libsbc.dll 产出的
//! 参考 PCM (`pcm_ref.s16le`, 由 `tests/data/gen_vectors.py` 生成) 逐字节比对。
//!
//! sbc (BlueZ 上游) 与 libsbc.dll (nefarius fork) 是同一份 C 源码, 故输出必须
//! 完全一致 (定点整数运算, 无浮点舍入差异)。任何字节差异都视为回归。

use std::path::PathBuf;

fn data_dir() -> PathBuf {
    PathBuf::from("tests/data")
}

#[test]
fn decode_golden_vectors() {
    let dir = data_dir();
    let frames = match std::fs::read(dir.join("msbc_frames.bin")) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("跳过: 缺少 tests/data/msbc_frames.bin (运行 gen_vectors.py 生成)");
            return;
        }
    };
    let ref_pcm = match std::fs::read(dir.join("pcm_ref.s16le")) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("跳过: 缺少 tests/data/pcm_ref.s16le");
            return;
        }
    };

    let mut dec = mousemic_rs::msbc::MsbcDecoder::new()
        .expect("sbc 解码器创建失败");

    let got = dec.decode_all(&frames);

    // 长度校验: 240 帧 × 240 字节 = 57600
    let expected_len = (frames.len() / 57) * 240;
    assert_eq!(got.len(), expected_len,
        "解码长度不匹配: 预期 {expected_len}, 实际 {}", got.len());

    // 逐字节比对: sbc 与 libsbc.dll 同源, 必须完全一致。
    let min = got.len().min(ref_pcm.len());
    assert_eq!(got.len(), ref_pcm.len(),
        "输出长度({})与参考({})不一致", got.len(), ref_pcm.len());
    let diff = got[..min]
        .iter()
        .zip(ref_pcm[..min].iter())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(diff, 0,
        "与参考向量有 {diff} 字节不同 (共 {min}); sbc 与 libsbc.dll 同源应逐字节一致");
}

#[test]
fn decoder_constants_sane() {
    let dec = mousemic_rs::msbc::MsbcDecoder::new().unwrap();
    assert_eq!(dec.framelen, 57, "framelen 应为 57");
    assert_eq!(dec.codesize, 240, "codesize 应为 240");
    assert!(mousemic_rs::msbc::MsbcDecoder::available(), "静态库应始终可用");
}
