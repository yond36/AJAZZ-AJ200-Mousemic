#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
mousemic.py — AJAZZ 语音鼠标 (VID=363C, 同系列多 PID 兼容) 桥接为普通麦克风

逆向结论（来自 usb.pcapng 抓包分析 + 实测验证）:
  1. 激活: 鼠标上电/被 0x55-off 关闭后, 需先向 0xFFA0 命令集合发送激活序列
     (SET_REPORT, ReportID=0x0B, 64 字节), 每条命令会收到 0x0A 应答 (同集合读取):
       0B 13 (x2) -> 0A 13 37 00 00 <设备ID5字节>
       0B 14 / 0B 15 / 0B 17 / 0B 19 / 0B 51  (读配置, 应答内容固定)
       0B 55 1A 18 01 01 00 00 02 ...  -> 语音模式 ON  (flag=00 即 OFF)
     全部应答与 AJAZZ Driver J2 软件会话完全一致 (静态值, 无加密握手)。
  2. 音频: 激活后按住语音键, 0xFFAA 集合 (Col07) 以 8ms 周期推送 64 字节中断包:
       [0]=0xB1 [1]=0x01 [2]=0x39(=57) [3:60]=57字节 mSBC 帧 [60:64]=0填充
     mSBC = 蓝牙 HFP 宽带语音, 0xAD 同步字, 16kHz 单声道, 每帧 120 采样。
  3. 语音键状态包 (0C 04 EE / 0C 00 00) 走 Consumer Control 集合 (Col03)。

依赖: pip install hidapi sounddevice; 本机需有 libsbc.dll (已随包内置)。
用法:
  python mousemic.py --list                 列出可用音频输出设备
  python mousemic.py --play                 按住语音键, 扬声器直接试听
  python mousemic.py --file test.wav        按住语音键录音, Ctrl+C 结束
  python mousemic.py --cable "CABLE Input"  转发到 VB-CABLE 虚拟麦克风
    (其他软件里选择 "CABLE Output" 作为麦克风即可)

  联动热键 (可选):
    --hotkey right_alt   按住鼠标语音键时同步按住键盘热键, 松手同步抬起。
    可选值: right_alt / right_ctrl / right_shift / f9 / f10 / space / grave / capslock
    原理: 以音频流出现/消失为信号合成 SendInput 扫描码按键, 与音频天然同步。
    --driver sendinput   按键注入方式: sendinput (默认) 或 interception。
    若目标软件 (用 Raw Input 监听热键, 如部分输入法/Discord) 识别不到模拟按键,
    需安装 Interception 内核驱动并把 interception.dll 放到本脚本同目录后用后者。
