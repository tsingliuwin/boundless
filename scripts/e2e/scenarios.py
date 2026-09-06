#!/usr/bin/env python3
"""E2E 场景定义（用例见 docs/test-plan.md §4.4）。

坐标策略：不手写死坐标 —— 每次运行前从截图自动定位工具栏
（图标簇检测，簇顺序即按钮序），画布动作用固定的安全区坐标。
首次运行无需人工校准；工具栏布局变更时只需更新 BUTTON_ORDER。
"""

from __future__ import annotations

import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import kit  # noqa: E402

# 工具栏图标从左到右的语义名（与 board.rs 工具栏布局一致，分隔符不产生图标簇）。
BUTTON_ORDER = [
    "select", "hand", "rect", "diamond", "circle", "arrow", "line",
    "pen", "text", "marker", "image",
    "undo", "redo",
    "pages", "folder",
    "boards", "ai",
]

# 画布安全区（逻辑坐标），避开工具栏与底部 AI 输入栏。
CANVAS = {
    "draw_from": (560, 330),
    "draw_to": (880, 570),
}


def find_toolbar_buttons() -> dict[str, tuple[float, float]]:
    """从全屏截图自动定位工具栏按钮，返回 {语义名: (逻辑x, 逻辑y)}。

    原理：工具栏带是屏幕顶部一块「横向连续分布深色图标」的行带；
    带内按列聚合出图标簇，簇中心即按钮位置。截图是 retina 2x，
    逻辑坐标 = 像素坐标 / 2。
    """
    kit.activate()  # 前台必须是 boundless，否则拍到的是别的应用
    im = kit.shot("calibrate-auto").convert("L")
    w, h = im.size
    px = im.load()

    # 1) 找候选行带：上部 500px 内，一行中横向深色像素 ≥ 30 视为候选行。
    #    菜单栏文本行、菜单栏底部分隔线（全宽 1-2px 暗线）也会命中，
    #    所以把候选行按连续段分组，最后按「图标簇数最多」选出真工具栏段。
    def row_dark_count(y: int) -> int:
        return sum(1 for x in range(500, w - 500, 2) if px[x, y] < 150)

    rows = [y for y in range(0, 500) if row_dark_count(y) >= 30]
    if not rows:
        raise SystemExit("未找到工具栏行带（应用是否在前台/全屏？）")

    segments: list[tuple[int, int]] = []
    seg_start = prev = rows[0]
    for y in rows[1:]:
        if y - prev <= 2:
            prev = y
        else:
            segments.append((seg_start, prev))
            seg_start = prev = y
    segments.append((seg_start, prev))

    def clusters_in(y0: int, y1: int) -> list[tuple[int, int]]:
        out: list[tuple[int, int]] = []
        cur: list[int] | None = None
        for x in range(500, w - 500):
            dark = any(px[x, y] < 150 for y in range(y0, y1 + 1))
            if dark:
                cur = [x, x] if cur is None else [cur[0], x]
            else:
                if cur and cur[1] - cur[0] >= 4:
                    out.append(tuple(cur))
                cur = None
        if cur and cur[1] - cur[0] >= 4:
            out.append(tuple(cur))
        return out

    candidates = [
        (clusters_in(y0, y1), y0, y1)
        for y0, y1 in segments
        if y1 - y0 >= 10  # 图标高度量级；过滤分隔线段
    ]
    if not candidates:
        raise SystemExit("候选行带里没有图标高度的分段")
    clusters, y0, y1 = max(candidates, key=lambda c: len(c[0]))

    if len(clusters) < len(BUTTON_ORDER):
        raise SystemExit(
            f"图标簇数 {len(clusters)} < 预期 {len(BUTTON_ORDER)}，工具栏布局可能变了，请核对 BUTTON_ORDER"
        )

    yc = (y0 + y1) / 2 / 2  # 逻辑 y
    return {
        name: (((a + b) / 2) / 2, yc)
        for name, (a, b) in zip(BUTTON_ORDER, clusters)
    }


