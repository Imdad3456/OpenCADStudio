//! The parametric part: sketch → extrude → edit → rebuild → save → reopen,
//! measured on the geometry kernel's own B-rep (volume from its mass
//! properties, size from its bounding box), not on display meshes.

use codec::entities::EntityType;
use codec::types::Vector3;
use codec::Handle;
use OpenCADStudio::scene::part::{
    BodyMeasure, DatumPlane, ExtrudeOperation, FeatureKind, FeatureStatus, PartError,
};
use OpenCADStudio::scene::Scene;

const TOL: f64 = 1e-6;

fn assert_body(scene: &mut Scene, size: [f64; 3], volume: f64) -> BodyMeasure {
    let measure = scene.part_body_measure().expect("the part has a body");
    let actual = measure.size();
    for axis in 0..3 {
        assert!(
            (actual[axis] - size[axis]).abs() < TOL,
            "size {actual:?}, expected {size:?}"
        );
    }
    assert!(
        (measure.volume - volume).abs() < volume * 1e-9 + TOL,
        "volume {}, expected {volume}",
        measure.volume
    );
    measure
}

/// NEWPART, a 100 x 60 rectangle on the Top (XY) plane, extruded 20.
fn block() -> Scene {
    let mut scene = Scene::new();
    scene.populate_new_drawing_defaults();
    scene.create_part(Some("Part1")).unwrap();
    let rect = scene
        .part_add_rectangle_sketch(DatumPlane::Top, [0.0, 0.0], 100.0, 60.0)
        .unwrap();
    assert_eq!(rect.width_parameter, "Sketch1_width");
    scene
        .part_add_extrude("Sketch1", 20.0, ExtrudeOperation::NewBody, false)
        .unwrap();
    scene
}

fn line(scene: &Scene, handle: Handle) -> (Vector3, Vector3) {
    match scene.document.get_entity(handle) {
        Some(EntityType::Line(line)) => (line.start, line.end),
        other => panic!("expected a line, got {other:?}"),
    }
}

#[test]
fn new_part_has_origin_planes_with_stable_ids() {
    let mut scene = Scene::new();
    let part = scene.create_part(None).unwrap();
    let names: Vec<&str> = part.features.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Top Plane", "Front Plane", "Right Plane"]);
    let ids: Vec<u32> = part.features.iter().map(|f| f.id.0).collect();
    assert_eq!(ids, [1, 2, 3]);
    assert_eq!(scene.create_part(None), Err(PartError::PartExists));
}

#[test]
fn rectangle_extrudes_to_an_exact_block() {
    let mut scene = block();
    assert_body(&mut scene, [100.0, 60.0, 20.0], 120_000.0);
    let body = scene.part_body().unwrap();
    // Six planar faces: an exact B-rep box, not a mesh.
    assert_eq!(body.faces.len(), 6);
    assert!(body.validate().is_empty());
    let part = scene.part().unwrap().unwrap();
    assert!(part.features.iter().all(|f| f.status == FeatureStatus::Ok));
    let solid = Handle::new(part.body.unwrap());
    assert!(matches!(
        scene.document.get_entity(solid),
        Some(EntityType::Solid3D(_))
    ));
}

