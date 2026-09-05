# Boundless 全量自动化测试方案

> 版本：2026-09-06 · 适用分支：main
> 配套脚本：`scripts/e2e/`（macOS UI 自动化工具链）

---

## 1. 目标与原则

1. **回归零逃逸**：任何已修复的 bug 必须留下一个自动化用例（回归测试优先）。
2. **测试金字塔**：单元（快、多）→ 集成（中）→ 渲染断言（中）→ UI E2E（少、关键路径）→ AI 评测（夜间/手动）。
3. **确定性优先**：渲染几何必须种子确定（`el.seed`），凡是能脱离 GPU 断言的，不下沉到像素层。
4. **平台差异显式化**：开发主平台 macOS；CI 单测在 Windows（`ci.yml`）。平台专属行为（手写笔、路径 fill 渲染差异）单独标注，不混入通用用例。

## 2. 现状盘点

### 2.1 已有测试分布（共 216 个，全部为 `src/**` 内联单元测试）

| 模块 | 数量 | 覆盖要点 |
|---|---|---|
| `ai/eval.rs` | 29 | 黑板报/水墨/思维导图三套 rubric 评分、replay 校验 |
| `scene/element.rs` | 20 | 命中测试、serde 往返、rescale、顶点编辑、曲线命中 |
| `render/rough.rs` | 14 | 各形状出路径、种子确定、渐变 FillPath、飞白密度参数、图片无矢量几何 |
| `ai/canvas_ops.rs` | 15 | op serde 往返、颜色解析（hex/数字/null）、文本规范化 |
| `ink/*`（4 文件） | 34 | 采样抽稀、笔压速度模型、ribbon 轮廓、圆点 |
| `scene/mindmap.rs` | 13 | 布局不重叠、连线不交叉、确定性、大图缩放 |
| `ai/panel.rs` | 12 | 工具步骤气泡/芯片预览文案 |
| `render/cache.rs` | 9 | 指纹稳定性/各字段变更敏感、几何缓存命中 |
| `scene/mod.rs` | 7 | 场景文件往返、旧版兼容、topmost 命中、z 序四操作 |
| `ai/*`（tools/store/client/skills/log/agent/settings） | 31 | 参数校验、会话存储、步骤重建、技能 frontmatter |
| `scene/pages.rs` | 5 | 比例解析、横向排布、page_at |
| `text.rs` | 5 | UTF-16 映射（CJK/emoji）、编辑会话、IME |
| `camera.rs` / `history.rs` / `workspace.rs` / `platform.rs` / `updater.rs` | 3+2+4+6+3 | 缩放锚点、undo 往返、工作区扫描、笔分类、更新器 |
| `board.rs` | **1** | ⚠️ 巨型模块（UI/交互/操作应用核心）几乎裸奔 |

### 2.2 已有基础设施

- `ci.yml`：Windows 单测 + release 缓存预热（push main / PR）。
- `scripts/`：`package-macos.sh`（打包）、`win-test.ps1`、`test-text.ps1`（Windows 手工脚本）。
- `ai/eval.rs`：**现成的无头评测引擎** —— `replay()` 把 `CanvasOp` 序列重放为 `VirtualCanvas`，rubric 打分。这是 AI 生成质量回归的基座。
- macOS UI 自动化经验（已验证可行，无 AX 树）：`screencapture -x`（2880×1800 retina）+ PIL 像素统计（墨迹亮度 <130、描边框 ~200-228、白底 ≥242）；CGEvent 点击必须 **down→up 间隔 ≥60-80ms**；`osascript key code`（Esc=53、F5=96）；用户画板在 `~/.boundless/workspace/*.boundless`，`app.json` 的 `last_board` 只能在应用退出期间修改。

### 2.3 主要缺口

