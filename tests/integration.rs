//! L2 集成测试（docs/test-plan.md §4.2）：跨模块行为，不启动 GPUI 循环。
//!
//! - I-SC-001 场景文件兼容矩阵（旧文件/缺省字段/图片元素）
//! - I-HI-001 历史 × 场景混合变更逐步 undo/redo
//! - I-SC-002 CanvasOp 回放与手工构造的一致性
//!
//! 运行：`cargo test --test integration`

use boundless::ai::canvas_ops::{CanvasOp, CanvasStyle, OpFillStyle, OpPoint};
use boundless::ai::eval::replay;
use boundless::camera::Camera;
use boundless::history::History;
use boundless::scene::{
    Element, ElementKind, ElementStyle, FillStyle, Scene, SceneFile, WBounds,
};


// ---------------------------------------------------------------------------
// I-SC-001 场景文件兼容矩阵
// ---------------------------------------------------------------------------

/// 模拟一份"字段尚不存在时代的"旧场景文件：基础样式字段齐全（它们没有
/// serde 默认值），而 line_type/fill_style/shadow/brush/dry_* 等后加字段
/// 全部缺省。旧文件必须原样加载。
#[test]
fn legacy_scene_without_newer_fields_loads() {
    let json = r#"{
        "type": "boundless-scene",
        "version": 1,
        "camera": {"x": 0.0, "y": 0.0, "zoom": 1.0},
        "show_grid": false,
        "elements": [
            {
                "id": "3f2a9c1e-1111-4222-8333-444455556666",
                "x": 10.0, "y": 20.0, "w": 100.0, "h": 80.0,
                "seed": 42,
                "stroke": 1973790, "background": null,
                "stroke_width": 2.0, "roughness": 1.0,
                "stroke_style": "solid", "opacity": 1.0,
                "kind": "rectangle"
            },
            {
                "id": "3f2a9c1e-1111-4222-8333-444455556667",
                "x": 0.0, "y": 0.0, "w": 60.0, "h": 0.0,
                "seed": 7,
                "stroke": 255, "background": null,
                "stroke_width": 2.0, "roughness": 1.0,
                "stroke_style": "solid", "opacity": 1.0,
                "kind": "freedraw",
                "points": [[0.0, 0.0], [30.0, 0.0], [60.0, 0.0]],
                "widths": []
            }
        ],
        "pages": []
    }"#;
    let file = SceneFile::parse(json).expect("legacy scene must load");
    assert_eq!(file.elements.len(), 2);
    // 后加字段全部落到安全默认值。
    for el in &file.elements {
        assert_eq!(el.style.line_type, boundless::scene::LineType::Straight);
        assert_eq!(el.style.fill_style, FillStyle::Hachure);
        assert!(el.style.shadow.is_none() && el.style.brush.is_none());
        assert!(el.style.dry_density.is_none() && el.style.dry_width.is_none());
    }
    // 保存 → 再加载，语义不丢。
    let json2 = serde_json::to_string(&file).unwrap();
    let file2 = SceneFile::parse(&json2).unwrap();
    assert_eq!(file.elements.len(), file2.elements.len());
    assert_eq!(file.elements[0].style.fill_style, file2.elements[0].style.fill_style);
}

/// 图片元素：场景文件往返无损（asset 名 + 显示框），且旧版字段组合共存。
#[test]
fn image_element_roundtrips_through_scene_file() {
    let mut scene = Scene::new();
    scene.add(Element::new(
        ElementKind::Image {
            asset: "img-deadbeef.png".into(),
        },
        WBounds::new(100.0, 200.0, 320.0, 240.0),
        ElementStyle::default(),
    ));
    let json = serde_json::to_string(&SceneFile::new(&scene, Camera::default())).unwrap();
    let parsed = SceneFile::parse(&json).expect("roundtrip parse");
    assert_eq!(parsed.elements.len(), 1);
    match &parsed.elements[0].kind {
        ElementKind::Image { asset } => assert_eq!(asset, "img-deadbeef.png"),
        other => panic!("expected image element, got {other:?}"),
    }
    let b = &parsed.elements[0].bounds;
    assert_eq!((b.x, b.y, b.w, b.h), (100.0, 200.0, 320.0, 240.0));
}