"""

import argparse
import ctypes
import os
import shutil
import subprocess
import sys
import threading
import time

# ffmpeg 已被 libsbc.dll 取代: 进程内 mSBC 解码 (见 msbc_decoder.py)
from msbc_decoder import MsbcDecoder, MsbcPump, libsbc_available, _libsbc_path


def _single_instance():
    """命名互斥量: 防止多个实例同时跑导致 HID/热键冲突。返回 False 表示已有实例。"""
    mutex = ctypes.windll.kernel32.CreateMutexW(None, False, "Global\\mousemic_single")
    return ctypes.windll.kernel32.GetLastError() != 183  # ERROR_ALREADY_EXISTS

VID = 0x363C
# 有线/无线分类完全靠 product_string: 同系列鼠标无线接口 product_string 带 "2.4G"、
# 有线接口带 "Mouse"(如 "AJAZZ Mouse"), 与 PID 取值无关。因此换 PID(ED03/ED05/ED1B/
# ED00...)也能自动识别有线/无线并兼容, 无需任何写死的 PID 列表。
# 仅当某鼠标的 product_string 既不含 "2.4G" 也不含 "Mouse" 时, 才需要在
# mousemic_gui.json 里用 wired_pids / wireless_pids 手动指定 PID 集合作为兜底。


def _classify_link(d, wired_pids=None, wireless_pids=None):
    """判断某 HID 设备是有线还是无线:
      - 优先(默认): product_string —— 同系列鼠标无线接口带 '2.4G'、有线接口带 'Mouse'
        (如 'AJAZZ Mouse'), 与 PID 取值无关, 因此换 PID(ED1B/ED00...)也能自动识别;
      - 可选手动兜底: 若某鼠标 product_string 不含 '2.4G'/'Mouse', 可用配置里的
        wired_pids / wireless_pids 集合显式指定;
      - 都判不出 -> 'unknown' (作为兜底候选, 仍会被纳入握手尝试)。"""
    pid = d.get("product_id")
    if wired_pids and pid in set(wired_pids):
        return "wired"
    if wireless_pids and pid in set(wireless_pids):
        return "wireless"
    ps = (d.get("product_string") or "").lower()
    if "2.4g" in ps:
        return "wireless"
    if "mouse" in ps:
        return "wired"
    return "unknown"


def _classify_label(d, wired_pids=None, wireless_pids=None):
    return {"wired": "有线", "wireless": "无线", "unknown": "未知"}.get(
        _classify_link(d, wired_pids, wireless_pids), "未知")
AUDIO_USAGE_PAGE = 0xFFAA   # 音频输入 (Col07)
CMD_USAGE_PAGE = 0xFFA0     # 保留兼容: 旧代码/旧固件命令通道 (Col05)
# 同系列不同 PID 的鼠标, 命令通道 usage_page 可能不同(如 ED03/ED05 用 0xFFA0,
# ED00 用 0xFFB1/0xFFDF)。arm_mouse 会按此列表逐个探测, 而非只认一个固定值。
CMD_USAGE_PAGES = (0xFFA0, 0xFFB1, 0xFFDF)
REPORT_ID = 0xB1
SAMPLE_RATE = 16000
HOTKEY_IDLE_TIMEOUT = 0.6  # 音频流消失多久后认为语音键已松开 (固件有约0.8s拖尾, 拖尾里是真音频)

# 联动热键表: 名称 -> (Set-1 扫描码, 是否扩展键)
# right_alt 经 Interception 注入在 Raw Input 监听层面不可见, 但豆包等软件实测可触发 (LL hook 层面收到)
HOTKEYS = {
    "left_alt": (0x38, False), "right_alt": (0x38, True),
    "right_ctrl": (0x1D, True), "right_shift": (0x36, False),
    "f9": (0x43, False), "f10": (0x44, False),
    "space": (0x39, False), "grave": (0x29, False), "capslock": (0x3A, False),
}


def _send_scan(scancode, extended, keyup):
    """SendInput 合成一次扫描码按键 (系统会翻译成对应 VK, 如右 Alt=VK_RMENU)。"""
    import ctypes
    from ctypes import wintypes

    KEYEVENTF_EXTENDEDKEY = 0x0001
    KEYEVENTF_KEYUP = 0x0002
    KEYEVENTF_SCANCODE = 0x0008

    class KEYBDINPUT(ctypes.Structure):
        _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                    ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                    ("dwExtraInfo", ctypes.c_size_t)]

    class MOUSEINPUT(ctypes.Structure):
        _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG),
                    ("mouseData", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
                    ("time", wintypes.DWORD), ("dwExtraInfo", ctypes.c_size_t)]

    class HARDWAREINPUT(ctypes.Structure):
        _fields_ = [("uMsg", wintypes.DWORD), ("wParamL", wintypes.WORD), ("wParamH", wintypes.WORD)]

    class _U(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]

    class INPUT(ctypes.Structure):
        _fields_ = [("type", wintypes.DWORD), ("u", _U)]

    flags = KEYEVENTF_SCANCODE | (KEYEVENTF_EXTENDEDKEY if extended else 0) | (KEYEVENTF_KEYUP if keyup else 0)
    inp = INPUT(1, _U(ki=KEYBDINPUT(0, scancode, flags, 0, 0)))
    ctypes.windll.user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(inp))


class HotKey:
    """SendInput 方式: 跟踪按住状态, 避免重复发送。"""

    def __init__(self, name):
        self.sc, self.ext = HOTKEYS[name]
        self.name = name
        self.down = False

    def press(self):
        if not self.down:
            _send_scan(self.sc, self.ext, False)
            self.down = True

    def release(self):
        if self.down:
            _send_scan(self.sc, self.ext, True)
            self.down = False

    def close(self):
        self.release()


class HotKeyInterception(HotKey):
    """Interception 内核驱动方式: 能穿透 Raw Input 监听 (需要安装驱动 + interception.dll)。"""

    # 注意: 键盘设备号是 1-10, 鼠标是 11-20
    # (interception.h: INTERCEPTION_KEYBOARD(i)=i+1, INTERCEPTION_MOUSE(i)=11+i)

    def __init__(self, name):
        super().__init__(name)
        import ctypes, os
        dll = None
        for p in (os.path.join(os.path.dirname(os.path.abspath(__file__)), "interception.dll"),
                  "interception.dll"):
            try:
                dll = ctypes.WinDLL(p)
                break
            except OSError:
                continue
        if dll is None:
            raise RuntimeError(
                "找不到 interception.dll。请从 https://github.com/oblitum/Interception/releases "
                "下载 Interception.zip, 以管理员运行 install-interception.exe /install 并重启, "
                "再把 library/x64/interception.dll 放到本脚本同目录。")
        self._ct = ctypes
        dll.interception_create_context.restype = ctypes.c_void_p
        dll.interception_send.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, ctypes.c_uint]
        dll.interception_destroy_context.argtypes = [ctypes.c_void_p]
        dll.interception_is_keyboard.argtypes = [ctypes.c_int]
        dll.interception_is_invalid.argtypes = [ctypes.c_int]
        self._dll = dll
        self._ctx = dll.interception_create_context()
        if not self._ctx:
            raise RuntimeError("Interception 上下文创建失败 (驱动未安装或未重启?)")
        # 选键盘设备: 优先 AJAZZ 鼠标自带的键盘接口, 其次第一个有实体硬件的非虚拟键盘
        dll.interception_get_hardware_id.argtypes = [ctypes.c_void_p, ctypes.c_int,
                                                     ctypes.c_void_p, ctypes.c_uint]
        dll.interception_get_hardware_id.restype = ctypes.c_uint
        best = fallback = 0
        for dev in range(1, 11):
            if not dll.interception_is_keyboard(dev):
                continue
            buf = (ctypes.c_ubyte * 512)()
            n = dll.interception_get_hardware_id(self._ctx, dev, buf, 512)
            if not n:
                continue
            hwid = bytes(buf[:n]).decode("utf-16-le", "ignore")
            if "VID_363C" in hwid:
                best = dev
                break
            if not fallback and "GVInput" not in hwid:
                fallback = dev
        self.device = best or fallback
        if not self.device:
            raise RuntimeError("Interception 找不到可用键盘设备")

    def _stroke(self, keyup):
        # InterceptionStroke 缓冲 20 字节; 起始处为 InterceptionKeyStroke:
        # code u16, state u16, information u32
        import struct
        state = (0x01 if keyup else 0x00) | (0x02 if self.ext else 0x00)  # UP=1, E0=2
        buf = (self._ct.c_ubyte * 20)()
        struct.pack_into("<HHI", buf, 0, self.sc, state, 0)
        return buf

    def press(self):
        if not self.down:
            s = self._stroke(False)
            self._dll.interception_send(self._ctx, self.device, self._ct.byref(s), 1)
            self.down = True

    def release(self):
        if self.down:
            s = self._stroke(True)
            self._dll.interception_send(self._ctx, self.device, self._ct.byref(s), 1)
            self.down = False

    def close(self):
        self.release()
        if getattr(self, "_ctx", None):
            self._dll.interception_destroy_context(self._ctx)
            self._ctx = None

# AJAZZ 激活序列 (与官方驱动会话逐字节一致)
def _pkt(c, tail=b""):
    b = bytearray(64)
    b[0] = 0x0B
    b[1] = c
    b[2:2 + len(tail)] = tail
    return bytes(b)

ARM_SEQ = [
    _pkt(0x13), _pkt(0x13), _pkt(0x14), _pkt(0x15),
    _pkt(0x17), _pkt(0x19), _pkt(0x51),
    _pkt(0x55, bytes.fromhex("1a18010100000200000400000801001002002000004000008000")),
]


def _enumerate_ajazz(usage_page):
    """枚举所有 AJAZZ(VID_363C) 且 usage_page 匹配的 HID 接口。"""
    import hid
    return [d for d in hid.enumerate()
            if d.get("vendor_id") == VID and d.get("usage_page") == usage_page]


def find_hid(usage_page, prefer_pid=None, exclude_path=None):
    """按 VID + usage_page 查找鼠标 HID 接口路径。

    三模鼠标: 有线/BT 用 PID=0xED03, 无线接收器用 PID=0xED05, 两者音频 usage_page
    都是 0xFFAA。旧逻辑 enumerate(VID, PID=ED05) 枚举不到有线设备, 导致"插线用不了"。

    默认(无 prefer_pid): 有线优先 ED03 -> 无线 ED05 -> 任意 AJAZZ 音频接口。
    传 prefer_pid 时**严格**只匹配该 PID(不匹配即返回 None, 不回退其他链路)——
    这是为了让 arm/选路能明确判断"某条特定链路是否在线", 而非被回退误导。

    exclude_path: 热切换时传入"刚拔掉的设备 path"。Windows 拔线后该条目会在枚举里残留
    一小段时间, 若不加排除, 有线优先逻辑会错误地重新选中这个残留的有线条目, 导致"切回
    无线没声音"。传 exclude_path 可强制跳过它, 直接选到真正在线的无线设备。"""
    matches = _enumerate_ajazz(usage_page)
    if exclude_path:
        matches = [m for m in matches if m.get("path") != exclude_path]
    if not matches:
        return None
    if prefer_pid is not None:
        # 严格匹配指定 PID: 不存在直接返回 None(不回退到其他链路)
        for d in matches:
            if d.get("product_id") == prefer_pid:
                return d["path"]
        return None
    return matches[0]["path"]


