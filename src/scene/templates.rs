//! 素材模板库：可复用的角色 / 场景 / 道具模板，是漫画「人物、场景一致性」的
//! 机制基础。AI 把画好的角色（一组元素）用 `save_template` 存进工作区
//! `<root>/.boundless/templates/`，之后每一格都用 `stamp_template` 从同一份
//! 模板实例化——同一个角色跨格、跨会话都是同一套形状与配色，而不是每次重画。
//!
//! 纯逻辑模块（不依赖 GPUI）：存储是 JSON 文件读写，盖章是确定性的几何变换
//! （平移 / 等比缩放 / 水平镜像 + id 重映射），方便单测覆盖。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::element::{Element, ElementKind, WBounds, WPoint};

/// 模板类别：角色（人物）、场景（背景/环境）、道具（常用物件）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateKind {
    Character,
    Scene,
    Prop,
}

impl TemplateKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "character" | "角色" | "人物" => Some(Self::Character),
            "scene" | "场景" | "背景" => Some(Self::Scene),
            "prop" | "道具" | "物件" => Some(Self::Prop),
            _ => None,
        }
    }

    /// 中文标签（工具结果 / 运行时上下文用）。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Character => "角色",
            Self::Scene => "场景",
            Self::Prop => "道具",
        }
    }
}

/// 一个已保存的模板：名字 + 类别 + 构成元素（完整深拷贝，含样式与文字）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComicTemplate {
    pub name: String,
    pub kind: TemplateKind,
    pub elements: Vec<Element>,
}

/// 模板摘要（`list_templates` 与运行时上下文用，不携带元素本体）。
#[derive(Clone, Debug)]
pub struct TemplateSummary {
    pub name: String,
    pub kind: TemplateKind,
    pub element_count: usize,
    pub w: f64,
    pub h: f64,
    /// 模板内前几个文本内容（角色名、招牌文字等，帮助模型识别模板）。
    pub texts: Vec<String>,
}

impl TemplateSummary {
    pub fn one_line(&self) -> String {
        let texts = if self.texts.is_empty() {
            String::new()
        } else {
            format!("，文字：{}", self.texts.join("、"))
        };
        format!(
            "{}（{}）{} 个元素 {:.0}×{:.0}{}",
            self.name,
            self.kind.label(),
            self.element_count,
            self.w,
            self.h,
            texts
        )
    }
}

/// 模板文件名清洗：去掉路径分隔符与控制字符、限长。返回 Err 表示没有可用名字。
pub fn sanitize_name(raw: &str) -> Result<String, String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return Err("模板名不能为空".to_string());
    }
    // 文件名长度上限（按字符计，中文一个字算一个）。
    let out: String = cleaned.chars().take(24).collect();
    Ok(out)
}

fn template_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

/// 保存模板（同名覆盖），返回写入的文件路径。
pub fn save(dir: &Path, t: &ComicTemplate) -> Result<PathBuf, String> {
    if t.elements.is_empty() {
        return Err("模板不能为空（至少要有一个元素）".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("创建模板目录失败：{e}"))?;
    let path = template_path(dir, &t.name);
    let json = serde_json::to_string_pretty(t).map_err(|e| format!("模板序列化失败：{e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("写入模板失败 {}: {e}", path.display()))?;
    Ok(path)
}

/// 按名字加载模板。
pub fn load(dir: &Path, name: &str) -> Result<ComicTemplate, String> {
    let path = template_path(dir, name);
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| format!("模板「{name}」不存在：先调用 list_templates 查看已有模板，或先用绘图工具画出再用 save_template 保存"))?;
    serde_json::from_str(&raw).map_err(|e| format!("模板「{name}」损坏：{e}"))
}

/// 列出模板库全部模板（按名字排序）。目录不存在 = 空库。
pub fn list(dir: &Path) -> Vec<TemplateSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(t) = serde_json::from_str::<ComicTemplate>(&raw) else {
            eprintln!(
                "[templates] 跳过损坏的模板文件：{}",
                path.display()
            );
            continue;
        };
        let bbox = union_bounds(&t.elements);
        let texts: Vec<String> = t
            .elements
            .iter()
            .filter_map(|el| match &el.kind {
                ElementKind::Text { text, .. } => {
                    let first = text.lines().next().unwrap_or("").to_string();
                    if first.is_empty() {
                        None
                    } else {
                        Some(first)
                    }
                }
                _ => None,
            })
            .take(3)
            .collect();
        out.push(TemplateSummary {
            name: t.name,
            kind: t.kind,
            element_count: t.elements.len(),
            w: bbox.w,
            h: bbox.h,
            texts,
        });
    }
    out
}