/// 损坏/伪造文件被拒绝（type 或 version 不对）。
#[test]
fn scene_parse_rejects_wrong_type_and_version() {
    assert!(SceneFile::parse(r#"{"type":"other","version":1}"#).is_err());
    assert!(SceneFile::parse(r#"{"type":"boundless-scene","version":99}"#).is_err());
}

// ---------------------------------------------------------------------------
// I-HI-001 历史 × 场景：混合变更后逐步 undo / redo
// ---------------------------------------------------------------------------

#[test]
fn history_over_mixed_mutations_restores_every_step() {
    let mut scene = Scene::new();
    let mut history = History::new();
    let mut snapshots: Vec<Vec<Element>> = Vec::new();

    // 六种混合变更：增 / 增 / 样式改 / 移动 / 删 / 层序。
    history.record(&scene);
    scene.add(Element::new(
        ElementKind::Rectangle,
        WBounds::new(0.0, 0.0, 100.0, 80.0),
        ElementStyle::default(),
    ));
    snapshots.push(scene.elements.clone());

    history.record(&scene);
    scene.add(Element::new(
        ElementKind::Image {
            asset: "img-x.png".into(),
        },
        WBounds::new(200.0, 0.0, 64.0, 48.0),
        ElementStyle::default(),
    ));
    snapshots.push(scene.elements.clone());

    history.record(&scene);
    scene.get_mut(scene.elements[0].id).unwrap().style.stroke = 0xff0000;
    snapshots.push(scene.elements.clone());

    history.record(&scene);
    scene.get_mut(scene.elements[0].id).unwrap().translate(15.0, 25.0);
    snapshots.push(scene.elements.clone());

    history.record(&scene);
    scene.remove(scene.elements[1].id).unwrap();
    snapshots.push(scene.elements.clone());

    history.record(&scene);
    scene.move_to_front(&[scene.elements[0].id]);
    snapshots.push(scene.elements.clone());

    // 逐步 undo：undo 栈存的是"每次变更前"的状态，所以第 k 次 undo
    // 恢复的是第 (6-k) 次变更后的场景（第 6 次回到空场景）。
    for k in 1..=6 {
        assert!(history.undo(&mut scene), "undo must have a step");
        let expected: &[Element] = if k < 6 { &snapshots[5 - k] } else { &[] };
        assert_eq!(scene.elements.len(), expected.len(), "undo step {k}");
        for (got, want) in scene.elements.iter().zip(expected.iter()) {
            assert_eq!(got.id, want.id);
            assert_eq!(got.bounds, want.bounds);
            assert_eq!(got.style.stroke, want.style.stroke);
        }
    }
    assert!(!history.undo(&mut scene), "no steps left");

    // redo 走回最终态。
    for _ in 0..snapshots.len() {
        assert!(history.redo(&mut scene));
    }
    assert!(!history.redo(&mut scene));
    assert_eq!(scene.elements.len(), snapshots.last().unwrap().len());
}

// ---------------------------------------------------------------------------
// I-SC-002 CanvasOp 回放与手工构造的一致性
// ---------------------------------------------------------------------------

#[test]
fn replay_places_ops_where_their_args_say() {
    fn pt(x: f64, y: f64) -> OpPoint {
        OpPoint { x, y }
    }
    let ops: Vec<(CanvasOp, Option<String>)> = vec![
        (
            CanvasOp::Rectangle {
                x: 100.0,
                y: 200.0,
                w: 300.0,
                h: 200.0,
                style: CanvasStyle {
                    fill: Some(0xe7f0ff),
                    fill_style: Some(OpFillStyle::Solid),
                    ..Default::default()
                },
                text: None,
            },
            None,
        ),
        (
            CanvasOp::AddImage {
                path: "/nonexistent/img.png".into(),
                x: Some(500.0),
                y: Some(100.0),
                width: Some(320.0),
            },
            None,
        ),
        (
            CanvasOp::Polygon {
                points: vec![pt(0.0, 0.0), pt(80.0, 40.0), pt(10.0, 90.0)],
                smooth: true,
                style: CanvasStyle::default(),
            },
            None,
        ),
    ];
    let canvas = replay(&ops);
    assert_eq!(canvas.ops_applied, 3, "all three ops must apply");
    assert_eq!(canvas.ops_failed, 0);

    // 矩形：参数原样落位。
    let rect = &canvas.elements[0];
    assert_eq!((rect.x, rect.y, rect.w, rect.h), (100.0, 200.0, 300.0, 200.0));

    // 图片：回放无法解码像素，但布局框必须预留（真实尺寸不可读 → 4:3 估计）。
    let img = &canvas.elements[1];
    assert_eq!(img.kind, "image");
    assert_eq!((img.x, img.y), (500.0, 100.0));
    assert_eq!(img.w, 320.0);
    assert_eq!(img.h, 240.0, "4:3 fallback aspect for unreadable files");

    // 多边形：包围盒 = 顶点极值。
    let poly = &canvas.elements[2];
    assert_eq!(poly.kind, "polygon");
    assert_eq!((poly.x, poly.y, poly.w, poly.h), (0.0, 0.0, 80.0, 90.0));
}