1. `board.rs`（约 8000 行：指针交互、op 应用、工具栏、页面、演示、自动保存）仅 1 个测试。
2. 无 `tests/` 集成测试目录：跨模块行为（serde 兼容矩阵、资源存储往返、历史×场景组合）没有载体。
3. 渲染只有"出不出路径"断言，缺几何属性断言（包围盒、渐变色标、指纹矩阵参数化）。
4. UI E2E 完全依赖手工；已验证的 macOS 自动化技巧未沉淀为脚本。
5. CI 无 macOS 作业、无 clippy/fmt/覆盖率门禁。

## 3. 分层架构

```
L5  AI Agent 评测      eval rubric 扩展 + 录制回放 + 真实模型冒烟（夜间/手动）
L4  UI E2E (macOS)     scripts/e2e：真实进程 + 截图像素断言（冒烟 + 关键路径）
L3  渲染断言           ReadyPath 几何/颜色断言 + 指纹参数化矩阵 + 性能预算
L2  集成 (tests/)      serde 兼容矩阵、资源存储、历史×场景、工作区文件 IO
L1  单元 (src/**)      纯函数：几何、布局、解析、状态机（现状主力，继续补缺）
```

失败成本越高、跑得越慢的层放越少用例；每层可独立运行：`cargo test`（L1-L3）、`cargo test --test integration`（L2）、`python3 scripts/e2e/run.py smoke`（L4）。

---

## 4. 测试用例总表

标记说明：状态 = ✅已有 / 🆕待建；优先级 = P0（提交门禁）/ P1（每日）/ P2（每周/夜间）。
用例编号规则：`<层>-<模块>-<序号>`，模块缩写：RD=render、SC=scene、BR=board、AI=ai、WK=workspace、TX=text、PF=platform、UP=updater。

### 4.1 L1 单元测试补缺（`cargo test --lib`）

#### 4.1.1 渲染（RD）

| ID | 用例 | 操作 | 断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| U-RD-001 | 五种 fill_style 出图 | 矩形×{hachure,dense,solid,watercolor,gradient} 生成 `paths_for_element` | 全部非空；solid/gradient 含 `FillPath`，其余含 `FillSketch` | P0 | 部分✅（solid/gradient 已有，hachure/dense/watercolor 🆕） |
| U-RD-002 | 渐变色标方向 | gradient 矩形 fill_color | `Background::Gradient`，180°，stop0=lighten(0.35)、stop1=darken(0.35) | P0 | 🆕 |
| U-RD-003 | fill_params 密度序 | 比较 5 种样式 (gap, weight) | hachure gap > dense > solid；watercolor/gradient 符合预设值 | P1 | 🆕 |
| U-RD-004 | hachure 覆盖参数生效 | `hachure_gap/fill_weight/hachure_angle` 设定后几何 | 排线行数 = f(bounds/gap)；角度改变行方向 | P1 | 🆕 |
| U-RD-005 | dry_width 参数 | 飞白 dry_width=2.0 vs 0.2 | 主笔 PaintOverride.width 线性跟随 | P1 | 🆕 |
| U-RD-006 | 阴影渲染 | 带 `shadow` 的矩形 | 输出在最前多两组深色 opset，且几何被偏移 dx/dy | P1 | 🆕 |
| U-RD-007 | 虚线样式 | dashed 矩形轮廓 | 路径段数显著多于实线（dash 切分） | P2 | 🆕 |
| U-RD-008 | 缩放稳定性 | 同一元素 zoom 0.5/1/2 各 paint | 路径数恒定（几何世界坐标缓存）；屏幕坐标随 zoom 缩放 | P1 | 🆕 |
| U-RD-009 | 退化输入 | 零宽/负尺寸/单点/空 points | 返回 `Empty` 或 dot，不 panic | P0 | 部分✅（点笔已有；形状退化 🆕） |
| U-RD-010 | 图片资产名进指纹 | 同 seed 同 bounds 换 asset 名 | 指纹必变 | P0 | ✅（`image_elements_have_no_vector_geometry`） |
| U-RD-011 | 缓存指纹参数化矩阵 | 对 style 每个字段（含 dry_density/dry_width/line_type/opacity/text_align…）逐字段翻转 | 每次翻转指纹必变（参数化遍历，防新增字段漏哈希） | P0 | 🆕（防"加字段忘指纹"复发） |
| U-RD-012 | 几何生成性能预算 | 1000 个混合元素 `world_geometry` | 总耗时 < 预算（如 2s，release）；缓存二次构建 0 生成 | P2 | 🆕（`#[ignore]` 基准） |