def _device_present(path):
    """判断某 HID path 当前是否还在枚举里(物理上未拔)。用于区分真断开 vs 残留条目。"""
    if not path:
        return False
    import hid
    try:
        return any(d.get("path") == path for d in hid.enumerate())
    except Exception:
        return False


def _link_priority_order(wired_pids=None, wireless_pids=None, exclude_path=None):
    """返回应尝试握手的音频链路 HID dict 列表(已排序):
    顺序 = 有线 > 无线 > 未知(兜底)。"有线/无线"由 _classify_link 决定(主依据 product_string
    里的 '2.4G'/'Mouse', 配置里的 wired_pids/wireless_pids 仅作手动兜底), 因此同系列换
    PID(ED1B/ED00...)也能自动识别并"有线优先"。exclude_path 用于排除刚拔掉的残留条目,
    使其不进入候选。"""
    matches = _enumerate_ajazz(AUDIO_USAGE_PAGE)
    if exclude_path:
        matches = [m for m in matches if m.get("path") != exclude_path]
    rank = {"wired": 0, "wireless": 1, "unknown": 2}
    return sorted(matches, key=lambda d: rank[_classify_link(d, wired_pids, wireless_pids)])


def _live_link(wired_pids=None, wireless_pids=None):
    """返回当前真实在线的音频链路 HID dict(基于激活握手确认); 都不在线返回 None。

    按 _link_priority_order 的顺序(有线优先, 但"有线/无线"靠 product_string 分类而非写死
    PID)逐个试握手。任一路握手成功即返回其 dict; 残留的 HID 条目握手必然失败, 被自然跳过。
    这样无论"插线(无线->有线)"还是"拔线(有线->无线)"都能稳定切换, 且同系列未知 PID
    鼠标也能用。"""
    for d in _link_priority_order(wired_pids=wired_pids, wireless_pids=wireless_pids):
        c = arm_mouse(d.get("product_id"))
        if c is not None:
            try:
                c.close()
            except Exception:
                pass
            return d
    return None