/// 元素集合的整体包围盒（空集合 = 默认零框）。
fn union_bounds(elements: &[Element]) -> WBounds {
    let mut acc: Option<WBounds> = None;
    for el in elements {
        acc = Some(match acc {
            Some(u) => u.union(&el.bounds),
            None => el.bounds,
        });
    }
    acc.unwrap_or_default()
}

/// 把模板「盖」到画布上：整体平移到 (x, y)（模板包围盒左上角对齐）、等比缩放、
/// 可选水平镜像（漫画里翻转朝向用）。返回全新元素列表：
/// - 每个元素都分配新 id；
/// - 文本与容器的绑定关系（container_id）映射到新 id，容器不在模板内的标签
///   降级为独立文本（避免绑到画布上无关元素）；
/// - 镜像同时翻转阴影偏移的 x 分量。
/// 失败（空模板 / 非法缩放）返回 Err。
pub fn stamp(
    t: &ComicTemplate,
    x: f64,
    y: f64,
    scale: f64,
    flip_x: bool,
) -> Result<Vec<Element>, String> {
    if t.elements.is_empty() {
        return Err("模板为空，无法盖章".to_string());
    }
    if !scale.is_finite() || !(0.05..=8.0).contains(&scale) {
        return Err(format!("scale {scale} 超出范围 0.05~8.0"));
    }
    if !x.is_finite() || !y.is_finite() {
        return Err("坐标必须是有限数值".to_string());
    }
    let bbox = union_bounds(&t.elements);
    let pivot = WPoint::new(bbox.x, bbox.y);
    let mut out: Vec<Element> = Vec::with_capacity(t.elements.len());
    let mut id_map: Vec<(super::element::ElementId, super::element::ElementId)> =
        Vec::with_capacity(t.elements.len());
    for el in &t.elements {
        let old_id = el.id;
        let mut el = el.clone();
        if flip_x {
            mirror_x(&mut el, bbox.x + bbox.w / 2.0);
        }
        el.rescale(scale, scale, pivot);
        el.translate(x - bbox.x, y - bbox.y);
        el.id = super::element::ElementId::new_v4();
        id_map.push((old_id, el.id));
        out.push(el);
    }
    // 第二遍：重映射 container_id；容器不在模板内的标签降级为独立文本
    //（否则会绑到画布上某个无关元素）。
    for el in &mut out {
        if let ElementKind::Text { container_id, .. } = &mut el.kind {
            let remapped = container_id
                .as_ref()
                .and_then(|cid| id_map.iter().find(|(old, _)| *old == *cid))
                .map(|(_, new)| *new);
            *container_id = remapped;
        }
    }
    Ok(out)
}

/// 把一个元素绕竖直线 `cx` 水平镜像。形状/图片只镜像包围盒；点集元素把相对
/// 坐标的 x 取反（new_rel.x = w - rel.x）；文字内容不镜像（字形保持可读），
/// 只平移位置；阴影偏移 x 分量取反。
fn mirror_x(el: &mut Element, cx: f64) {
    let b = el.bounds;
    el.bounds.x = 2.0 * cx - b.x - b.w;
    if el.is_point_based() {
        if let Some(points) = point_slice_mut(el) {
            for p in points.iter_mut() {
                p.x = b.w - p.x;
            }
        }
    }
    if let Some(shadow) = &mut el.style.shadow {
        shadow.dx = -shadow.dx;
    }
}