#[test]
fn sketch_dimension_edit_solves_the_sketch_and_regenerates_the_extrusion() {
    let mut scene = block();
    let part = scene.part().unwrap().unwrap();
    let sketch = part.find("Sketch1").unwrap().id;
    let dims = scene.part_sketch_dimensions(sketch);
    let labels: Vec<(&str, f64)> = dims.iter().map(|d| (d.label.as_str(), d.value)).collect();
    assert_eq!(labels, [("width", 100.0), ("height", 60.0)]);

    let report = scene.part_set_parameter("Sketch1", "width", 150.0).unwrap();
    assert!(report.complete());
    assert_body(&mut scene, [150.0, 60.0, 20.0], 180_000.0);

    // The constraint solver kept the rectangle closed and square, with the
    // origin corner where it was.
    let FeatureKind::Sketch(data) = &scene
        .part()
        .unwrap()
        .unwrap()
        .find("Sketch1")
        .unwrap()
        .kind
        .clone()
    else {
        panic!()
    };
    let lines: Vec<Handle> = data.entities.iter().map(|h| Handle::new(*h)).collect();
    let (bottom_start, bottom_end) = line(&scene, lines[0]);
    assert!((bottom_start.x).abs() < TOL && (bottom_start.y).abs() < TOL);
    assert!((bottom_end.x - 150.0).abs() < TOL && bottom_end.y.abs() < TOL);
    let (top_start, top_end) = line(&scene, lines[2]);
    assert!((top_start.x - 150.0).abs() < TOL && (top_start.y - 60.0).abs() < TOL);
    assert!(top_end.x.abs() < TOL && (top_end.y - 60.0).abs() < TOL);
}

#[test]
fn moving_a_sketch_line_directly_also_regenerates() {
    // The change pipeline, not the edit command, drives regeneration: any
    // edit of a sketch curve that goes through bump_entities rebuilds.
    let mut scene = block();
    scene
        .set_named_parameter_and_solve("Sketch1_height", "45")
        .unwrap();
    assert_body(&mut scene, [100.0, 45.0, 20.0], 90_000.0);
}

#[test]
fn extrusion_depth_edit_rebuilds_only_the_extrusion() {
    let mut scene = block();
    scene.part_set_parameter("Sketch1", "width", 150.0).unwrap();
    let report = scene.part_set_parameter("Extrude1", "depth", 30.0).unwrap();
    assert_body(&mut scene, [150.0, 60.0, 30.0], 270_000.0);
    let part = scene.part().unwrap().unwrap();
    let rebuilt: Vec<&str> = report
        .rebuilt
        .iter()
        .map(|id| part.feature(*id).unwrap().name.as_str())
        .collect();
    assert_eq!(
        rebuilt,
        ["Extrude1"],
        "planes and sketch come from the cache"
    );
}

#[test]
fn regeneration_order_follows_dependencies() {
    let mut scene = block();
    scene
        .part_add_rectangle_sketch(DatumPlane::Top, [20.0, 20.0], 30.0, 20.0)
        .unwrap();
    // The pocket sketch lies on the block's bottom face; cut up through it.
    let (_, report) = scene
        .part_add_extrude("Sketch2", 20.0, ExtrudeOperation::Cut, false)
        .unwrap();
    let part = scene.part().unwrap().unwrap();
    let position = |name: &str| {
        let id = part.find(name).unwrap().id;
        report.order.iter().position(|f| *f == id).unwrap()
    };
    assert!(position("Top Plane") < position("Sketch1"));
    assert!(position("Sketch1") < position("Extrude1"));
    assert!(position("Extrude1") < position("Cut-Extrude1"));
    assert!(position("Sketch2") < position("Cut-Extrude1"));
    assert_eq!(
        part.dependencies(part.find("Cut-Extrude1").unwrap().id),
        vec![
            part.find("Sketch2").unwrap().id,
            part.find("Extrude1").unwrap().id
        ]
    );
    // A through pocket: 100*60*20 - 30*20*20.
    assert_body(&mut scene, [100.0, 60.0, 20.0], 120_000.0 - 12_000.0);
    // Editing the base sketch rebuilds the base and the cut that reads it.
    let report = scene.part_set_parameter("Sketch1", "width", 150.0).unwrap();
    let rebuilt: Vec<String> = report
        .rebuilt
        .iter()
        .map(|id| part.feature(*id).unwrap().name.clone())
        .collect();
    assert_eq!(rebuilt, ["Sketch1", "Extrude1", "Cut-Extrude1"]);
    assert_body(&mut scene, [150.0, 60.0, 20.0], 180_000.0 - 12_000.0);
}