def _find_command_paths(audio_pid=None):
    """返回候选命令通道 HID path 列表(按常见 usage_page 优先级排序)。

    同系列不同 PID 的鼠标, 命令通道 usage_page 可能不同: ED03/ED05 实测用 0xFFA0,
    ED00 诊断显示 0xFFB1/0xFFDF。若只固定查 0xFFA0, 新 PID 鼠标会因找不到命令通道而
    握手失败, 最终报"未找到音频接口"。这里先按 CMD_USAGE_PAGES 顺序枚举, 再把同 PID 下
    非音频、非通用桌面的接口作为兜底, 保证兼容各种固件。"""
    import hid
    paths = []
    seen = set()
    if audio_pid is not None:
        for up in CMD_USAGE_PAGES:
            for d in hid.enumerate():
                if (d.get("vendor_id") == VID and
                    d.get("product_id") == audio_pid and
                    d.get("usage_page") == up):
                    p = d["path"]
                    if p not in seen:
                        paths.append(p)
                        seen.add(p)
        # 兜底: 同 PID 下非音频、非通用桌面的接口
        skip = {AUDIO_USAGE_PAGE, 0x0001}
        for d in hid.enumerate():
            if (d.get("vendor_id") == VID and
                d.get("product_id") == audio_pid and
                d.get("usage_page") not in skip and
                d["path"] not in seen):
                paths.append(d["path"])
                seen.add(d["path"])
    if not paths:
        # 没有指定 PID 或没筛到: 回退到旧行为, 任意找一个已知命令通道
        for up in CMD_USAGE_PAGES:
            p = find_hid(up)
            if p and p not in seen:
                paths.append(p)
                seen.add(p)
                break
    return paths


def arm_mouse(audio_pid=None):
    """打开命令集合并发送激活序列。返回保持打开的句柄 (需存活整个运行期); 握手失败返回 None。

    audio_pid: 可选, 指定要激活的音频设备 product_id。激活序列必须发给"当前正在用的"
    那条物理链路对应的命令通道, 否则会发到已拔掉/休眠的设备上、语音模式实际没打开。
    典型坑: 三模鼠标 无线(ED05)<->有线(ED03) 热切换时, 若仍按固定优先序(有线优先)去
    arm, 切回无线那一刻有线设备可能还在 HID 枚举里残留, 导致错 arm 到旧的有线命令通道、
    无线链路收不到 voice-ON -> 表现为"切回无线没声音"。传 audio_pid 可锁定到正确链路。

    关键: 用"能否完成整段激活握手(每条命令都写成功、且读 0x0A 应答不抛 OSError)"来判断
    链路是否真实在线, 而非仅凭"出现在枚举里"。Windows 拔线后 HID 条目会残留一小段时间,
    但残留设备写/读必然失败 -> 握手通不过 -> 被判定为不在线 -> 自动切回真正在线的链路。

    命令通道 discovery: 不再只认固定 0xFFA0, 而是按 _find_command_paths 返回的列表逐个
    尝试, 兼容同系列不同 PID 鼠标使用不同命令 usage_page 的情况。"""
    import hid
    paths = _find_command_paths(audio_pid)
    if not paths:
        return None
    for path in paths:
        dev = hid.device()
        try:
            dev.open_path(path)
        except OSError:
            continue
        ok = True
        try:
            for p in ARM_SEQ:
                if dev.write(p) != 64:
                    ok = False
                    break
                time.sleep(0.02)
                try:
                    dev.read(64, timeout_ms=300)  # 读 0x0A 应答
                except OSError:
                    ok = False
                    break
        except OSError:
            ok = False
        if ok:
            return dev
        try:
            dev.close()
        except Exception:
            pass
    return None