class Result:
    def __init__(self, ok: bool, detail: str):
        self.ok, self.detail = ok, detail

    def __repr__(self):
        return f"{'PASS' if self.ok else 'FAIL'}  {self.detail}"


def cmd_z() -> None:
    kit.key_code(6, cmd=True)  # kVK_ANSI_Z=6，走 CGEvent（System Events 会被吞）


def e2e_001_startup_smoke() -> Result:
    """启动冒烟：窗口出现、工具栏图标簇齐全、日志无 panic。"""
    kit.launch(fresh=True)
    buttons = find_toolbar_buttons()
    n = len(buttons)
    ok = n >= len(BUTTON_ORDER) and not kit.log_has_panic()
    return Result(ok, f"工具栏图标簇={n} (≥{len(BUTTON_ORDER)})，panic={kit.log_has_panic()}")


def e2e_002_draw_rect() -> Result:
    """画矩形：选矩形工具 → 拖动 → 画布区域出现墨迹行。"""
    c = CANVAS
    buttons = find_toolbar_buttons()
    kit.click(*buttons["rect"])
    time.sleep(0.4)
    kit.drag(*c["draw_from"], *c["draw_to"])
    time.sleep(0.4)
    im = kit.shot("e2e-002")
    crop = (int(c["draw_from"][0] * 2), int(c["draw_from"][1] * 2),
            int(c["draw_to"][0] * 2), int(c["draw_to"][1] * 2))
    g = kit.gray_region(im, crop)
    rows = kit.dark_row_count(g, threshold=170)  # 170：排除选中框(~200-228)，兼容橙/深色墨迹
    return Result(rows > 3, f"墨迹行数={rows} (>3)")


def e2e_007_move_undo() -> Result:
    """选择移动 + 撤销还原：从矩形中心拖动（移动而非框选），撤销后像素还原。"""
    c = CANVAS
    buttons = find_toolbar_buttons()
    kit.click(*buttons["select"])
    time.sleep(0.3)

    region = (int(c["draw_from"][0] * 2) - 60, int(c["draw_from"][1] * 2) - 60,
              int(c["draw_to"][0] * 2) + 60, int(c["draw_to"][1] * 2) + 60)

    def snap(tag: str) -> list[int]:
        return list(kit.gray_region(kit.shot(tag), region).getdata())

    before = snap("e2e-007-before")
    cx = (c["draw_from"][0] + c["draw_to"][0]) / 2
    cy = (c["draw_from"][1] + c["draw_to"][1]) / 2
    kit.drag(cx, cy, cx + 200, cy + 150)
    time.sleep(0.4)
    after_move = snap("e2e-007-moved")
    cmd_z()
    time.sleep(0.4)
    after_undo = snap("e2e-007-undo")
    moved = before != after_move
    restored = before == after_undo
    return Result(moved and restored, f"移动生效={moved}，撤销还原={restored}")


SCENARIOS = {
    "e2e-001": ("启动冒烟", e2e_001_startup_smoke),
    "e2e-002": ("画矩形", e2e_002_draw_rect),
    "e2e-007": ("移动+撤销", e2e_007_move_undo),
}


def main() -> int:
    which = sys.argv[1:] or list(SCENARIOS)
    if which == ["calibrate"]:
        for k, v in find_toolbar_buttons().items():
            print(f"{k}: ({v[0]:.0f}, {v[1]:.0f})")
        return 0
    # 显示器睡眠会让截图全黑（本机 displaysleep=2min）——运行期间守护显示
    keep_awake = subprocess.Popen(["caffeinate", "-d"])
    try:
        failures = 0
        for name in which:
            title, fn = SCENARIOS[name]
            result = fn()
            print(f"{name} {title}: {result}")
            failures += 0 if result.ok else 1
        return 1 if failures else 0
    finally:
        keep_awake.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
