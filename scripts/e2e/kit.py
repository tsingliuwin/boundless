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

import ctypes
import ctypes.util
import subprocess
import time
from pathlib import Path

from PIL import Image

APP = "boundless"
RELEASE_BIN = Path(__file__).resolve().parents[2] / "target" / "release" / APP
LOG = Path("/tmp/boundless.log")
ARTIFACTS = Path("/tmp/e2e-artifacts")

# 合成事件用的虚拟屏幕坐标（全局点，非 retina 像素）。
CLICK_GAP_S = 0.07  # down→up 间隔，<60ms GPUI 会丢


def _cg():
    path = ctypes.util.find_library("CoreGraphics")
    lib = ctypes.CDLL(path)
    lib.CGEventCreateMouseEvent.restype = ctypes.c_void_p
    lib.CGEventCreateMouseEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_double * 2, ctypes.c_uint32]
    lib.CGEventPost.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
    lib.CFRelease.argtypes = [ctypes.c_void_p]
    return lib


_CG = _cg()
_KCGHIDEventTap = 0
_KCGEventLeftMouseDown = 1
_KCGEventLeftMouseUp = 2
_KCGEventMouseMoved = 5


def click(x: float, y: float) -> None:
    """合成一次左键点击：down→(gap)→up。"""
    pt = (ctypes.c_double * 2)(x, y)
    down = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseDown, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, down)
    time.sleep(CLICK_GAP_S)
    up = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseUp, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, up)
    _CG.CFRelease(down)
    _CG.CFRelease(up)


def move_to(x: float, y: float) -> None:
    pt = (ctypes.c_double * 2)(x, y)
    ev = _CG.CGEventCreateMouseEvent(None, _KCGEventMouseMoved, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, ev)
    _CG.CFRelease(ev)


def drag(x0: float, y0: float, x1: float, y1: float, steps: int = 12) -> None:
    """按下→分步移动→抬起，画一个形状/框选。"""
    pt = (ctypes.c_double * 2)(x0, y0)
    down = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseDown, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, down)
    time.sleep(CLICK_GAP_S)
    for i in range(1, steps + 1):
        t = i / steps
        move_to(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        time.sleep(0.016)
    up = _CG.CGEventCreateMouseEvent(None, _KCGEventLeftMouseUp, pt, 0)
    _CG.CGEventPost(_KCGHIDEventTap, up)
    _CG.CFRelease(down)
    _CG.CFRelease(up)
    time.sleep(0.2)


def key_code(code: int, cmd: bool = False, shift: bool = False) -> None:
    """osascript 按键（已验证：Esc=53、F5=96）。"""
    mods = []
    if cmd:
        mods.append("command down")
    if shift:
        mods.append("shift down")
    using = f" using {{ {' , '.join(mods)} }}" if mods else ""
    subprocess.run(
        ["osascript", "-e", f'tell application "System Events" to key code {code}{using}'],
        check=True,
    )


def keystroke(text: str) -> None:
    subprocess.run(["osascript", "-e", f'tell application "System Events" to keystroke "{text}"'], check=True)


# ---------------------------------------------------------------- screenshots


def shot(tag: str) -> Image.Image:
    """全屏截图并存档。返回 PIL 图（未降采样）。"""
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    path = ARTIFACTS / f"{tag}.png"
    subprocess.run(["screencapture", "-x", str(path)], check=True)
    return Image.open(path)


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
    with LOG.open("wb") as f:
        proc = subprocess.Popen([str(RELEASE_BIN)], stdout=f, stderr=f)
    time.sleep(4)
    return proc


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