def _connect_audio(exclude_path=None, wired_pids=None, wireless_pids=None, log=None):
    """打开鼠标音频接口 + 命令通道。返回 (audio_dev, cmd_dev, path, product_id, product_string)。
    命令通道(arm_mouse)发送激活序列, 是让鼠标开始推送音频的前提。

    按 _link_priority_order 已排好序的候选(有线优先, 但分类靠 product_string)逐个握手:
    有线条目若是拔线后的残留(握手失败)则被跳过、退而用无线, 故"插线自动切有线、拔线自动
    切回无线"都稳定, 不受 Windows HID 枚举滞后(残留条目)影响。

    若完全无法连接, 会抛出带诊断信息的 RuntimeError, 方便用户/开发者判断是"没插鼠标"
    还是"找到了音频接口但命令通道握手失败(新 PID/固件未适配或命令通道被占用)"。

    exclude_path: 仍保留兼容参数, 用于显式排除刚拔掉的旧 path(可选)。"""
    import hid
    candidates = _link_priority_order(exclude_path=exclude_path,
                                      wired_pids=wired_pids, wireless_pids=wireless_pids)
    if not candidates:
        if callable(log):
            log("未找到鼠标音频 HID 接口 (usage_page=0xFFAA)。请确认鼠标已连接(无线接收器或数据线)。")
        return (None, None, None, None, None)
    tried = []
    for d in candidates:
        pid = d.get("product_id")
        ps = d.get("product_string") or ""
        # 握手确认这条链路真实在线(残留设备会握手失败); 不通则试下一条
        cmd_dev = arm_mouse(pid)
        if cmd_dev is None:
            tried.append("PID=0x%04X (%s) 命令通道握手失败" % (pid, ps))
            continue
        adev = hid.device()
        try:
            adev.open_path(d["path"])
        except OSError as e:
            cmd_dev.close()
            tried.append("PID=0x%04X 音频接口打开失败: %s" % (pid, e))
            continue
        adev.set_nonblocking(False)
        return adev, cmd_dev, d["path"], pid, ps
    detail = "; ".join(tried)
    if callable(log):
        log("找到鼠标音频接口但无法建立连接。可能原因: 1) 该 PID/固件的命令通道尚未适配; "
            "2) 鼠标官方软件正在运行并独占了设备; 3) 设备枚举残留导致握手失败。诊断: %s" % detail)
    return (None, None, None, None, None)


def _ffmpeg_path():
    """[已废弃] ffmpeg 已由 libsbc.dll 取代, 保留占位以避免外部引用报错。"""
    return _libsbc_path()


def ffmpeg_available():
    """[已废弃] 见 libsbc_available()。"""
    return libsbc_available()


def list_devices():
    import sounddevice as sd
    for i, d in enumerate(sd.query_devices()):
        if d["max_output_channels"] > 0:
            print(f'  [{i}] {d["name"]}  (rate={int(d["default_samplerate"])})')


def list_hid():
    """列出全部 HID 设备, 重点标注 AJAZZ(VID_363C) 与音频 usage_page(0xFFAA)。
    插上线后用它确认有线模式下鼠标的真实 VID/PID / 哪个接口是音频。"""
    import hid
    devs = hid.enumerate()
    print("HID 设备清单 (VID PID  接口  usage_page  厂商/产品):")
    for d in devs:
        vid = d.get("vendor_id"); pid = d.get("product_id")
        iface = d.get("interface_number")
        up = d.get("usage_page")
        manu = d.get("manufacturer_string") or ""
        prod = d.get("product_string") or ""
        mark = ""
        if vid == VID:
            mark = "  <== AJAZZ"
            cl = _classify_link({"product_id": pid, "product_string": prod})
            if cl == "wired":
                mark += "  [有线]"
            elif cl == "wireless":
                mark += "  [无线]"
        if up == AUDIO_USAGE_PAGE:
            mark += "  [音频接口]"
        print(f'  {vid:04X} {pid:04X}   #{iface}  0x{up:04X}  {manu} {prod}{mark}')
    print("\n提示: 音频接口需 usage_page=0xFFAA。若插线后看不到 AJAZZ 或没有该接口, "
          "说明此鼠标有线的 USB 未暴露语音 HID(硬件限制), 只能无线用语音。")


