#!/usr/bin/env python3
"""Boundless macOS E2E 工具链。

已验证的机制（详见 docs/test-plan.md 附录 A）：
- GPUI 无辅助功能树 → 一律走「截图像素断言 + 合成事件」。
- 合成鼠标点击必须 down→up 间隔 ≥60ms，GPUI 才会认（瞬时事件被丢弃）。
- screencapture 抓到的是 retina 2x 图（2880x1800），所有像素统计先转灰度。
- 屏幕锁定时截图只会拍到锁屏 —— 跑 E2E 前屏幕必须解锁。

依赖：仅 Pillow（系统 python3 可直接 import PIL）+ macOS 自带 CoreGraphics。
"""

from __future__ import annotations

import atexit
import ctypes
import ctypes.util
import os
import shutil
import subprocess
import time
from pathlib import Path

from PIL import Image

APP = "boundless"
RELEASE_BIN = Path(__file__).resolve().parents[2] / "target" / "release" / APP
LOG = Path("/tmp/boundless.log")
ARTIFACTS = Path("/tmp/e2e-artifacts")
# 应用级隔离：BOUNDLESS_HOME 重定向整个数据树（app.json/workspace/日志），
# E2E 永远在空白工作区跑，不碰用户真实数据。
E2E_HOME = Path("/tmp/boundless-e2e-home")

# 显示器睡眠让截图全黑、事件不达（本机 displaysleep=2min，合成事件不重置
# 空闲计时，且右下热角设了锁屏/睡眠动作）——import 即先唤醒（-u 模拟用户
# 活动），再持有防睡眠断言（-d 只防不醒），进程退出时释放。
subprocess.run(["caffeinate", "-u", "-t", "2"], check=False)
time.sleep(1)
_CAFFEINATE = subprocess.Popen(["caffeinate", "-d"])
atexit.register(lambda: _CAFFEINATE.terminate())

# 合成事件用的虚拟屏幕坐标（全局点，非 retina 像素）。
CLICK_GAP_S = 0.07  # down→up 间隔，<60ms GPUI 会丢


class CGPoint(ctypes.Structure):
    """必须用 Structure 按值传参：arm64 上 CGPoint 走浮点寄存器，
    声明成 c_double*2 数组会按指针传 → 垃圾坐标甚至 SIGSEGV。"""

    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


def _cg():
    path = ctypes.util.find_library("CoreGraphics")
    lib = ctypes.CDLL(path)
    lib.CGEventCreateMouseEvent.restype = ctypes.c_void_p
    lib.CGEventCreateMouseEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint32, CGPoint, ctypes.c_uint32]
    lib.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
    lib.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
    lib.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
    lib.CGEventPost.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
    lib.CFRelease.argtypes = [ctypes.c_void_p]
    return lib


_CG = _cg()
_KCGHIDEventTap = 0
_KCGEventLeftMouseDown = 1
_KCGEventLeftMouseUp = 2
_KCGEventMouseMoved = 5
_KCGEventLeftMouseDragged = 6  # 按住拖动必须发 Dragged，Moved 是无按键的悬停语义


def click(x: float, y: float) -> None:
    """合成一次左键点击：down→(gap)→up。"""
    pt = CGPoint(x, y)
    down = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseDown, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, down)
    time.sleep(CLICK_GAP_S)
    up = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseUp, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, up)
    _CG.CFRelease(down)
    _CG.CFRelease(up)


def move_to(x: float, y: float) -> None:
    ev = _CG.CGEventCreateMouseEvent(None, _KCGEventMouseMoved, CGPoint(x, y), 0)
    _CG.CGEventPost(_KCGHIDEventTap, ev)
    _CG.CFRelease(ev)


def drag_to(x: float, y: float) -> None:
    """按住左键时的中间移动：必须用 LeftMouseDragged 事件。"""
    ev = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseDragged, CGPoint(x, y), 0)
    _CG.CGEventPost(_KCGHIDEventTap, ev)
    _CG.CFRelease(ev)


def drag(x0: float, y0: float, x1: float, y1: float, steps: int = 12) -> None:
    """先悬停到起点（GPUI 需要 hover 命中），按下→分步拖动→抬起。"""
    move_to(x0, y0)
    time.sleep(CLICK_GAP_S)
    down = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseDown, CGPoint(x0, y0), 0)
    _CG.CGEventPost(_KCGHIDEventTap, down)
    time.sleep(CLICK_GAP_S)
    for i in range(1, steps + 1):
        t = i / steps
        drag_to(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        time.sleep(0.016)
    up = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseUp, CGPoint(x1, y1), 0)
    _CG.CGEventPost(_KCGHIDEventTap, up)
    _CG.CFRelease(down)
    _CG.CFRelease(up)
    time.sleep(0.2)