#### 4.1.2 场景与画板核心（SC/BR）

| ID | 用例 | 操作 | 断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| U-SC-001 | Image 元素 serde 往返 | `ElementKind::Image{asset}` → JSON → 反序列化 | 字段无损；旧文件（无 image）照常加载 | P0 | 🆕 |
| U-SC-002 | 图片命中测试 | bounds 内/外/边缘 tol | 内外命中正确 | P0 | 🆕 |
| U-SC-003 | 命中测试矩阵 | 9 种 ElementKind × 内部/边缘/外部 | 全部按各自几何规则 | P1 | 部分✅（rect/ellipse/line 有） |
| U-SC-004 | elements_in / content_bounds | 混合元素集合 | 返回与 bounds 相交全集并集正确 | P1 | 🆕 |
| U-BR-001 | apply_style_to_selection | 多选混合样式 → 设 fill_style | 整组一致变更；`selection_fill_style` 返回 Some(同值)；不一致时返回 None 回退预设 | P0 | 🆕 |
| U-BR-002 | 渐变按钮缺省底色 | 无 background 选中矩形 → 点「渐」 | `background == Some(0xa5d8ff)` 且 fill_style = Gradient（回归 781ec73 前行为） | P0 | 🆕 |
| U-BR-003 | insert 尺寸计算 | 各尺寸图片 × 各视口 | fit ≤ 视口 45%、不放大、居中、比例保持 | P0 | 🆕（把 `insert_image_bytes` 的计算抽为纯函数后测） |
| U-BR-004 | page_of_op | 每种 CanvasOp → 所属页 | 位置落在哪页返回哪页；UpdateElement 移动后跟随新页 | P1 | 🆕 |
| U-BR-005 | visible_world_bounds | 给定 camera/bounds | 世界可视矩形正确（含缩放/平移） | P1 | 🆕 |
| U-BR-006 | 自动保存节拍 | autosave_tick 模拟脏/干净 | 脏→写盘并清除脏标；干净→不写 | P1 | 🆕 |
| U-BR-007 | 删除级联 | 删容器形状 | 绑定标签随之删除 | P0 | 🆕 |
| U-BR-008 | 演示模式状态机 | start→翻页→idle 2.5s→chrome 隐藏→exit | 状态迁移与指针时间戳逻辑正确 | P2 | 🆕 |

#### 4.1.3 AI（AI）

| ID | 用例 | 操作 | 断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| U-AI-001 | CanvasStyle 全字段 merge_into | 带/不带各字段叠加到 base | 提供字段覆盖、缺省字段继承 | P0 | 🆕（现散在 tools 校验里） |
| U-AI-002 | dry_density/dry_width 越界拒绝 | 0.04 / 0.96 / 0.19 / 2.1 | validate 报范围错误；边界值 0.05/0.95/0.2/2.0 通过 | P0 | ✅（本轮加入，需保持） |
| U-AI-003 | AddImage 参数校验 | width 19/20/4000/4001 | 两侧边界行为正确 | P0 | 🆕 |
| U-AI-004 | AddImage serde 往返 | 最小/全字段 JSON | path 必填，x/y/width 可选 | P0 | 🆕 |
| U-AI-005 | smooth 透传 | draw_polygon smooth=true | op 携带 smooth（回归：曾被硬编码 false） | P0 | ✅（随本轮语义，建议加显式断言） |
| U-AI-006 | 工具清单快照 | `tools()` 返回的 NAME 集合 | 与文档清单一致（防工具漏注册/误删） | P1 | 🆕 |
| U-AI-007 | 系统提示词快照 | agent system prompt | 含每个工具名 + fill_style 五种枚举 + dry_* 说明（内容变更需显式更新快照） | P2 | 🆕 |
| U-AI-008 | 会话 JSONL 损坏容忍 | 截断/脏行文件 load | 跳过坏行不 panic | P1 | 🆕 |