class PcmPump(threading.Thread):
    """从 ffmpeg stdout 读 PCM, 供声卡输出流消费; 带抖动缓冲。"""

    def __init__(self, proc):
        super().__init__(daemon=True)
        self.proc = proc
        self.buf = bytearray()
        self.cond = threading.Condition()
        self.eof = False

    def run(self):
        while True:
            chunk = self.proc.stdout.read(480)  # 240 samples @16k = 15ms
            if not chunk:
                with self.cond:
                    self.eof = True
                    self.cond.notify_all()
                return
            with self.cond:
                self.buf += chunk
                self.cond.notify_all()

    def read(self, n):
        with self.cond:
            while len(self.buf) < n and not self.eof:
                self.cond.wait(0.5)
            out = bytes(self.buf[:n])
            del self.buf[:n]
            return out

    def read_available(self, n):
        """非阻塞: 只取缓冲里现有的数据。"""
        with self.cond:
            out = bytes(self.buf[:n])
            del self.buf[:n]
            return out


def main():
    ap = argparse.ArgumentParser()
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--list", action="store_true")
    g.add_argument("--list-hid", dest="list_hid", action="store_true",
                   help="列出全部 HID 设备的 VID/PID/接口/usage_page (排错用: 插线后看鼠标真实身份)")
    g.add_argument("--play", action="store_true")
    g.add_argument("--file", metavar="WAV")
    g.add_argument("--cable", metavar="NAME", help="虚拟声卡输入设备名, 如 'CABLE Input'")
    ap.add_argument("--hotkey", choices=sorted(HOTKEYS), default=None,
                    help="按住语音键时同步按住的键盘热键")
    ap.add_argument("--driver", choices=("sendinput", "interception"), default="sendinput",
                    help="热键注入方式; 目标软件识别不到时换 interception")
    args = ap.parse_args()

    if args.list:
        list_devices()
        return

    if args.list_hid:
        list_hid()
        return

    if not _single_instance():
        sys.exit("mousemic 已在运行中, 请先关闭已有窗口再启动。", flush=True)

    # 用握手确认的在线链路(有线优先, 残留设备自动跳过), 与 GUI 运行期行为保持一致
    dev, cmd_dev, path, pid, ps = _connect_audio(log=print)
    if dev is None:
        sys.exit("未找到鼠标音频 HID 接口 (usage_page=0xFFAA)。请确认鼠标已连接(无线接收器或数据线)。")
    if cmd_dev:
        print("已发送激活序列 (语音模式 ON)。", flush=True)
    else:
        print("警告: 未找到命令通道, 若按键无音频请重新插拔接收器或先运行一次官方软件。", flush=True)
    print("已连接鼠标音频接口 (%s模式)。按住鼠标语音键开始说话, Ctrl+C 退出。" % _classify_label({"product_id": pid, "product_string": ps}), flush=True)

    pump = None

    stream = None
    wav_f = None
    hotkey = None
    try:
        if args.play or args.cable:
            import sounddevice as sd
            device = None
            if args.cable:
                for i, d in enumerate(sd.query_devices()):
                    if args.cable.lower() in d["name"].lower() and d["max_output_channels"] > 0:
                        device = i
                        break
                if device is None:
                    sys.exit(f"找不到输出设备 '{args.cable}', 用 --list 查看。")
            stream = sd.RawOutputStream(samplerate=SAMPLE_RATE, channels=1,
                                        dtype="int16", device=device, blocksize=480)
            stream.start()
        elif args.file:
            import wave
            wav_f = wave.open(args.file, "wb")
            wav_f.setnchannels(1)
            wav_f.setsampwidth(2)
            wav_f.setframerate(SAMPLE_RATE)

        n_pkts = 0
        if args.hotkey:
            cls = HotKeyInterception if args.driver == "interception" else HotKey
            hotkey = cls(args.hotkey)
            print(f"联动热键: {args.hotkey} [{args.driver}] (按住语音键 = 按住该键)", flush=True)
        last_audio = 0.0
        while True:
            try:
                rep = dev.read(64, timeout_ms=200)
            except OSError:
                print("\n鼠标连接中断, 请重新插入接收器后重启。", flush=True)
                break
            if len(rep) == 64 and rep[0] == REPORT_ID and rep[2] == 0x39:
                if pump is None:
                    # 进程内 libsbc 解码, 第一帧到达即启动
                    pump = MsbcPump()
                pump.write(bytes(rep[3:60]))
                n_pkts += 1
                last_audio = time.time()
                if hotkey:
                    hotkey.press()
                if n_pkts % 125 == 0:
                    print(f"\r音频流中... {n_pkts * 0.008:.1f}s", end="", flush=True)
            # 转发已解码的 PCM (非阻塞, 缓冲里没有就跳过)
            if pump is not None:
                pcm = pump.read_available(960)
                if pcm:
                    if stream:
                        stream.write(pcm)
                    if wav_f:
                        wav_f.writeframesraw(pcm)
            if hotkey and hotkey.down and time.time() - last_audio > HOTKEY_IDLE_TIMEOUT:
                hotkey.release()
                print("\n(已松开热键)", end="", flush=True)
    except KeyboardInterrupt:
        pass
    finally:
        print()
        if hotkey:
            hotkey.close()  # 防止退出时热键卡住
        if pump is not None:
            # 先排空缓冲里的尾音, 再关闭解码器
            rest = pump.read(1 << 20)
            if rest:
                if stream:
                    stream.write(rest)
                if wav_f:
                    wav_f.writeframesraw(rest)
            pump.close()
        if stream:
            stream.stop(); stream.close()
        if wav_f:
            wav_f.close()
            print(f"已保存 {args.file}")
        if cmd_dev:
            cmd_dev.close()
        dev.close()