fn point_slice_mut(el: &mut Element) -> Option<&mut Vec<WPoint>> {
    match &mut el.kind {
        ElementKind::Line { points } | ElementKind::Arrow { points, .. } => Some(points),
        ElementKind::Freedraw { points, .. } => Some(points),
        ElementKind::Polygon { points, .. } => Some(points),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::element::{ElementStyle, Shadow, TextAlign};

    fn rect(x: f64, y: f64, w: f64, h: f64, fill: Option<u32>) -> Element {
        let mut el = Element::new(ElementKind::Rectangle, WBounds::new(x, y, w, h), {
            let mut s = ElementStyle::default();
            s.background = fill;
            s.shadow = Some(Shadow { dx: 10.0, dy: 12.0 });
            s
        });
        el.style.background = fill;
        el
    }

    fn bound_label(container: &Element, text: &str) -> Element {
        let mut el = Element::new(
            ElementKind::Text {
                text: text.to_string(),
                font_size: 20.0,
                font_family: "Caveat".into(),
                wrap_width: Some(container.bounds.w - 12.0),
                min_height: None,
                container_id: Some(container.id),
                text_align: TextAlign::Center,
                anchor: None,
            },
            WBounds::new(
                container.bounds.x + 10.0,
                container.bounds.y + 10.0,
                container.bounds.w - 20.0,
                25.0,
            ),
            ElementStyle::default(),
        );
        el.style.roughness = 0.0;
        el
    }

    fn sample_template() -> ComicTemplate {
        let head = rect(100.0, 100.0, 80.0, 80.0, Some(0xffd8a8));
        // 身体比头部更靠右：让整体包围盒 x∈[100,230]，镜像时有非平凡变化。
        let body = rect(110.0, 190.0, 120.0, 90.0, Some(0xa5d8ff));
        let label = bound_label(&head, "小明");
        ComicTemplate {
            name: "小明".into(),
            kind: TemplateKind::Character,
            elements: vec![head, body, label],
        }
    }

    #[test]
    fn sanitize_name_strips_separators_and_caps_length() {
        assert_eq!(sanitize_name(" 小明 ").unwrap(), "小明");
        assert_eq!(sanitize_name("a/b\\c:d").unwrap(), "abcd");
        assert!(sanitize_name("  ").is_err());
        assert!(sanitize_name("/\\").is_err());
        let long: String = "字".repeat(30);
        assert_eq!(sanitize_name(&long).unwrap().chars().count(), 24);
    }

    #[test]
    fn save_load_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!("boundless-tpl-test-{}", uuid::Uuid::new_v4()));
        let t = sample_template();
        save(&dir, &t).unwrap();
        let back = load(&dir, "小明").unwrap();
        assert_eq!(back.name, "小明");
        assert_eq!(back.kind, TemplateKind::Character);
        assert_eq!(back.elements.len(), 3);
        // 元素完整深拷贝（id、样式一致）。
        assert_eq!(back.elements[0].id, t.elements[0].id);

        let summaries = list(&dir);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "小明");
        assert_eq!(summaries[0].element_count, 3);
        // 包围盒：x∈[100,230] y∈[100,280] → 130×180。
        assert!((summaries[0].w - 130.0).abs() < 1e-9);
        assert!((summaries[0].h - 180.0).abs() < 1e-9);
        assert!(summaries[0].texts.contains(&"小明".to_string()));
        assert!(summaries[0].one_line().contains("角色"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_template_gives_guidance() {
        let dir = std::env::temp_dir().join(format!("boundless-tpl-test-{}", uuid::Uuid::new_v4()));
        let err = load(&dir, "不存在").unwrap_err();
        assert!(err.contains("list_templates"), "{err}");
    }

    #[test]
    fn stamp_translates_and_remaps_container() {
        let t = sample_template();
        let stamped = stamp(&t, 1000.0, 500.0, 1.0, false).unwrap();
        assert_eq!(stamped.len(), 3);
        // 原包围盒 (100,100) → 目标 (1000,500)：整体平移 (+900, +400)。
        let head = &stamped[0];
        assert!((head.bounds.x - 1000.0).abs() < 1e-9);
        assert!((head.bounds.y - 500.0).abs() < 1e-9);
        assert!((head.bounds.w - 80.0).abs() < 1e-9);
        // id 全部更新。
        assert_ne!(head.id, t.elements[0].id);
        // 标签绑定到新 head 的 id。
        let label = &stamped[2];
        match &label.kind {
            ElementKind::Text {
                container_id: Some(cid),
                ..
            } => assert_eq!(*cid, head.id),
            other => panic!("期望绑定标签，实际 {other:?}"),
        }
    }

    #[test]
    fn stamp_scales_geometry_and_font() {
        let t = sample_template();
        let stamped = stamp(&t, 0.0, 0.0, 2.0, false).unwrap();
        let head = &stamped[0];
        assert!((head.bounds.w - 160.0).abs() < 1e-9);
        assert!((head.bounds.h - 160.0).abs() < 1e-9);
        match &stamped[2].kind {
            ElementKind::Text { font_size, .. } => {
                assert!((font_size - 40.0).abs() < 1e-9);
            }
            other => panic!("期望文本，实际 {other:?}"),
        }
    }

    #[test]
    fn stamp_flip_mirrors_shapes_and_shadow_but_keeps_text_readable() {
        let t = sample_template();
        // 模板包围盒 x∈[100,230]，镜像中心 cx=165：head(100..180) → 150..230，
        // body(110..230) → 100..220，label(110..170) → 160..220。
        // 目标 (100,100) 与包围盒原点重合，平移为零，纯看镜像效果。
        let stamped = stamp(&t, 100.0, 100.0, 1.0, true).unwrap();
        let head = &stamped[0];
        assert!((head.bounds.x - 150.0).abs() < 1e-9);
        // 阴影偏移翻转。
        assert!((head.style.shadow.unwrap().dx + 10.0).abs() < 1e-9);
        // 文字内容不变（不镜像字形），位置平移到镜像位置。
        match &stamped[2].kind {
            ElementKind::Text { text, .. } => assert_eq!(text, "小明"),
            other => panic!("期望文本，实际 {other:?}"),
        }
        // 绑定仍指向新 head。
        match &stamped[2].kind {
            ElementKind::Text {
                container_id: Some(cid),
                ..
            } => assert_eq!(*cid, head.id),
            other => panic!("期望绑定标签，实际 {other:?}"),
        }
        // 镜像不改变整体包围盒。
        let bbox = union_bounds(&stamped);
        assert!((bbox.x - 100.0).abs() < 1e-9);
        assert!((bbox.w - 130.0).abs() < 1e-9);
    }

    #[test]
    fn stamp_orphan_label_degrades_to_standalone() {
        // 标签的容器不在模板元素集合里 → 盖章后降级为独立文本。
        let mut t = sample_template();
        t.elements.remove(0); // 去掉头部容器，标签成为孤儿
        let stamped = stamp(&t, 0.0, 0.0, 1.0, false).unwrap();
        match &stamped[1].kind {
            ElementKind::Text {
                container_id, ..
            } => assert_eq!(*container_id, None),
            other => panic!("期望文本，实际 {other:?}"),
        }
    }

    #[test]
    fn stamp_rejects_bad_scale_and_empty() {
        let t = sample_template();
        assert!(stamp(&t, 0.0, 0.0, 0.01, false).is_err());
        assert!(stamp(&t, 0.0, 0.0, f64::NAN, false).is_err());
        assert!(stamp(&ComicTemplate {
            name: "空".into(),
            kind: TemplateKind::Prop,
            elements: vec![],
        }, 0.0, 0.0, 1.0, false)
        .is_err());
    }

    #[test]
    fn point_based_elements_mirror_their_points() {
        let poly = Element::from_absolute_points(
            |points| ElementKind::Polygon {
                points,
                smooth: false,
            },
            vec![
                WPoint::new(100.0, 100.0),
                WPoint::new(160.0, 120.0),
                WPoint::new(110.0, 160.0),
            ],
            ElementStyle::default(),
        );
        let t = ComicTemplate {
            name: "山".into(),
            kind: TemplateKind::Scene,
            elements: vec![poly],
        };
        // 目标 (100,100) 与包围盒原点重合，平移为零，纯看镜像效果。
        let stamped = stamp(&t, 100.0, 100.0, 1.0, true).unwrap();
        let abs = stamped[0].absolute_points();
        // 包围盒 x∈[100,160]，镜像中心 130：点 (160,120) → (100,120)。
        assert!(abs.iter().any(|p| (p.x - 100.0).abs() < 1e-9 && (p.y - 120.0).abs() < 1e-9));
        // 点 (100,100) → (160,100)。
        assert!(abs.iter().any(|p| (p.x - 160.0).abs() < 1e-9 && (p.y - 100.0).abs() < 1e-9));
    }
}