#### 4.1.4 文本/平台/更新器（TX/PF/UP）

| ID | 用例 | 操作 | 断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| U-TX-001 | wrap_width 折行 | 长段落 + 窄宽 | 行数、末行不溢出（配合 `measure_text` 纯函数路径） | P1 | 🆕 |
| U-TX-002 | 对齐偏移 | line_offsets × {Left,Center,Right} | 偏移方向与宽度关系正确 | P2 | 🆕 |
| U-PF-001 | 光标样式映射 | tool/modifier → cursor | 每种组合映射唯一且合理 | P2 | 🆕 |
| U-UP-001 | 更新清单下载解析（已有 3 个） | — | 保持 | P2 | ✅ |

### 4.2 L2 集成测试（新建 `tests/integration.rs`）

载体原则：不启动 GPUI 循环，组合多个真实模块；涉及 `BoardView` 的逻辑先抽纯函数再进 L1。

| ID | 用例 | 操作 | 断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| I-SC-001 | 场景文件兼容矩阵 | `tests/fixtures/` 下存放：v0.2 旧板、无 pages 板、无 line_type 板、含 image 板 | 全部可加载 → 再保存 → 再加载，语义等价（现有两个 ✅ 的扩展） | P0 | 🆕 |
| I-SC-002 | CanvasOp 批量回放→场景 | eval `replay()` 结果与手工构造元素集合对比（类型/位置/样式） | 两轨一致（防 op 与手画行为漂移） | P1 | 🆕 |
| I-WK-001 | 图片资源存储往返 | `store_image_asset` → 磁盘 → `load_image_asset` 解码 | 尺寸/像素一致；重名自动避让（uuid）；目录自动创建 | P0 | 🆕 |
| I-WK-002 | 工作区全生命周期 | create_board → scan → rename → delete → last_board 记取/恢复 | 每步磁盘状态与 API 返回一致（扩展现有 4 个） | P1 | 🆕 |
| I-HI-001 | 历史×场景组合 | 连续 20 次混合变更（增/删/改/层序）后逐步 undo 到空、redo 到顶 | 每步场景与录制快照一致 | P1 | 🆕 |
| I-AI-001 | 会话绑定板切换 | session 绑定 board A/B → 切换 → 打开 | `open_session` 语义正确（现有 rebind 测试的端到端扩展） | P2 | 🆕 |
| I-TX-001 | 编辑会话×元素落盘 | 文本编辑会话 → commit → 场景序列化 → 重载 | 文本与 UTF-16 映射无损 | P1 | 🆕 |
| I-UP-001 | 原地更新残留清理 | 模拟 `.old` 残留 → `cleanup_old` | 清理且不动现役文件 | P2 | 🆕 |

### 4.3 L3 渲染断言细则

原则：**不渲染像素**，断言 `ReadyPath`/`WorldGeom` 的可观测属性（都是纯数据）。

- 几何属性：路径数量、包围盒（含 stroke 半宽外扩）、顶点密度随 zoom 的容差变化。
- 颜色属性：`ReadyPath.color` 为 `Background::Solid|Gradient`；渐变断言色标（角度、两端颜色、不透明度）。
- 确定性：同 seed 两次生成逐顶点相等；不同 seed 至少一顶点不同。
- 性能预算见 U-RD-012（`cargo test --release -- --ignored` 跑）。

### 4.4 L4 UI E2E（macOS，`scripts/e2e/`）

工具链（全部已在项目实践中验证）：