def run_bridge(config, stop_event, log=None):
    """供 GUI 导入调用的桥接函数。
    config: dict(hotkey=None, driver='sendinput', mode='play', cable_device='CABLE Input')
    stop_event: threading.Event, set() 后干净退出。
    log: 可选回调, 接收单行字符串; 不传则打印到控制台(供 CLI 用)。

    设备策略: 按 product_string 自动分类有线/无线(无线带 '2.4G'、有线带 'Mouse',
    与 PID 取值无关), 已知鼠标保持"有线优先", 同系列换 PID(ED03/ED05/ED1B/ED00...)也能
    自动识别; 用"激活握手是否成功"判断链路真实在线, 而非仅凭 HID 枚举
    (Windows 拔线后条目会残留)。运行期每 ~1.5s 探测一次:
      - 插线(无线状态下) -> 有线握手成功 -> 自动切到有线;
      - 拔线(有线状态下) -> 有线握手失败被跳过 -> 自动切回无线;
      - 两条都失败(全断开) -> 等待重连, 不退出循环。
    残留的 HID 条目因握手通不过会被正确忽略, 所以所有方向的切换都稳定。
    """
    out = log if callable(log) else (lambda m: print(m, flush=True))

    # 已知有线/无线 PID 集合: 优先用配置里手动指定的(若用户为某未知 PID 标注了有线/无线),
    # 否则用默认 {ED03}/{ED05}。未知 PID 仍会自动兼容(落到"任意能握手")。
    def _coerce_pids(lst):
        s = set()
        for v in (lst or []):
            try:
                s.add(int(v, 16) if isinstance(v, str) and v.lower().startswith("0x") else int(v))
            except (ValueError, TypeError):
                pass
        return s
    wired_pids = _coerce_pids(config.get("wired_pids"))
    wireless_pids = _coerce_pids(config.get("wireless_pids"))

    def mode_label(d):
        """d: HID dict(含 product_id / product_string)。按有线/无线分类打标签。"""
        return _classify_label(d, wired_pids, wireless_pids)

    def cur_dev():
        return {"product_id": current_pid, "product_string": current_ps}

    out("正在连接鼠标音频接口...")
    dev, cmd_dev, current_path, current_pid, current_ps = _connect_audio(wired_pids=wired_pids, wireless_pids=wireless_pids, log=out)
    if dev is None:
        raise RuntimeError("未找到鼠标音频 HID 接口 (usage_page=0xFFAA)。请确认鼠标已连接(无线接收器或数据线)。")

    mode = config.get("mode", "play")
    cable_name = config.get("cable_device", "CABLE Input")
    hotkey_name = config.get("hotkey")
    driver = config.get("driver", "sendinput")

    stream = None
    if mode in ("play", "cable"):
        import sounddevice as sd
        device = None
        if mode == "cable":
            for i, d in enumerate(sd.query_devices()):
                if cable_name.lower() in d["name"].lower() and d["max_output_channels"] > 0:
                    device = i
                    break
            if device is None:
                raise RuntimeError(f"找不到输出设备 '{cable_name}'")
        stream = sd.RawOutputStream(samplerate=SAMPLE_RATE, channels=1,
                                    dtype="int16", device=device, blocksize=480)
        stream.start()

    hotkey = None
    if hotkey_name:
        cls = HotKeyInterception if driver == "interception" else HotKey
        hotkey = cls(hotkey_name)

    def reconnect(reason, exclude_path=None):
        """关闭当前设备并重建音频解码管线, 然后重新连接设备。
        成功返回 True(新句柄写回闭包变量), 失败返回 False。

        exclude_path: 热切换时传"刚拔掉的设备 path"。Windows 拔线后该 HID 条目会在枚举
        里残留一小段时间, 若不排除, 有线优先逻辑会错选这个残留的有线条目, 导致切回无线
        后仍然没声音。传它可强制跳过残留条目, 选到真正在线的无线设备。

        切换/断开后必须重建 libsbc 解码器: mSBC 解码器有状态, 旧设备留下的半截帧
        会使其卡在坏状态、迟迟不重新同步, 表现为"能切换但不出声"; 手动停止再启动之所以
        能用, 正是因为它新建了解码器。这里复刻该行为——换设备时一并丢弃旧解码管线。"""
        nonlocal dev, cmd_dev, current_path, current_pid, current_ps
        nonlocal pump, audio_started, hotkey_engaged
        if hotkey:
            hotkey.release()  # 防止重连时热键卡在按下状态
        # 1) 停掉旧解码管线(否则旧解码器卡在坏状态, 新设备帧进来也不出声)
        if pump is not None:
            pump.close()
        pump = None
        audio_started = False
        hotkey_engaged = False
        # 2) 关旧 HID 设备
        try:
            if dev is not None:
                dev.close()
        except Exception:
            pass
        try:
            if cmd_dev is not None:
                cmd_dev.close()
        except Exception:
            pass
        dev = cmd_dev = current_path = current_pid = current_ps = None
        # 3) 重连新设备(排除刚拔掉的残留设备)
        adev, cdev, path, pid, ps = _connect_audio(exclude_path=exclude_path, wired_pids=wired_pids, wireless_pids=wireless_pids, log=out)
        if adev is None:
            return False
        dev, cmd_dev, current_path, current_pid, current_ps = adev, cdev, path, pid, ps
        return True

    pump = None
    n_pkts = 0
    last_audio = 0.0
    audio_started = False
    hotkey_engaged = False
    last_probe = 0.0
    PROBE_INTERVAL = 1.5
    out("已连接鼠标音频接口 (%s模式), 等待语音键..." % mode_label(cur_dev()))
    try:
        while not stop_event.is_set():
            now = time.time()
            if now - last_probe >= PROBE_INTERVAL:
                last_probe = now
                # 用握手确认的在线链路做探测, 而非看枚举(枚举会残留)。有线优先。
                live = _live_link(wired_pids=wired_pids, wireless_pids=wireless_pids)
                if live is None:
                    # 两条链路都握手失败(同时没插线、也没连无线) -> 等待重连
                    if current_path is not None and dev is not None:
                        out("鼠标已断开, 等待重新连接...")
                    if reconnect("gone"):
                        out("已重新连接 (%s模式)。" % mode_label(cur_dev()))
                    else:
                        time.sleep(0.5)
                    continue
                if live["path"] != current_path:
                    # 应使用的链路变了: 插线 -> 有线优先; 拔线 -> 无线。两条方向都稳。
                    out("检测到链路变化, 切换到%s模式..." % mode_label(live))
                    if reconnect("switch"):
                        out("已切换至%s模式。" % mode_label(cur_dev()))
                    else:
                        out("切换失败, 继续重试...")
                        time.sleep(0.3)
                    continue
            try:
                rep = dev.read(64, timeout_ms=200)
            except OSError:
                out("鼠标连接中断, 尝试重新连接...")
                # 当前设备已物理断开: 排除它的旧 path, 避免重连时选回残留的有线条目
                if reconnect("oserror", exclude_path=current_path):
                    out("已重新连接 (%s模式)。" % mode_label(cur_dev()))
                else:
                    time.sleep(0.5)
                continue
            if len(rep) == 64 and rep[0] == REPORT_ID and rep[2] == 0x39:
                if pump is None:
                    pump = MsbcPump()
                    out("检测到语音输入, 已启动音频解码。")
                    audio_started = True
                pump.write(bytes(rep[3:60]))
                n_pkts += 1
                last_audio = time.time()
                if hotkey:
                    if not hotkey_engaged:
                        hotkey_engaged = True
                        out("联动热键已激活: 按住语音键 = 按住 %s" % hotkey_name)
                    hotkey.press()
            if pump is not None:
                pcm = pump.read_available(960)
                if pcm and stream:
                    stream.write(pcm)
            if hotkey and hotkey.down and time.time() - last_audio > HOTKEY_IDLE_TIMEOUT:
                hotkey.release()
    finally:
        if audio_started:
            out("已处理 %d 个语音包。" % n_pkts)
        if stop_event.is_set():
            out("桥接已正常停止。")
        else:
            out("桥接已退出。")
        if hotkey:
            hotkey.close()
        if pump is not None:
            pump.close()
        if stream:
            stream.stop()
            stream.close()
        if cmd_dev:
            cmd_dev.close()
        if dev:
            dev.close()


if __name__ == "__main__":
    main()