#[test]
fn invalid_depth_is_refused_and_nothing_changes() {
    let mut scene = block();
    let before = scene.part().unwrap();
    for depth in [0.0, -5.0] {
        let error = scene
            .part_set_parameter("Extrude1", "depth", depth)
            .unwrap_err();
        assert!(matches!(error, PartError::Rebuild(_)), "{error:?}");
    }
    assert_eq!(scene.part().unwrap(), before);
    assert_body(&mut scene, [100.0, 60.0, 20.0], 120_000.0);
}

#[test]
fn a_broken_sketch_fails_the_rebuild_and_keeps_the_last_good_body() {
    let mut scene = block();
    let part = scene.part().unwrap().unwrap();
    let FeatureKind::Sketch(data) = &part.find("Sketch1").unwrap().kind else {
        panic!()
    };
    // Erase one side: the profile is open.
    let side = Handle::new(data.entities[1]);
    scene.erase_entities(&[side]);
    let report = scene.part_rebuild(false).unwrap();
    assert!(!report.complete());
    let part = scene.part().unwrap().unwrap();
    let sketch = part.find("Sketch1").unwrap();
    assert!(
        matches!(&sketch.status, FeatureStatus::Error(m) if m.contains("no longer exists")),
        "{:?}",
        sketch.status
    );
    assert_eq!(
        part.find("Extrude1").unwrap().status,
        FeatureStatus::DependencyFailed(sketch.id)
    );
    // The body is not replaced by a partial result.
    assert_body(&mut scene, [100.0, 60.0, 20.0], 120_000.0);
    // New edits that would add failures are refused, but suppressing the
    // failing feature is how to get out.
    let report = scene.part_set_suppressed("Extrude1", true).unwrap();
    assert!(!report.complete());
}

#[test]
fn missing_references_are_reported_not_ignored() {
    let mut scene = block();
    assert!(matches!(
        scene.part_add_extrude("NoSuchSketch", 10.0, ExtrudeOperation::Join, false),
        Err(PartError::UnknownFeature(_))
    ));
    assert!(matches!(
        scene.part_add_extrude("Top Plane", 10.0, ExtrudeOperation::Join, false),
        Err(PartError::Invalid(_))
    ));
    assert!(matches!(
        scene.part_set_parameter("Extrude1", "taper", 3.0),
        Err(PartError::UnknownParameter { .. })
    ));
    // A second new body is refused: parts are single-body for now.
    scene
        .part_add_rectangle_sketch(DatumPlane::Top, [200.0, 0.0], 10.0, 10.0)
        .unwrap();
    assert!(matches!(
        scene.part_add_extrude("Sketch2", 10.0, ExtrudeOperation::NewBody, false),
        Err(PartError::Rebuild(_))
    ));
    // A tree whose reference points at a feature that does not exist (a
    // damaged file) reports it on regeneration.
    let mut part = scene.part().unwrap().unwrap();
    if let FeatureKind::Extrude(extrude) = &mut part
        .features
        .iter_mut()
        .find(|f| f.name == "Extrude1")
        .unwrap()
        .kind
    {
        extrude.sketch = OpenCADStudio::scene::part::FeatureId(99);
    }
    let mut cache = Default::default();
    let report =
        OpenCADStudio::scene::part::regen::regenerate(&part, &NoSketches, &mut cache, true);
    let status = report.status(part.find("Extrude1").unwrap().id).unwrap();
    assert!(
        matches!(status, FeatureStatus::Error(m) if m.contains("does not exist")),
        "{status:?}"
    );
}

struct NoSketches;

impl OpenCADStudio::scene::part::regen::SketchGeometry for NoSketches {
    fn sketch_profile(
        &self,
        _: &OpenCADStudio::scene::part::SketchFeature,
        _: &kernel::space::Plane,
    ) -> Result<OpenCADStudio::scene::part::SketchProfile, String> {
        Err("no sketches here".into())
    }
    fn sketch_digest(&self, _: &OpenCADStudio::scene::part::SketchFeature) -> u64 {
        0
    }
}

