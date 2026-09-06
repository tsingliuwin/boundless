# 测试日志 — 2026-09-06 全量补齐轮

> 循环：测试方案（docs/test-plan.md）→ 实现 → 测试 → 分析 → 修复 → 优化方案 → 再测试
> 环境：macOS 15 (arm64)，stable rust，`cargo test`（lib 228 + integration 5）

## 第 0 轮：基线

- 提交 `536e1f4` 时点：216 个单元测试全绿；`tests/` 目录不存在；`board.rs` 仅 1 个测试。

## 本轮新增测试（17 个）

| 层 | 文件 | 用例 |
|---|---|---|
| L1 | `render/cache.rs` | U-RD-011 指纹参数化矩阵（15 字段逐一翻转）+ 阴影偏移中间值敏感 |
| L1 | `render/rough.rs` | U-RD-001 五种填充出图 + U-RD-002 渐变色标方向/端点 + U-RD-009 退化输入 |
| L1 | `assets.rs`（新模块） | 存储唯一命名/自动建目录/解码缓存命中/缺失损坏返回 None/扩展名规范化（5 个） |
| L1 | `board.rs` | U-BR-003 fit_image 尺寸计算 + U-BR-002 渐变按钮缺省底色 |
| L2 | `tests/integration.rs`（新） | I-SC-001 旧场景兼容 + 图片元素往返 + 坏文件拒绝；I-HI-001 历史×场景；I-SC-002 回放落位 |

配套重构（为可测性）：新建 `src/assets.rs::AssetStore`（替代 BoardView 内联的资产方法）；
`board.rs` 抽出纯函数 `fit_image` / `apply_fill_style` 并接线回工具栏与插入流程；
`rough.rs` 抽出 `gradient_background`。

## 第 1 轮：运行结果与分析

**运行**：`cargo test` → lib 3 failed / integration 2 failed（红 5）。

| # | 失败 | 分析结论 | 处置 |
|---|---|---|---|
| 1 | `fingerprint_changes_for_every_style_field`（预期内，先写测试后修码） | **真 bug #1：`ElementStyle.shadow` 从未参与指纹哈希**——给已有形状加/改阴影不会触发几何重建，渲染缓存一直命中旧几何，阴影画不出来（直到元素因其他原因重绘）。代码阅读在写测试阶段即确认 | `fingerprint_into` 补 shadow(dx,dy) 哈希（cache.rs） |
| 2 | `all_fill_styles_produce_paths` | 测试断言错误：「实」按设计走双遍密排 FillSketch（视觉近实心），只有「渐」走 FillPath（Windows fill 不可见约束）。与既有测试 `solid_fill_ellipse_emits_fillpath` 的语义对齐后修正期望表 | 修测试 |
| 3 | `degenerate_shapes_do_not_panic` | 测试断言错误：单点线条按设计渲染为圆点（笔触点按），不是 Empty；零点才是 Empty | 修测试 |
| 4 | `replay_places_ops_where_their_args_say`（ops_applied=2≠3，元素却 3 个全落位） | **真 bug #2：`eval.rs::push_shape`（矩形/椭圆/菱形共用）成功路径漏 `ops_applied += 1`**。AI 评测的统计（"narrating without drawing fails" 等检查）长期低估画图操作。现有 29 个 eval 测试只断言评分结果、不断言计数，故未发现 | push_shape 成功路径补计数（eval.rs） |
| 5 | `history_over_mixed_mutations_restores_every_step` | 测试逻辑错误：undo 栈存"变更前"状态，快照却是"变更后"采集，逐帧对齐错位 | 修测试（第 k 次 undo 对齐 snapshots[5-k]，第 6 步对齐空场景） |

**运行方法教训**：尝试用 python 脚本临时改码做变异验证时，坏替换 + `git checkout` 把未提交修复一起回滚，
丢失两次编辑。教训：临时变异一律用 Edit 工具，禁止用脚本改码后 checkout 回滚。

## 变异验证（测试有效性）

对真 bug #1 的修复做变异检查：把 shadow 哈希替换为 `h.write_bool(false)`（假装字段不存在）→
`fingerprint_changes_for_every_style_field` 立即红，报文精确指向 `shadow` 字段 → 恢复实现后转绿。
证明该矩阵测试对此类"新增字段忘接指纹"的回归有捕获能力。

## 第 2 轮：全量回归

```
cargo test
  lib:         228 passed, 0 failed
  integration:   5 passed, 0 failed
（合计 233；基线 216 → 净增 17）
```

## 遗留与风险

1. **E2E（L4）本轮未跑**：设备锁屏，`scripts/e2e` 坐标未校准。解锁后按 README 校准并跑
   e2e-001/002/003（渐变回归主用例）。
2. push_shape 计数修复会让**依赖 ops_applied 的评分曲线整体上移**——下次真实模型评测时
   阈值可能需要再校准（已在 test-plan §4.5 AI-EV-002 备注）。
3. `board.rs` 仍有大量交互逻辑无法直接测（BoardView 依赖 GPUI 上下文）——按方案 R3 继续抽纯函数。
