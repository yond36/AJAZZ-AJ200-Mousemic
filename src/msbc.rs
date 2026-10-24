//! mSBC 解码器 — FFI 调用 vendored BlueZ sbc (nefarius/libsbc fork)。
//!
//! 不再依赖外部 `libsbc.dll` 或有缺陷的 `mini_sbc` crate。sbc 源码随项目
//! vendor 在 `vendor/sbc/`, 由 `build.rs` 用 `cc` 静态编译进二进制, 无需
//! 分发 DLL。这是和原始 Python 版 (调 libsbc.dll) 行为完全一致的实现——
//! 同一份 C 源码, `tests/data` 黄金向量逐字节匹配 (`diff=0`)。
//!
//! mSBC 参数 (HFP): 8 子带 / 15 块 / 16kHz / 单声道 / 16-bit,
//! 每帧 57 字节 (含 0xAD 同步字 + CRC8 + scale factors + audio) → 240 字节
//! s16le PCM (120 样本)。sbc 内部自带同步字校验与 CRC, 失败时返回 -1。

use std::os::raw::{c_int, c_ulong, c_void};
use std::ptr;

const MSBC_FRAME_LEN: usize = 57;
const MSBC_CODESIZE: usize = 240;

/// 与 sbc.h 的 `struct sbc_struct` 对应。字段顺序/布局必须与 C 完全一致。
/// `priv` / `priv_alloc_base` 是解码器内部状态指针, 由 sbc_init_msbc 分配。
#[repr(C)]
struct SbcStruct {
    flags: c_ulong,
    frequency: u8,
    blocks: u8,
    subbands: u8,
    mode: u8,
    allocation: u8,
    bitpool: u8,
    endian: u8,
    priv_: *mut c_void,
    priv_alloc_base: *mut c_void,
}

extern "C" {
    fn sbc_init_msbc(sbc: *mut SbcStruct, flags: c_ulong) -> c_int;
    fn sbc_decode(
        sbc: *mut SbcStruct,
        input: *const c_void,
        input_len: usize,
        output: *mut c_void,
        output_len: usize,
        written: *mut usize,
    ) -> isize;
    fn sbc_finish(sbc: *mut SbcStruct);
    fn sbc_get_codesize(sbc: *mut SbcStruct) -> usize;
    fn sbc_get_frame_length(sbc: *mut SbcStruct) -> usize;
}

/// mSBC 解码器: 单帧解码 (57 字节 mSBC → 240 字节 s16le PCM)。
///
/// sbc 是**有状态**解码器 (含分析滤波器历史), 但状态在 `sbc_struct` 内部,
/// 连续解码同一链路的帧即可。失步/换设备时应重建 (丢弃 self.sbc) 以避免
/// 旧设备的半截帧污染滤波器状态 (表现为"能切换但不出声")。
pub struct MsbcDecoder {
    sbc: SbcStruct,
    pub codesize: usize, // 240
    pub framelen: usize, // 57
}

impl MsbcDecoder {
    /// 创建解码器 (sbc_init_msbc)。失败极罕见, 仅内存不足时返回错误。
    pub fn new() -> anyhow::Result<Self> {
        // SAFETY: sbc_struct 零初始化是 sbc_init_* 的契约; init 成功后字段才有效。
        let mut sbc = SbcStruct {
            flags: 0,
            frequency: 0,
            blocks: 0,
            subbands: 0,
            mode: 0,
            allocation: 0,
            bitpool: 0,
            endian: 0,
            priv_: ptr::null_mut(),
            priv_alloc_base: ptr::null_mut(),
        };
        // SAFETY: sbc_init_msbc 仅写入 *sbc 并分配内部状态, 不读未初始化字段。
        let rc = unsafe { sbc_init_msbc(&mut sbc as *mut _, 0) };
        if rc != 0 {
            return Err(anyhow::anyhow!("sbc_init_msbc 失败 (rc={})", rc));
        }
        // SAFETY: init 成功后可安全查询常量 (不修改状态)。
        let codesize = unsafe { sbc_get_codesize(&sbc as *const _ as *mut _) };
        let framelen = unsafe { sbc_get_frame_length(&sbc as *const _ as *mut _) };
        Ok(Self {
            sbc,
            codesize,
            framelen,
        })
    }

    /// 解码器是否可用 (sbc 已静态链入, 始终可用)。
    pub fn available() -> bool {
        true
    }

    /// 解码一帧 mSBC → s16le PCM (长度 = codesize = 240)。
    ///
    /// 输入必须是完整 57 字节帧 (鼠标 HID 包 `buf[3..60]` 的内容, 以 0xAD 开头)。
    /// sbc 内部做同步字校验与 CRC8, 失败返回 None。正确帧输出固定 240 字节。
    ///
    /// 注意: sbc_decode 在标准 mSBC 帧上**不会** panic (与 mini_sbc 的 i32 溢出
    /// bug 无关), 故无需 catch_unwind。
    pub fn decode_frame(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() != MSBC_FRAME_LEN {
            // 只接受完整 57 字节帧; 截断帧补零会被 sbc 误读为有效帧。
            return None;
        }

        let mut out = vec![0u8; MSBC_CODESIZE];
        let mut written: usize = 0;
        // SAFETY: sbc, out, written 均为本函数局部可变内存; 长度参数正确。
        let rc = unsafe {
            sbc_decode(
                &mut self.sbc as *mut _,
                payload.as_ptr() as *const c_void,
                MSBC_FRAME_LEN,
                out.as_mut_ptr() as *mut c_void,
                MSBC_CODESIZE,
                &mut written as *mut _,
            )
        };
        if rc < 0 || written != MSBC_CODESIZE {
            return None;
        }
        Some(out)
    }