#[test]
fn suppressing_the_base_feature_removes_the_body_and_restoring_brings_it_back() {
    let mut scene = block();
    let body = scene.part().unwrap().unwrap().body.unwrap();
    scene.part_set_suppressed("Extrude1", true).unwrap();
    let part = scene.part().unwrap().unwrap();
    assert_eq!(part.body, None);
    assert!(scene.document.get_entity(Handle::new(body)).is_none());
    assert_eq!(
        part.find("Extrude1").unwrap().status,
        FeatureStatus::Suppressed
    );
    scene.part_set_suppressed("Extrude1", false).unwrap();
    assert_body(&mut scene, [100.0, 60.0, 20.0], 120_000.0);
}

#[test]
fn a_sketch_on_the_front_plane_extrudes_along_its_normal() {
    let mut scene = Scene::new();
    scene.populate_new_drawing_defaults();
    scene.create_part(None).unwrap();
    scene
        .part_add_rectangle_sketch(DatumPlane::Front, [0.0, 0.0], 40.0, 25.0)
        .unwrap();
    scene
        .part_add_extrude("Sketch1", 10.0, ExtrudeOperation::NewBody, false)
        .unwrap();
    let measure = assert_body(&mut scene, [40.0, 10.0, 25.0], 10_000.0);
    // Front's normal is -Y.
    assert!((measure.min[1] + 10.0).abs() < TOL && measure.max[1].abs() < TOL);
}

fn reopen(scene: &Scene, extension: &str) -> Scene {
    let bytes =
        OpenCADStudio::io::save_to_bytes(&scene.document, extension, scene.document.version)
            .expect("the part saves");
    let document = OpenCADStudio::io::load_bytes(&format!("part.{extension}"), bytes)
        .expect("the part reloads");
    Scene::from_loaded_document(document)
}

#[test]
fn the_part_survives_dwg_and_dxf_round_trips_and_stays_editable() {
    let mut scene = block();
    scene.part_set_parameter("Sketch1", "width", 150.0).unwrap();
    let saved = scene.part().unwrap().unwrap();
    for extension in ["dwg", "dxf"] {
        let mut reopened = reopen(&scene, extension);
        let part = reopened
            .part()
            .unwrap()
            .expect("the feature tree is in the file");
        assert_eq!(part, saved, "{extension}: tree, ids, references, statuses");
        // The body read back from the file's ACIS data.
        assert_body(&mut reopened, [150.0, 60.0, 20.0], 180_000.0);
        // Sketch dimensions and constraints came back from the file too.
        let dims = reopened.part_sketch_dimensions(part.find("Sketch1").unwrap().id);
        let width = dims.iter().find(|d| d.label == "width").expect("width");
        assert!((width.value - 150.0).abs() < TOL, "{extension}");

        reopened
            .part_set_parameter("Extrude1", "depth", 30.0)
            .unwrap();
        assert_body(&mut reopened, [150.0, 60.0, 30.0], 270_000.0);
        // The sketch is still driven after reopening: the constraints
        // keep it a rectangle while the width changes.
        reopened
            .part_set_parameter("Sketch1", "width", 120.0)
            .unwrap();
        assert_body(&mut reopened, [120.0, 60.0, 30.0], 216_000.0);
    }
}

#[test]
fn a_drawing_without_a_part_is_untouched() {
    let mut scene = Scene::new();
    scene.populate_new_drawing_defaults();
    let line = scene.add_entity(EntityType::Line(codec::entities::Line::from_points(
        Vector3::new(0.0, 0.0, 0.0),
        Vector3::new(10.0, 0.0, 0.0),
    )));
    assert_eq!(scene.part().unwrap(), None);
    let reopened = reopen(&scene, "dxf");
    assert_eq!(reopened.part().unwrap(), None);
    assert!(reopened.document.get_entity(line).is_some());
}
