# Boundless E2E（macOS UI 自动化）

测试用例定义见 [`docs/test-plan.md`](../../docs/test-plan.md) §4.4。本目录是 L4 层的执行载体。
2026-09-06 已实跑全绿（e2e-001/002/007），机制均为实测结论，见文末。

## 前置条件

1. Release 二进制已构建：`cargo build --release`。
2. `python3 -c "import PIL"` 可用（仅 Pillow 一个第三方依赖）。
3. 屏幕解锁最好；锁屏/熄屏也可以 —— `kit.shot()` 拍到黑帧会自动
   `caffeinate -u` 唤醒并重拍（最多 3 次）。

## 运行

```bash
python3 scenarios.py                 # 全部场景（顺序执行，007 依赖 002 画的矩形）
python3 scenarios.py e2e-001 e2e-002 # 指定场景
python3 scenarios.py calibrate       # 打印自动定位的工具栏按钮坐标
```

失败/中间截图统一落在 `/tmp/e2e-artifacts/`；应用侧事件日志在
`/tmp/e2e-app.log`（`BOUNDLESS_E2E_LOG` 门控，`launch()` 自动打开）。

## 隔离

`kit.launch()` 以 `BOUNDLESS_HOME=/tmp/boundless-e2e-home` 启动应用
（应用侧 `src/ai/store.rs::data_dir()` 支持该环境变量），每次 `fresh`
启动清空该目录 —— 测试永远在空白工作区跑，不碰用户真实数据。

## 已验证的机制（踩坑实录，勿回退）

- GPUI 无辅助功能树 → 不能用 AX API 定位控件，只能坐标 + 像素断言。
- **CGPoint 必须用 ctypes.Structure 按值传**：arm64 上走浮点寄存器，
  声明成 `c_double*2` 数组会按指针传 → 事件发到垃圾坐标甚至 SIGSEGV。
- **按住拖动必须发 `kCGEventLeftMouseDragged`（type 6）**，Moved（type 5）
  是无按键语义；down 前先 move 到起点（GPUI 需要 hover 命中）。
- **合成鼠标事件之后 System Events 的 `keystroke` 会被吞**（与间隔无关），
  键盘一律走 `CGEventCreateKeyboardEvent`（`kit.key_code`）。
- 合成点击 down→up 间隔 ≥60ms（`kit.CLICK_GAP_S`），瞬时事件被 GPUI 丢弃。
- **工具栏坐标自动定位**（`scenarios.find_toolbar_buttons`）：截图顶部找
  「横向深色行带」，按连续段分组（菜单栏底部分隔线是全宽 1-2px 暗线，
  必须按段过滤），选图标簇最多的一段，簇序即按钮序（`BUTTON_ORDER`）。
  校准前必须把 boundless 拉到前台（`kit.activate`），否则拍到别的应用。
- **截图是 retina 2x**；逻辑坐标 = 像素 / 2；应用收到的事件坐标是
  **窗口坐标**（全局坐标 Y 减菜单栏高度 ~25px），断言时注意换算。
- 显示器睡眠/用户锁屏会让截图全黑、事件不达：`shot()` 黑帧自愈 +
  import 时 `caffeinate -u` 唤醒、`-d` 持有防睡眠断言（用户手动锁屏
  无解，重跑即可）。
- 墨迹阈值：默认灰度 <170（橙色墨迹灰度 ~123-135，选中框 ~200-228）。
