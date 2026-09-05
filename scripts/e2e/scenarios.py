#!/usr/bin/env python3
"""E2E 场景定义（用例见 docs/test-plan.md §4.4）。

坐标说明：全局点 = 逻辑分辨率（1440x900 屏即 0..1440/0..900）。
工具栏各按钮位置随窗口布局变化 —— 首次在新环境运行前，先运行
`python3 scenarios.py calibrate` 人工核对一遍 COORDS 并修正。
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import kit  # noqa: E402

# 逻辑分辨率下的近似坐标（retina 截图坐标 = 逻辑坐标 × 2）。
COORDS = {
    "toolbar.select": (696, 54),
    "toolbar.rect": (773, 54),
    "toolbar.pen": (973, 54),
    "canvas.center": (720, 450),
    "canvas.draw_from": (560, 330),
    "canvas.draw_to": (880, 570),
}


class Result:
    def __init__(self, ok: bool, detail: str):
        self.ok, self.detail = ok, detail

    def __repr__(self):
        return f"{'PASS' if self.ok else 'FAIL'}  {self.detail}"


def e2e_001_startup_smoke() -> Result:
    """启动冒烟：窗口出现、工具栏区域非纯白、日志无 panic。"""
    kit.launch(fresh=True)
    im = kit.shot("e2e-001")
    g = kit.gray_region(im, (1300, 80, 3000, 260))  # 顶部工具栏带（retina 坐标）
    luma = kit.mean_luma(g)
    ok = luma < 250 and not kit.log_has_panic()
    return Result(ok, f"toolbar luma={luma:.1f} (<250)，panic={kit.log_has_panic()}")


def e2e_002_draw_rect() -> Result:
    """画矩形：拖动后画布出现墨迹行。"""
    c = COORDS
    kit.click(*c["toolbar.rect"])
    time.sleep(0.3)
    kit.drag(*c["canvas.draw_from"], *c["canvas.draw_to"])
    im = kit.shot("e2e-002")
    g = kit.gray_region(im, tuple(v * 2 for v in (500, 280, 940, 620)))
    rows = kit.dark_row_count(g)
    return Result(rows > 3, f"墨迹行数={rows} (>3)")


def e2e_007_move_undo() -> Result:
    """选择移动 + 撤销还原。"""
    c = COORDS
    before = kit.gray_region(kit.shot("e2e-007-before"), tuple(v * 2 for v in (500, 280, 940, 620)))
    kit.click(*c["toolbar.select"])
    kit.drag(*c["canvas.draw_from"], *c["canvas.draw_to"])
    time.sleep(0.3)
    after_move = kit.gray_region(kit.shot("e2e-007-moved"), tuple(v * 2 for v in (500, 280, 940, 620)))
    import subprocess

    subprocess.run(
        ["osascript", "-e", 'tell application "System Events" to keystroke "z" using command down'],
        check=True,
    )
    time.sleep(0.3)
    after_undo = kit.gray_region(kit.shot("e2e-007-undo"), tuple(v * 2 for v in (500, 280, 940, 620)))
    moved = list(before.getdata()) != list(after_move.getdata())
    restored = list(before.getdata()) == list(after_undo.getdata())
    return Result(moved and restored, f"移动生效={moved}，撤销还原={restored}")


SCENARIOS = {
    "e2e-001": ("启动冒烟", e2e_001_startup_smoke),
    "e2e-002": ("画矩形", e2e_002_draw_rect),
    "e2e-007": ("移动+撤销", e2e_007_move_undo),
}


def main() -> int:
    which = sys.argv[1:] or ["e2e-001"]
    failures = 0
    for name in which:
        if name == "calibrate":
            im = kit.shot("calibrate")
            im.save(kit.ARTIFACTS / "calibrate-full.png")
            print(f"已存 {kit.ARTIFACTS / 'calibrate-full.png'}，人工核对后更新 COORDS")
            return 0
        title, fn = SCENARIOS[name]
        result = fn()
        print(f"{name} {title}: {result}")
        failures += 0 if result.ok else 1
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
