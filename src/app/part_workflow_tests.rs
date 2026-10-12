//! The milestone workflow driven through the editor itself: the automation
//! operations the MCP bridge forwards (`new`, `run`, `save`, `open`, `undo`,
//! `redo`), on a headless editor, with the body measured on the kernel B-rep
//! after every step.

use super::OpenCADStudio;
use serde_json::Value;

fn op(app: &mut OpenCADStudio, json: &str) -> Value {
    let result = app.automation_op(json);
    assert_ne!(result["ok"], Value::Bool(false), "{json} -> {result}");
    result
}

fn run(app: &mut OpenCADStudio, cmd: &str) -> Value {
    op(
        app,
        &serde_json::json!({ "op": "run", "cmd": cmd }).to_string(),
    )
}

fn body(app: &mut OpenCADStudio) -> Option<([f64; 3], f64)> {
    let i = app.active_tab;
    let measure = app.tabs[i].scene.part_body_measure()?;
    Some((measure.size(), measure.volume))
}

fn assert_block(app: &mut OpenCADStudio, size: [f64; 3], volume: f64, step: &str) {
    let (actual, actual_volume) = body(app).unwrap_or_else(|| panic!("{step}: no body"));
    for axis in 0..3 {
        assert!(
            (actual[axis] - size[axis]).abs() < 1e-6,
            "{step}: size {actual:?}, expected {size:?}"
        );
    }
    assert!(
        (actual_volume - volume).abs() < 1e-3,
        "{step}: volume {actual_volume}, expected {volume}"
    );
}