    /// 解码一段拼接好的 mSBC 帧 (每 framelen 字节一帧), 返回拼接 PCM。
    /// 尾部不足一帧的残缺数据被忽略 (不补零解码)。
    pub fn decode_all(&mut self, frames: &[u8]) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(frames.len() / MSBC_FRAME_LEN * MSBC_CODESIZE);
        for chunk in frames.chunks_exact(MSBC_FRAME_LEN) {
            if let Some(p) = self.decode_frame(chunk) {
                pcm.extend_from_slice(&p);
            }
        }
        pcm
    }
}

impl Drop for MsbcDecoder {
    fn drop(&mut self) {
        // SAFETY: 释放 sbc 内部分配的状态; drop 后不再使用。
        unsafe { sbc_finish(&mut self.sbc as *mut _) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_constants() {
        let dec = MsbcDecoder::new().unwrap();
        assert_eq!(dec.codesize, 240, "每帧应输出 240 字节 PCM");
        assert_eq!(dec.framelen, 57, "每帧应输入 57 字节 mSBC");
        assert!(MsbcDecoder::available(), "静态库始终可用");
    }

    #[test]
    fn decode_empty_and_bad() {
        let mut dec = MsbcDecoder::new().unwrap();
        assert!(dec.decode_frame(&[]).is_none(), "空帧应返回 None");
        // 全零帧: sync 字 0 != 0xAD → sbc 拒帧返回 None。
        assert!(dec.decode_frame(&[0u8; 57]).is_none(), "全零帧应返回 None");
        // 保留字节非零同样拒帧 (msbc_unpack_frame 要求 data[1]==0 && data[2]==0)。
        let mut bad = [0u8; 57];
        bad[0] = 0xAD;
        bad[1] = 1;
        assert!(dec.decode_frame(&bad).is_none(), "保留字节非零应返回 None");
    }

    #[test]
    fn truncated_frames_are_rejected_not_zero_padded() {
        let mut dec = MsbcDecoder::new().unwrap();
        // 长度不是 57 的帧一律拒绝, 不再补零交给 sbc。
        for len in [0usize, 1, 3, 4, 7, 8, 16, 32, 56, 58, 64] {
            assert!(
                dec.decode_frame(&[0u8; 64][..len]).is_none(),
                "len={} 应返回 None",
                len
            );
        }
    }

    #[test]
    fn decode_all_ignores_incomplete_tail() {
        let frames = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/msbc_frames.bin"
        ))
        .unwrap();
        let reference = MsbcDecoder::new().unwrap().decode_all(&frames);
        // 去掉 56 字节 (<1帧): 完整帧 239 个, 残尾 1 字节不产出 PCM。
        let truncated = &frames[..frames.len() - 56];
        let complete = truncated.len() / 57;
        let mut dec = MsbcDecoder::new().unwrap();
        let got = dec.decode_all(truncated);
        assert_eq!(got.len(), complete * 240);
        assert_eq!(got, reference[..complete * 240]);
    }

    #[test]
    fn bad_first_frame_does_not_distort_following_valid_frames() {
        let frames = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/msbc_frames.bin"
        ))
        .unwrap();
        let reference = MsbcDecoder::new().unwrap().decode_all(&frames);

        // 首帧同步字损坏 → 拒帧; 之后的合法帧必须与全新解码器逐字节一致。
        let mut dec = MsbcDecoder::new().unwrap();
        let mut bad_sync = frames[..57].to_vec();
        bad_sync[0] = 0x00;
        assert!(dec.decode_frame(&bad_sync).is_none());
        let after_bad_sync = dec.decode_all(&frames);
        assert_eq!(after_bad_sync, reference, "坏同步字首帧不得污染后续解码");

        // 首帧保留字节非零 → 拒帧; 同样不得污染。
        let mut dec = MsbcDecoder::new().unwrap();
        let mut bad_reserved = frames[..57].to_vec();
        bad_reserved[1] = 0x01;
        assert!(dec.decode_frame(&bad_reserved).is_none());
        let after_bad_reserved = dec.decode_all(&frames);
        assert_eq!(
            after_bad_reserved, reference,
            "坏保留字节首帧不得污染后续解码"
        );

        // CRC 损坏帧 → 拒帧; 同样不得污染。
        let mut dec = MsbcDecoder::new().unwrap();
        let mut bad_crc = frames[..57].to_vec();
        bad_crc[3] ^= 1;
        assert!(dec.decode_frame(&bad_crc).is_none());
        let after_bad_crc = dec.decode_all(&frames);
        assert_eq!(after_bad_crc, reference, "坏 CRC 首帧不得污染后续解码");
    }
}
