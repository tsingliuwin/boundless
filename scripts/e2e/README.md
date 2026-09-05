# Boundless E2E（macOS UI 自动化）

测试用例定义见 [`docs/test-plan.md`](../../docs/test-plan.md) §4.4。本目录是 L4 层的执行载体。

## 前置条件

1. 屏幕已解锁（锁屏时截图只拍到锁屏，用例必然失败）。
2. Release 二进制已构建：`cargo build --release`。
3. `python3 -c "import PIL"` 可用（仅 Pillow 一个第三方依赖）。
4. 首次在新机器运行：`python3 scenarios.py calibrate` 存一张全屏图，
   对照更新 `scenarios.py` 里 `COORDS` 的工具栏/画布坐标（逻辑分辨率，非 retina 像素）。

## 运行

```bash
python3 scenarios.py                 # 全部场景
python3 scenarios.py e2e-001 e2e-002 # 指定场景
```

失败/中间截图统一落在 `/tmp/e2e-artifacts/`。

## 已验证的机制（为什么这样写）

- GPUI 无辅助功能树 → 不能用 AX API 定位控件，只能坐标 + 像素断言。
- 合成点击 down→up 必须间隔 ≥60ms，否则 GPUI 丢弃事件（`kit.CLICK_GAP_S`）。
- `screencapture -x` 输出 retina 2x 图；`kit` 里的像素阈值（墨迹 <130、
  白底 ≥242）均按灰度图统计。
- ⚠️ 本骨架的坐标与阈值在提交时未经实跑校准（提交时设备锁屏），
  首次运行请先 `calibrate`，跑通用例后如有阈值偏差按实际截图微调。