#[test]
fn sketch_extrude_edit_save_reopen_edit_undo_redo() {
    let mut app = OpenCADStudio::new_for_test();
    op(&mut app, r#"{"op":"new"}"#);

    // 1-5: a part, a 100 x 60 rectangle on XY, extruded 20.
    run(&mut app, "NEWPART Part1");
    run(&mut app, "PARTRECT XY 0,0 100 60");
    run(&mut app, "PARTEXTRUDE Sketch1 20");
    assert_block(&mut app, [100.0, 60.0, 20.0], 120_000.0, "extruded");

    // 7-10: select the sketch, widen it; the extrusion follows.
    run(&mut app, "PARTSELECT Sketch1");
    let i = app.active_tab;
    assert_eq!(
        app.tabs[i].scene.selected.len(),
        4,
        "the sketch's four lines"
    );
    run(&mut app, "PARTEDIT Sketch1 width 150");
    assert_block(&mut app, [150.0, 60.0, 20.0], 180_000.0, "widened");

    // 11-13: save, close (a fresh drawing), reopen.
    let dir = std::env::temp_dir().join(format!("ocs-part-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("block.dwg");
    op(
        &mut app,
        &serde_json::json!({ "op": "save", "path": path.to_string_lossy() }).to_string(),
    );
    op(&mut app, r#"{"op":"new"}"#);
    assert!(body(&mut app).is_none(), "the new drawing has no part");
    op(
        &mut app,
        &serde_json::json!({ "op": "open", "path": path.to_string_lossy() }).to_string(),
    );
    let i = app.active_tab;
    let part = app.tabs[i]
        .scene
        .part()
        .unwrap()
        .expect("the tree came back");
    let names: Vec<&str> = part.features.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Top Plane",
            "Front Plane",
            "Right Plane",
            "Sketch1",
            "Extrude1"
        ]
    );
    let sketch = part.find("Sketch1").unwrap().id;
    let extrude = part.find("Extrude1").unwrap().id;
    assert_eq!(part.dependencies(extrude), vec![sketch]);
    let dims = app.tabs[i].scene.part_sketch_dimensions(sketch);
    assert!(dims
        .iter()
        .any(|d| d.label == "width" && (d.value - 150.0).abs() < 1e-9));
    assert!(dims
        .iter()
        .any(|d| d.label == "height" && (d.value - 60.0).abs() < 1e-9));
    assert_block(&mut app, [150.0, 60.0, 20.0], 180_000.0, "reopened");

    // 14-15: deepen and rebuild.
    run(&mut app, "PARTEDIT Extrude1 depth 30");
    assert_block(&mut app, [150.0, 60.0, 30.0], 270_000.0, "deepened");
    run(&mut app, "PARTEDIT Sketch1 height 50");
    assert_block(&mut app, [150.0, 50.0, 30.0], 225_000.0, "narrowed");

    // 16: undo and redo walk the same states back and forth, tree included.
    op(&mut app, r#"{"op":"undo"}"#);
    assert_block(&mut app, [150.0, 60.0, 30.0], 270_000.0, "undo height");
    op(&mut app, r#"{"op":"undo"}"#);
    assert_block(&mut app, [150.0, 60.0, 20.0], 180_000.0, "undo depth");
    let depth = |app: &OpenCADStudio| match &app.tabs[app.active_tab]
        .scene
        .part()
        .unwrap()
        .unwrap()
        .find("Extrude1")
        .unwrap()
        .kind
    {
        crate::scene::part::FeatureKind::Extrude(e) => e.depth,
        _ => unreachable!(),
    };
    assert_eq!(depth(&app), 20.0, "undo restored the feature parameter");
    op(&mut app, r#"{"op":"redo"}"#);
    assert_eq!(depth(&app), 30.0);
    assert_block(&mut app, [150.0, 60.0, 30.0], 270_000.0, "redo depth");
    op(&mut app, r#"{"op":"redo"}"#);
    assert_block(&mut app, [150.0, 50.0, 30.0], 225_000.0, "redo height");

    // A refused edit leaves no undo step and no change.
    let result = app.automation_op(r#"{"op":"run","cmd":"PARTEDIT Extrude1 depth 0"}"#);
    assert_eq!(result["ok"], Value::Bool(false), "{result}");
    assert_block(&mut app, [150.0, 50.0, 30.0], 225_000.0, "refused");
    op(&mut app, r#"{"op":"undo"}"#);
    assert_block(
        &mut app,
        [150.0, 60.0, 30.0],
        270_000.0,
        "undo after refusal",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn undo_walks_back_through_part_creation() {
    let mut app = OpenCADStudio::new_for_test();
    op(&mut app, r#"{"op":"new"}"#);
    run(&mut app, "NEWPART");
    run(&mut app, "PARTRECT XY 0,0 100 60");
    run(&mut app, "PARTEXTRUDE Sketch1 20");
    let i = app.active_tab;
    op(&mut app, r#"{"op":"undo"}"#);
    let part = app.tabs[i].scene.part().unwrap().unwrap();
    assert!(part.find("Extrude1").is_none());
    assert!(body(&mut app).is_none(), "the body went with the extrusion");
    op(&mut app, r#"{"op":"undo"}"#);
    let part = app.tabs[i].scene.part().unwrap().unwrap();
    assert!(part.find("Sketch1").is_none());
    op(&mut app, r#"{"op":"undo"}"#);
    assert_eq!(app.tabs[i].scene.part().unwrap(), None);
    for _ in 0..3 {
        op(&mut app, r#"{"op":"redo"}"#);
    }
    assert_block(&mut app, [100.0, 60.0, 20.0], 120_000.0, "redone");
}

#[test]
fn finishing_a_sketch_on_an_origin_plane_adds_it_to_the_part() {
    let mut app = OpenCADStudio::new_for_test();
    op(&mut app, r#"{"op":"new"}"#);
    run(&mut app, "NEWPART");
    run(&mut app, "CREATESKETCH XY");
    // A closed polyline rectangle, drawn the ordinary way.
    run(&mut app, "RECTANG 0,0 40,30");
    run(&mut app, "FINISHSKETCH");
    let i = app.active_tab;
    let part = app.tabs[i].scene.part().unwrap().unwrap();
    assert!(part.find("Sketch1").is_some(), "{:?}", part.features);
    run(&mut app, "PARTEXTRUDE Sketch1 10");
    assert_block(&mut app, [40.0, 30.0, 10.0], 12_000.0, "polyline sketch");
}