- 进程：`nohup target/release/boundless &`，`pgrep -x boundless` 守护；退出码即冒烟结果。
- 截图：`screencapture -x`（retina 2x）→ PIL 裁剪/降采样；亮度阈值：墨迹 <130、描边框 200~228、白底 ≥242。
- 点击：CGEvent 合成，**down→up 间隔 ≥60-80ms**（瞬时事件会被 GPUI 丢弃）；悬停高亮出现可验证事件到达。
- 键盘：`osascript key code`（Esc=53、F5=96）+ `keystroke "v" using command down`。
- 前置：屏幕必须解锁（锁屏时 `screencapture` 返回锁屏内容）；测试用独立 `BOUNDLESS_HOME` 隔离用户工作区（🆕 需要应用支持环境变量覆盖 `~/.boundless`，见路线图 R3）。

| ID | 场景 | 步骤（脚本化） | 像素/状态断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| E2E-001 | 启动冒烟 | 启动 → 等窗口 → 全屏截图 | 工具栏像素区非纯白；日志无 panic | P0 | 🆕 |
| E2E-002 | 画矩形 | 选矩形工具 → 按下拖动抬起 | 区域内出现墨迹行（亮度 <130 的行数 >0）；选中框出现 | P0 | 🆕 |
| E2E-003 | 五种填充切换 | 画圆 → 依次点 纹/密/实/彩/渐 → 每次截图 | 渐变模式：上 1/3 行均亮度 > 下 1/3（顶亮底暗）且列方向无此梯度；实心：填充区非白像素占比 > 纹 | P0 | 🆕（真·渐变回归主用例） |
| E2E-004 | 三种笔刷 | 画三笔：钢笔/铅笔/飞白 | 飞白笔迹的行覆盖率显著低于钢笔（断续） | P1 | 🆕 |
| E2E-005 | 图片粘贴 | 剪贴板放 PNG → Cmd-V | 视口中央出现非画布内容的大块异色区域；`.boundless/assets/` 新增 1 文件 | P0 | 🆕 |
| E2E-006 | 图片选择/缩放/删除 | 点击图片 → 拖角柄 → Delete | 尺寸变化（非白 bbox 变大）；删除后区域回到纯底 | P1 | 🆕 |
| E2E-007 | 选择/移动/撤销 | 画矩形 → 选择拖动 → Cmd-Z | 移动前后 bbox 变化；撤销后 bbox 还原且截图差 ≈0 | P0 | 🆕 |
| E2E-008 | 文本编辑 | T → 点画布 → 输入 → Esc | 出现文字墨迹；Esc 后进入选中 | P1 | 🆕 |
| E2E-009 | 页面与演示 | F5 → 翻页 → Esc | F5 后 chrome 淡出（工具栏区像素变化）、翻页内容切换 | P1 | 🆕 |
| E2E-010 | 保存恢复 | 画 3 元素 → Cmd-S → 杀进程 → 重启 | 重启后元素数一致（读板文件 + 截图） | P0 | 🆕 |
| E2E-011 | AI 面板冒烟 | Ctrl-B 打开面板（不调用模型） | 面板区出现；输入框聚焦时单键工具快捷键不触发（`!Input` 上下文） | P1 | 🆕 |
| E2E-012 | 退出写盘 | 画未存内容 → 点红点退出 | 退出后板文件更新（autosave flush） | P1 | 🆕 |

运行方式：`python3 scripts/e2e/run.py e2e-001 …`（默认全部 P0）；断点失败自动留存 `/tmp/e2e-artifacts/<id>/`（全图+裁剪+diff）。

### 4.5 L5 AI Agent 评测

| ID | 用例 | 载体 | 断言 | 优先级 | 状态 |
|---|---|---|---|---|---|
| AI-EV-001 | 既有 rubric 回归 | `cargo test -p boundless eval`（29 个 ✅） | 保持通过 | P0 | ✅ |
| AI-EV-002 | 录制回放金样 | 把历史会话的 op 序列存 `tests/fixtures/ops/*.json`，逐个 replay + rubric | 评分不低于基线（防提示词/布局回归） | P1 | 🆕 |
| AI-EV-003 | 工具 schema 快照 | 序列化全部 `ToolDefinition` | JSON Schema 与快照 diff（防字段漂移破坏模型端契约） | P1 | 🆕 |
| AI-EV-004 | 真实模型端到端 | 手动/夜间：3 个标准任务（登录流程图/水墨山水/思维导图）跑真模型 → rubric 评分 | 评分达标；成本记录在案 | P2 | 🆕 |

