#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""用已验证的 libsbc.dll 生成 mSBC 黄金测试向量, 供 Rust 解码器 (cargo test) 校准。

产出 (写入本目录):
  pcm_input.s16le  原始合成 PCM (i16, 16kHz, mono), 4 段: 静音/1kHz正弦/白噪声/混合
  msbc_frames.bin  按 mSBC 线格式拼接的 57 字节帧 (即鼠标 HID 包 rep[3:60] 的内容)
  pcm_ref.s16le    libsbc 把上述帧解回的 PCM (解码器预期的参考输出)
  vectors.json     元信息

用法:
  python gen_vectors.py
"""
import ctypes
import json
import math
import os
import struct

HERE = os.path.dirname(os.path.abspath(__file__))
DLL = os.path.join(HERE, "..", "..", "..", "libsbc.dll")  # E:/Mousemic/libsbc.dll
DLL = os.path.abspath(DLL)
assert os.path.exists(DLL), "找不到 libsbc.dll: %s" % DLL

lib = ctypes.CDLL(DLL)
lib.sbc_init_msbc.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
lib.sbc_init_msbc.restype = ctypes.c_int
lib.sbc_get_codesize.argtypes = [ctypes.c_void_p]
lib.sbc_get_codesize.restype = ctypes.c_size_t
lib.sbc_get_frame_length.argtypes = [ctypes.c_void_p]
lib.sbc_get_frame_length.restype = ctypes.c_size_t
lib.sbc_encode.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_size_t,
                           ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
lib.sbc_encode.restype = ctypes.c_ssize_t
lib.sbc_decode.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_size_t,
                           ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
lib.sbc_decode.restype = ctypes.c_ssize_t
lib.sbc_finish.argtypes = [ctypes.c_void_p]
lib.sbc_finish.restype = ctypes.c_int


def make_sbc():
    raw = (ctypes.c_char * (8192 + 16))()
    p = ctypes.c_void_p((ctypes.addressof(raw) + 15) & ~15)
    if lib.sbc_init_msbc(p, 0) != 0:
        raise RuntimeError("sbc_init_msbc 失败")
    cs = lib.sbc_get_codesize(p)       # 240
    fl = lib.sbc_get_frame_length(p)   # 57
    return p, raw, cs, fl  # raw 必须保持引用, 否则 p 指向的内存被 GC 释放 (use-after-free)


def main():
    print("start gen_vectors", flush=True)
    sr = 16000
    p_enc, raw_e, codesize, framelen = make_sbc()
    p_dec, raw_d, _, _ = make_sbc()
    assert codesize == 240 and framelen == 57, (codesize, framelen)

    # 合成 PCM: 每段 60 帧 * 120 样本 = 7200 样本, 共 4 段
    per_seg_frames = 60
    per_seg_samples = per_seg_frames * (codesize // 2)  # 120
    samples = []
    for seg in range(4):
        for i in range(per_seg_samples):
            t = i / sr
            if seg == 0:
                v = 0.0
            elif seg == 1:
                v = 0.1 * math.sin(2 * math.pi * 1000 * t)          # 1kHz -20dBFS
            elif seg == 2:
                v = (os.urandom(1)[0] / 255.0 - 0.5) * 0.2          # 白噪声 -14dB 左右
            else:
                v = 0.08 * math.sin(2 * math.pi * 1000 * t) + (os.urandom(1)[0] / 255.0 - 0.5) * 0.15
            s = max(-1.0, min(1.0, v))
            samples.append(int(s * 32767))
    pcm_input = struct.pack("<%dh" % len(samples), *samples)
    del samples

    # 编码: 每 codesize 字节一帧 -> framelen 字节
    frames = bytearray()
    for off in range(0, len(pcm_input), codesize):
        chunk = pcm_input[off:off + codesize]
        if len(chunk) < codesize:
            chunk = chunk + b"\x00" * (codesize - len(chunk))
        ibuf = ctypes.create_string_buffer(chunk, codesize)
        obuf = (ctypes.c_char * framelen)()
        written = ctypes.c_size_t(0)
        consumed = lib.sbc_encode(p_enc, ibuf, codesize, obuf, framelen, ctypes.byref(written))
        assert consumed == codesize, "encode consumed %r" % consumed
        assert written.value == framelen, "encode wrote %r" % written.value
        frame = obuf.raw[:framelen]
        assert frame[0] == 0xAD, "mSBC 帧同步字应为 0xAD, 实际 0x%02X" % frame[0]
        frames += frame

    # 解码回参考 PCM
    ref = bytearray()
    for off in range(0, len(frames), framelen):
        frame = bytes(frames[off:off + framelen])
        ibuf = ctypes.create_string_buffer(frame, framelen)
        obuf = (ctypes.c_char * codesize)()
        written = ctypes.c_size_t(0)
        consumed = lib.sbc_decode(p_dec, ibuf, framelen, obuf, codesize, ctypes.byref(written))
        assert consumed == framelen, "decode consumed %r" % consumed
        ref += obuf.raw[:written.value]

    lib.sbc_finish(p_enc)
    lib.sbc_finish(p_dec)

    n_frames = len(frames) // framelen
    meta = {
        "sample_rate": sr,
        "channels": 1,
        "bits": 16,
        "format": "s16le",
        "codesize": codesize,
        "framelen": framelen,
        "n_frames": n_frames,
        "pcm_input_bytes": len(pcm_input),
        "pcm_ref_bytes": len(ref),
        "segments": [
            {"index": 0, "kind": "silence", "frames": per_seg_frames},
            {"index": 1, "kind": "sine_1k_-20dBFS", "frames": per_seg_frames},
            {"index": 2, "kind": "white_noise", "frames": per_seg_frames},
            {"index": 3, "kind": "sine_plus_noise", "frames": per_seg_frames},
        ],
        "source": "libsbc.dll (BlueZ sbc reference) encode->decode",
    }
    with open(os.path.join(HERE, "pcm_input.s16le"), "wb") as f:
        f.write(pcm_input)
    with open(os.path.join(HERE, "msbc_frames.bin"), "wb") as f:
        f.write(bytes(frames))
    with open(os.path.join(HERE, "pcm_ref.s16le"), "wb") as f:
        f.write(bytes(ref))
    with open(os.path.join(HERE, "vectors.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2, ensure_ascii=False)

    # 自检: 参考解码与原始输入的 RMS 比例 (应接近 1, 说明编解码闭环正常)
    def rms(b):
        n = len(b) // 2
        s = struct.unpack("<%dh" % n, b[:n * 2])
        return math.sqrt(sum(x * x for x in s) / n) if n else 0.0
    print("n_frames =", n_frames, flush=True)
    print("pcm_input RMS = %.1f, pcm_ref RMS = %.1f, ratio = %.3f"
          % (rms(pcm_input), rms(bytes(ref)), rms(bytes(ref)) / max(1e-9, rms(pcm_input))), flush=True)
    print("向量已写入:", HERE, flush=True)
    # libsbc 在解释器退出清理期偶发崩溃会冲掉输出, 这里直接退出跳过清理
    import sys
    sys.stdout.flush()
    os._exit(0)


if __name__ == "__main__":
    main()