def key_code(code: int, cmd: bool = False, shift: bool = False) -> None:
    """CGEvent 键盘事件。实测：合成鼠标事件之后 System Events 的
    keystroke 会被吞（时序无关），CGEvent 键盘事件始终可靠。
    键值：Esc=53、F5=96、Z=6。"""
    flags = 0
    if cmd:
        flags |= 1 << 20  # kCGEventFlagMaskCommand
    if shift:
        flags |= 1 << 17  # kCGEventFlagMaskShift
    down = _CG.CGEventCreateKeyboardEvent(None, code, True)
    _CG.CGEventSetFlags(down, flags)
    _CG.CGEventPost(_KCGHIDEventTap, down)
    time.sleep(CLICK_GAP_S)
    up = _CG.CGEventCreateKeyboardEvent(None, code, False)
    _CG.CGEventSetFlags(up, flags)
    _CG.CGEventPost(_KCGHIDEventTap, up)
    _CG.CFRelease(down)
    _CG.CFRelease(up)
    time.sleep(0.1)


# ---------------------------------------------------------------- screenshots


def shot(tag: str) -> Image.Image:
    """全屏截图并存档。返回 PIL 图（未降采样）。

    自愈：拍到黑帧（显示器被热角/电源键/用户操作关掉）时，模拟一次用户
    活动唤醒显示器并把光标停到屏幕中心（远离热角），重拍最多 3 次。
    """
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    path = ARTIFACTS / f"{tag}.png"
    im = Image.open(_capture(path))
    for _ in range(3):
        if not _is_black(im):
            return im
        subprocess.run(["caffeinate", "-u", "-t", "2"], check=False)
        move_to(720, 450)  # 光标离开热角
        time.sleep(2.5)
        im = Image.open(_capture(path))
    return im


def _capture(path: Path) -> str:
    subprocess.run(["screencapture", "-x", str(path)], check=True)
    return str(path)


def _is_black(im: Image.Image) -> bool:
    g = im.convert("L")
    hist = g.histogram()
    total = sum(hist) or 1
    return sum(i * c for i, c in enumerate(hist[:8])) / total > 0.9  # >90% 近黑


def gray_region(im: Image.Image, crop: tuple[int, int, int, int]) -> Image.Image:
    """裁剪并转灰度。坐标按给定图尺寸，无缩放。"""
    return im.crop(crop).convert("L")


def dark_row_count(g: Image.Image, threshold: int = 130) -> int:
    """亮度低于阈值的像素所在的行数 —— 粗测「有没有墨迹」。"""
    w, h = g.size
    px = g.load()
    rows = 0
    for y in range(h):
        if any(px[x, y] < threshold for x in range(0, w, 2)):
            rows += 1
    return rows


def mean_luma(g: Image.Image) -> float:
    hist = g.histogram()
    total = sum(hist)
    return sum(i * c for i, c in enumerate(hist)) / max(total, 1)


# -------------------------------------------------------------------- process


def launch(fresh: bool = False) -> subprocess.Popen:
    if running_pid() and fresh:
        subprocess.run(["kill", str(running_pid())], check=False)
        time.sleep(1)
    if not RELEASE_BIN.exists():
        raise SystemExit(f"缺少二进制：{RELEASE_BIN}（先 cargo build --release）")
    if fresh and E2E_HOME.exists():
        shutil.rmtree(E2E_HOME)  # 全新隔离 home：空工作区、无 last_board
    env = {
        **os.environ,
        "BOUNDLESS_HOME": str(E2E_HOME),
        # 应用侧地面真相（app 内 env 门控，无开销）
        "BOUNDLESS_E2E_LOG": "/tmp/e2e-app.log",
    }
    with LOG.open("wb") as f:
        proc = subprocess.Popen([str(RELEASE_BIN)], stdout=f, stderr=f, env=env)
    time.sleep(4)
    activate()
    return proc


def activate() -> None:
    """把 boundless 拉到前台（合成点击只落到最前窗口，必须先激活）。"""
    pid = running_pid()
    if not pid:
        raise SystemExit("boundless 未运行，无法激活")
    subprocess.run(
        ["osascript", "-e",
         f'tell application "System Events" to set frontmost of (first process whose unix id is {pid}) to true'],
        check=True,
    )
    time.sleep(1)


def running_pid() -> int | None:
    out = subprocess.run(["pgrep", "-x", APP], capture_output=True, text=True)
    out = out.stdout.strip()
    return int(out.splitlines()[0]) if out else None


def quit_app() -> None:
    if running_pid():
        subprocess.run(["kill", str(running_pid())], check=False)
        time.sleep(1)


def log_has_panic() -> bool:
    if not LOG.exists():
        return False
    return "panic" in LOG.read_text(errors="ignore")