---

## 5. 横切：静态检查、覆盖率、性能

| 项 | 工具 | 门禁 |
|---|---|---|
| 格式 | `cargo fmt --check` | 提交前 + CI |
| Lint | `cargo clippy --all-targets -- -D warnings` | CI（先清零现有警告） |
| 覆盖率 | `cargo llvm-cov --html`（本地）/ CI 上报数值 | L1+L2 行覆盖 ≥60% 起步，目标 75% |
| 测试隔离 | 单测禁止写真实 `~/.boundless`；一律临时目录（`workspace.rs` 现有测试已是范式） | code review 检查项 |
| 性能基准 | `#[ignore]` 预算测试（U-RD-012）+ 可选 criterion：fingerprint、geometry、layout | 夜间跑，超预算 20% 告警 |

## 6. CI 集成（改造 `.github/workflows/ci.yml`）

1. **保留** Windows `cargo test` 作业（现状）。
2. **新增** macOS 作业：`macos-latest`，`cargo fmt --check && cargo clippy -D warnings && cargo test`（L1-L3；macOS 是主开发平台，必须与 Windows 同绿）。
3. PR 触发全部；main 触发全部 + 覆盖率数值上报；nightly（cron）跑 L2 全量 + L3 预算 + `AI-EV-002` 录制回放。
4. L4 E2E 不进 CI（需要解锁的 GUI 会话），作为**发版前本地门禁**：`scripts/e2e/run.py` 全绿才允许打 tag（可写进 `package-macos.sh` 前置提示）。

## 7. 落地路线图

| 阶段 | 内容 | 交付物 | 预估 |
|---|---|---|---|
| R1（立即） | U-RD-002/009/011、U-BR-001/002/003、U-AI-002/003/005、I-WK-001、E2E-001/002/003/007 | 本轮三特性（渐变/图片/飞白）的回归网 | 1~2 天 |
| R2（本迭代） | L2 `tests/` 骨架 + fixtures 兼容矩阵、CI macOS 作业、clippy 清零、覆盖率基线 | `tests/integration.rs`、ci.yml v2 | 2~3 天 |
| R3（下迭代） | E2E 全套 + `BOUNDLESS_HOME` 隔离支持、历史/自动保存抽纯函数进 L1 | `scripts/e2e/run.py` 全套 | 3~5 天 |
| R4（持续） | AI 录制回放金样、性能预算、真实模型夜间评测 | nightly 工作流 | 持续 |

## 8. 已知不可自动化项（明确不做）

- 真实 Apple Pencil/触控笔压手感（硬件在环）——以 `ink/` 单元测试模拟信号替代。
- Windows 上的 `PathBuilder::fill()` 渲染产物（gpui 0.2.2 已知缺陷，排线是兜底方案）——以 L3 几何断言 + macOS E2E 覆盖语义。
- 视觉美感（渐变色标是否"好看"）——rubric 只管结构，美感人工评审。
- 锁屏/多显示器切换等系统级行为。

## 附录 A：E2E 工具链速查（来自项目实践）

| 需求 | 方法 |
|---|---|
| 启动 | `nohup target/release/boundless > /tmp/boundless.log 2>&1 &` |
| 截图 | `screencapture -x /tmp/x.png`（2880×1800 retina） |
| 点击 | CGEvent down→up ≥60-80ms（`scripts/e2e/kit.py` 已封装） |
| 按键 | `osascript -e 'tell application "System Events" to key code 53'`（Esc=53、F5=96） |
| 剪贴板 | `osascript -e 'set the clipboard to (read (POSIX file "/tmp/x.png") as «class PNGf»)'` |
| 板文件 | `~/.boundless/workspace/*.boundless`（JSON）；`app.json` 的 `last_board` 仅在应用退出期间可改 |
