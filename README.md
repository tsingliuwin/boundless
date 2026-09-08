<div align="center">

<img src="logo.png" width="120" alt="boundless logo"/>

# boundless

**An infinite hand-drawn whiteboard on GPUI — with an AI agent that draws directly onto the canvas.**

Excalidraw 风格的无限手绘白板，AI 智能体直接在画布上作画。

[![CI](https://github.com/tsingliuwin/boundless/actions/workflows/ci.yml/badge.svg)](https://github.com/tsingliuwin/boundless/actions/workflows/ci.yml)
[![Release](https://github.com/tsingliuwin/boundless/actions/workflows/release.yml/badge.svg)](https://github.com/tsingliuwin/boundless/releases)

</div>

---

boundless 是一个用 Rust + [GPUI](https://gpui.rs) 写的无限白板：笔迹带压感、图形自带手绘抖动（rough.js 风格）、多页面放映，内置一个 rig 驱动的 AI 智能体——你说"画一份讲递归的 PPT"，它就调用工具直接把元素画到画布上，而不是给你一张图或一段代码。

## 特性

**手绘画板**
- 全套工具：画笔（压感）、矩形、菱形、椭圆、箭头、直线、文本、橡皮、抓手，单键切换（`P R D O A L T E H`）
- 手绘渲染：rough 风格抖动线条，排线/水彩/密实/干刷多种填充，画布表面预设（白板/黑板/宣纸…），支持真渐变填充与无边框样式
- 中文手绘风：内嵌小赖字体，全场景手写体渲染
- 数位笔压感（Windows WM_POINTER）、触摸板捏合缩放、平滑与墨迹收集管线
- 撤销/重做、层级调整、多页面 + 页面栏、工作区资源管理器与自动保存

**放映**
- PowerPoint 式真全屏放映，翻页镜头滑动过渡，逐页转场效果
- 缩放窗口自动适配回位，`PageUp/PageDown` 翻页

**AI 智能体**
- 基于 [rig](https://github.com/0xPlaygrounds/rig) 的多轮工具调用循环，流式输出，直接操作画布元素
- 任意 OpenAI 兼容端点均可（OpenAI / DeepSeek / 本地 Ollama…），在应用内设置页配置
- 内置六大创作技能（见下表），按用户请求自动路由；技能是纯 `SKILL.md` 文件，加场景不用改代码

| 技能 | 用途 |
|------|------|
| **slides** | PPT/幻灯片——手绘质感的演示文稿，每页从内容里长出涂鸦，叙事架构组织全篇 |
| **comic** | 漫画/分镜——角色套件保证跨格一致，表情库 + 动势 + 气泡，老夫子式夸张分镜 |
| **article-illustration** | 文章插画——把文章的判断、流程、隐喻画成一眼读懂的手绘解释图 |
| **blackboard-poster** | 黑板报/海报——墨绿粉笔板、三栏版面、粉笔高亮配色 |
| **ink-wash-landscape** | 水墨山水——墨色分层、留白、题跋朱印的国画构图 |
| **mindmap** | 思维导图——`draw_mindmap` 一次画出整棵树 |

**自动更新**
- 自托管 `latest.json` 清单 + minisign 签名校验，下载后原地换包重启（配置见 [UPDATER_CONFIG.md](UPDATER_CONFIG.md)）

## 下载

从 [Releases](https://github.com/tsingliuwin/boundless/releases) 获取：

- Windows：`win-x64-setup.exe`（NSIS 安装器）或 `win-x64.zip`（绿色版）
- macOS (Apple Silicon)：`macos-arm64.dmg` 或 `zip`

## 从源码构建

需要 Rust stable（edition 2021）。

```sh
cargo run              # 开发构建
cargo test             # 单元测试
cargo build --release  # 发布构建
```

- GPUI 以 vendored 方式内嵌（`vendor/gpui`），带一个本地补丁：macOS 触摸板捏合手势合成 cmd+scroll 实现光标处缩放（stock 0.2.2 会丢弃该事件）
- Windows 安装器：NSIS 脚本 `installer/installer.nsi`
- macOS 打包：`scripts/package-macos.sh`
- 评测 harness：`cargo run --example slides_eval`（另有 `blackboard_eval` / `mindmap_eval`）

## AI 配置

启动后在设置页（`Ctrl/Cmd + ,`）填三项：

- **Base URL**：任意 OpenAI 兼容端点，如 `https://api.openai.com/v1`
- **API Key**
- **Model**：默认 `gpt-4o-mini`；本地 Ollama 预设 `http://localhost:11434/v1/`

自定义技能放到 `~/.boundless/skills/<name>/SKILL.md`（YAML frontmatter + 正文，WorkBuddy / Agent-Skills 兼容格式），重启即生效。

## 快捷键

| 按键 | 动作 | 按键 | 动作 |
|------|------|------|------|
| `P` | 画笔 | `Ctrl/Cmd+Z` | 撤销（`+Shift` 重做） |
| `R` / `D` / `O` | 矩形/菱形/椭圆 | `Ctrl/Cmd+S` / `O` | 保存/打开 |
| `A` / `L` | 箭头/直线 | `Ctrl/Cmd+I` / `V` | 插入/粘贴图片 |
| `T` / `E` / `H` | 文本/橡皮/抓手 | `Ctrl/Cmd+B` | AI 面板 |
| `Ctrl/Cmd+=` `-` `0` | 缩放/复位 | `Ctrl/Cmd+E` | 资源管理器 |
| `Ctrl/Cmd+[` `]` | 层级（`+Shift` 置顶/沉底） | `PageUp/Down` | 翻页 |
| `Delete` / `Esc` | 删除/取消 | `Ctrl/Cmd+,` | 设置 |

## 项目结构

```
src/
├── board.rs      画板视图与交互
├── ink/          墨迹管线：压感采集、平滑、轮廓
├── render/       rough 手绘渲染与缓存
├── scene/        元素模型、多页面、模板、思维导图（.boundless JSON 格式）
├── ai/           rig 智能体：工具调用、技能路由、会话存储、面板
├── workspace.rs  工作区与资源管理器
└── updater.rs    自托管自动更新
skills/           六大内置技能（SKILL.md）
vendor/gpui/      vendored GPUI + 触摸板捏合补丁
```

数据与诊断位于 `~/.boundless/`（可用 `BOUNDLESS_HOME` 重定向）：会话与工作区、`agent-logs/*.jsonl`（智能体运行日志）、`panic.log`。

## License

暂未附开源许可证，如需使用请先联系作者。
