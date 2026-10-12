//! Parametric parts: a feature tree that rebuilds a solid when its inputs
//! change.
//!
//! * [`model`] — the tree as data: features, ids, references, statuses.
//! * [`regen`] — dependency-ordered, cached regeneration on the geometry
//!   kernel.
//! * [`store`] — the tree's home in the drawing (an XRecord).
//! * this module — the scene side: reading sketch curves, writing the body
//!   Solid3D, and the edit operations commands call.
//!
//! The pieces it is built from already existed. Sketches are model-space
//! curves held in shape by the drawing's parametric constraints, and their
//! dimensions are named parameters, so editing one goes through the same
//! solve every other constrained edit does. What this adds is the step after
//! the solve: [`Scene::refresh_part_features`] runs from the same change
//! pipeline as associative surfaces and rebuilds whatever features read the
//! curves that moved.

pub mod model;
pub mod regen;
pub mod store;

use super::named_parameters::DrivingValue;
use super::parametric_constraints::{ConstraintKind, ParametricRef, ParametricScope};
use super::{ChangeKind, Scene};
use codec::entities::{EntityType, Line};
use codec::types::Vector3;
use codec::Handle;
use kernel::geom2d::{Arc as Arc2, Curve as Curve2, Line as Line2};
use kernel::space::Plane;
pub use model::{
    DatumPlane, ExtrudeFeature, ExtrudeOperation, Feature, FeatureId, FeatureKind, FeatureStatus,
    PartDocument, SketchFeature,
};
pub use regen::{RegenCache, RegenReport, SketchProfile};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Why a part operation was refused. Nothing in the drawing changed.
#[derive(Debug, Clone, PartialEq)]
pub enum PartError {
    NoPart,
    PartExists,
    Load(store::LoadError),
    UnknownFeature(String),
    UnknownParameter {
        feature: String,
        parameter: String,
    },
    Invalid(String),
    /// The edit would leave the part unable to rebuild; it was not applied.
    Rebuild(String),
}

impl std::fmt::Display for PartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PartError::NoPart => write!(f, "The drawing has no part. NEWPART starts one."),
            PartError::PartExists => write!(f, "The drawing already has a part."),
            PartError::Load(error) => write!(f, "The part could not be read: {error}."),
            PartError::UnknownFeature(name) => write!(f, "The part has no feature \"{name}\"."),
            PartError::UnknownParameter { feature, parameter } => {
                write!(f, "{feature} has no parameter \"{parameter}\".")
            }
            PartError::Invalid(reason) => write!(f, "{reason}"),
            PartError::Rebuild(reason) => {
                write!(f, "Rebuild failed, nothing was changed: {reason}")
            }
        }
    }
}

impl From<store::LoadError> for PartError {
    fn from(error: store::LoadError) -> Self {
        PartError::Load(error)
    }
}

/// A dimension that drives a sketch: a dimensional constraint on its curves
/// whose value is a named parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct SketchDimension {
    /// Short name shown and typed by the user (`width`).
    pub label: String,
    /// The named parameter behind it (`Sketch1_width`).
    pub parameter: String,
    pub value: f64,
}

/// What [`Scene::part_add_rectangle_sketch`] made.
#[derive(Debug, Clone)]
pub struct RectangleSketch {
    pub feature: FeatureId,
    /// Bottom, right, top, left, in the plane's own directions.
    pub lines: [Handle; 4],
    pub width_parameter: String,
    pub height_parameter: String,
}

/// Measured facts about a body, for reports and tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyMeasure {
    pub volume: f64,
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl BodyMeasure {
    pub fn of(body: &kernel::brep::Body) -> Option<Self> {
        let bounds = kernel::brep::body_bounds(body)?;
        let volume = kernel::brep::mass_properties(body)?.volume;
        Some(Self {
            volume,
            min: bounds.min,
            max: bounds.max,
        })
    }

    pub fn size(&self) -> [f64; 3] {
        std::array::from_fn(|axis| self.max[axis] - self.min[axis])
    }
}

/// Sketch curves read from the live drawing.
struct DocumentSketches<'a> {
    document: &'a codec::CadDocument,
}

fn to_array(v: Vector3) -> [f64; 3] {
    [v.x, v.y, v.z]
}

/// Angle in `[0, 2π)`.
fn wrap(angle: f64) -> f64 {
    angle.rem_euclid(std::f64::consts::TAU)
}

impl DocumentSketches<'_> {
    fn curves(&self, sketch: &SketchFeature) -> Result<Vec<&EntityType>, String> {
        sketch
            .entities
            .iter()
            .map(|value| {
                self.document
                    .get_entity(Handle::new(*value))
                    .ok_or_else(|| format!("Sketch curve {value:X} no longer exists."))
            })
            .collect()
    }
}

impl regen::SketchGeometry for DocumentSketches<'_> {
    fn sketch_digest(&self, sketch: &SketchFeature) -> u64 {
        let mut hasher = DefaultHasher::new();
        for value in &sketch.entities {
            match self.document.get_entity(Handle::new(*value)) {
                // Only what shapes the profile: a recoloured line is the
                // same sketch.
                Some(EntityType::Line(line)) => {
                    format!("L{:?}{:?}", line.start, line.end).hash(&mut hasher)
                }
                Some(EntityType::Arc(arc)) => format!(
                    "A{:?}{}{}{}{:?}",
                    arc.center, arc.radius, arc.start_angle, arc.end_angle, arc.normal
                )
                .hash(&mut hasher),
                Some(EntityType::Circle(circle)) => {
                    format!("C{:?}{}{:?}", circle.center, circle.radius, circle.normal)
                        .hash(&mut hasher)
                }
                Some(other) => {
                    // Anything else is read whole.
                    let mut copy = other.clone();
                    copy.common_mut().extended_data = Default::default();
                    format!("{copy:?}").hash(&mut hasher)
                }
                None => format!("missing {value}").hash(&mut hasher),
            }
        }
        hasher.finish()
    }

    fn sketch_profile(
        &self,
        sketch: &SketchFeature,
        plane: &Plane,
    ) -> Result<SketchProfile, String> {
        let entities = self.curves(sketch)?;
        if entities.is_empty() {
            return Err("The sketch has no curves.".to_string());
        }
        let normal = plane.normal().ok_or("The sketch plane is degenerate.")?;
        let size = entities
            .iter()
            .flat_map(|entity| match entity {
                EntityType::Line(line) => vec![line.start, line.end],
                EntityType::Arc(arc) => vec![arc.center_wcs()],
                _ => Vec::new(),
            })
            .map(|p| p.x.abs().max(p.y.abs()).max(p.z.abs()))
            .fold(1.0_f64, f64::max);
        let tolerance = 1e-9 * size.max(1.0) * 1e3;
        let on_plane = |point: [f64; 3]| {
            plane
                .distance_to(point)
                .is_some_and(|distance| distance.abs() <= tolerance)
        };

        // One closed curve (a circle, a closed polyline) is its own profile,
        // read the way EXTRUDE reads it.
        if let [entity] = entities.as_slice() {
            if !matches!(entity, EntityType::Line(_) | EntityType::Arc(_)) {
                let (profile, closed) =
                    crate::scene::model::sweep_model::extrusion_profile_of(entity)
                        .ok_or("The sketch curve cannot be used as a profile.")?;
                if !closed {
                    return Err("The sketch curve is not closed.".to_string());
                }
                let parallel = profile.plane.normal().is_some_and(|own| {
                    (own[0] * normal[0] + own[1] * normal[1] + own[2] * normal[2]).abs()
                        >= 1.0 - 1e-9
                });
                if !parallel || !on_plane(profile.plane.origin) {
                    return Err("The sketch curve does not lie on the sketch plane.".to_string());
                }
                return Ok(SketchProfile {
                    plane: profile.plane,
                    curves: profile.pieces,
                });
            }
        }

        let project = |point: Vector3| -> Result<[f64; 2], String> {
            let point = to_array(point);
            if !on_plane(point) {
                return Err("A sketch curve does not lie on the sketch plane.".to_string());
            }
            plane
                .project(point)
                .ok_or_else(|| "The sketch plane is degenerate.".to_string())
        };
        let mut curves: Vec<Curve2> = Vec::new();
        for entity in &entities {
            match entity {
                EntityType::Line(line) => {
                    let (start, end) = (project(line.start)?, project(line.end)?);
                    if (start[0] - end[0]).hypot(start[1] - end[1]) <= tolerance {
                        return Err("The sketch has a line of zero length.".to_string());
                    }
                    curves.push(Curve2::Line(Line2 { start, end }));
                }
                EntityType::Arc(arc) => {
                    let centre = project(arc.center_wcs())?;
                    let angle = |point: Vector3| -> Result<f64, String> {
                        let p = project(point)?;
                        Ok(wrap((p[1] - centre[1]).atan2(p[0] - centre[0])))
                    };
                    let (start, end, middle) = (
                        angle(arc.start_point_wcs())?,
                        angle(arc.end_point_wcs())?,
                        angle(arc.midpoint_wcs())?,
                    );
                    // Seen from the plane's side the arc may run clockwise;
                    // a 2D arc always runs counter-clockwise, so swap ends.
                    let ccw = wrap(middle - start) < wrap(end - start);
                    let (from, to) = if ccw { (start, end) } else { (end, start) };
                    let sweep = wrap(to - from);
                    curves.push(Curve2::Arc(Arc2 {
                        centre,
                        radius: arc.radius,
                        start_angle: from,
                        end_angle: from
                            + if sweep == 0.0 {
                                std::f64::consts::TAU
                            } else {
                                sweep
                            },
                    }));
                }
                _ => {
                    return Err(
                        "Only lines and arcs can share a sketch with other curves; a circle or \
                         closed polyline must be a sketch's only curve."
                            .to_string(),
                    )
                }
            }
        }
        check_single_loop(&curves, tolerance)?;
        Ok(SketchProfile {
            plane: *plane,
            curves,
        })
    }
}

/// Refuses a chain the kernel would refuse, but with a reason a user can act
/// on: every end must meet exactly one other end, and all pieces must form
/// one loop.
fn check_single_loop(curves: &[Curve2], tolerance: f64) -> Result<(), String> {
    let ends: Vec<[f64; 2]> = curves
        .iter()
        .flat_map(|curve| [curve.point_at(0.0), curve.point_at(1.0)])
        .collect();
    let meets = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).hypot(a[1] - b[1]) <= tolerance;
    let mut partner = vec![usize::MAX; ends.len()];
    for i in 0..ends.len() {
        let matches: Vec<usize> = (0..ends.len())
            .filter(|&j| j / 2 != i / 2 && meets(ends[i], ends[j]))
            .collect();
        match matches.as_slice() {
            [j] => partner[i] = *j,
            [] => {
                return Err("The sketch is open: a curve end is not joined to another.".to_string())
            }
            _ => {
                return Err(
                    "The sketch branches: three or more curve ends meet at one point.".to_string(),
                )
            }
        }
    }
    // Walk the loop from the first piece; it must visit every piece.
    let mut visited = vec![false; curves.len()];
    let mut end = 1usize;
    let mut count = 0;
    while !visited[end / 2] {
        visited[end / 2] = true;
        count += 1;
        let next = partner[end];
        end = next ^ 1;
    }
    if count != curves.len() {
        return Err(
            "The sketch has more than one closed loop; a profile with holes or islands is not \
             supported yet."
                .to_string(),
        );
    }
    Ok(())
}

impl Scene {
    /// A scene for a drawing just read from disk: parameters, constraints
    /// and solid caches loaded the way opening a file in the editor loads
    /// them. For headless use (tests, batch tools).
    pub fn from_loaded_document(document: codec::CadDocument) -> Scene {
        let mut scene = Scene::new();
        scene.document = document;
        scene.bump_layout_epoch();
        scene.bump_scale_epoch();
        scene.bump_ucs_epoch();
        scene.load_named_parameters_from_document();
        scene.load_parametric_constraints_from_document();
        scene.rebuild_derived_caches();
        scene
    }

    /// The drawing's part, if it has one.
    pub fn part(&self) -> Result<Option<PartDocument>, PartError> {
        Ok(store::load(&self.document)?)
    }

    fn require_part(&self) -> Result<PartDocument, PartError> {
        self.part()?.ok_or(PartError::NoPart)
    }

    /// Writes `part` into the drawing, recording undo before-images.
    fn write_part(&mut self, part: &PartDocument) -> Result<(), PartError> {
        let touched = store::save(&mut self.document, part).ok_or_else(|| {
            PartError::Invalid("The drawing has no named-object dictionary.".into())
        })?;
        for (handle, before) in touched.before {
            self.record_undo_object_before(handle, before);
        }
        Ok(())
    }

    /// Starts a part with its origin planes. One part per drawing for now.
    pub fn create_part(&mut self, name: Option<&str>) -> Result<PartDocument, PartError> {
        if self.part()?.is_some() {
            return Err(PartError::PartExists);
        }
        let part = PartDocument::new(name.filter(|n| !n.trim().is_empty()).unwrap_or("Part1"));
        self.write_part(&part)?;
        Ok(part)
    }

    /// The part's body as the kernel holds it.
    pub fn part_body(&mut self) -> Option<kernel::brep::Body> {
        let handle = Handle::new(self.part().ok()??.body?);
        self.restore_solid_models(&[handle]);
        self.solid_models.get(&handle).cloned()
    }

    /// Volume and bounding box of the part's body, from the kernel.
    pub fn part_body_measure(&mut self) -> Option<BodyMeasure> {
        BodyMeasure::of(&self.part_body()?)
    }

    /// Adds a sketch made of `entities`, which must already lie on `plane`.
    pub fn part_add_sketch(
        &mut self,
        plane: DatumPlane,
        entities: &[Handle],
    ) -> Result<FeatureId, PartError> {
        let mut part = self.require_part()?;
        let plane_id = part
            .datum(plane)
            .ok_or_else(|| PartError::Invalid(format!("The part has no {}.", plane.label())))?;
        let curves: Vec<u64> = entities
            .iter()
            .filter(|handle| {
                matches!(
                    self.document.get_entity(**handle),
                    Some(
                        EntityType::Line(_)
                            | EntityType::Arc(_)
                            | EntityType::Circle(_)
                            | EntityType::LwPolyline(_)
                            | EntityType::Ellipse(_)
                            | EntityType::Spline(_)
                    )
                )
            })
            .map(|handle| handle.value())
            .collect();
        if curves.is_empty() {
            return Err(PartError::Invalid(
                "A sketch needs at least one curve.".into(),
            ));
        }
        let name = part.next_name("Sketch");
        let id = part.push(
            name,
            FeatureKind::Sketch(SketchFeature {
                plane: plane_id,
                entities: curves,
            }),
        );
        let report = self.regenerate_part(&mut part, false);
        self.write_part(&part)?;
        let _ = report;
        Ok(id)
    }

    /// A rectangle on `plane` held square by constraints, with its width and
    /// height driven by named parameters `<Sketch>_width` and
    /// `<Sketch>_height` — what a CAD rectangle tool gives you.
    pub fn part_add_rectangle_sketch(
        &mut self,
        plane: DatumPlane,
        corner: [f64; 2],
        width: f64,
        height: f64,
    ) -> Result<RectangleSketch, PartError> {
        for (what, value) in [("width", width), ("height", height)] {
            if !value.is_finite() || value <= 0.0 {
                return Err(PartError::Invalid(format!(
                    "The rectangle's {what} must be a positive number."
                )));
            }
        }
        let part = self.require_part()?;
        let name = part.next_name("Sketch");
        let width_parameter = format!("{name}_width");
        let height_parameter = format!("{name}_height");
        if self.named_parameters().contains(&width_parameter)
            || self.named_parameters().contains(&height_parameter)
        {
            return Err(PartError::Invalid(format!(
                "Parameters named {width_parameter} or {height_parameter} already exist."
            )));
        }
        let frame = plane.kernel_plane();
        let corners: Vec<Vector3> = [
            [corner[0], corner[1]],
            [corner[0] + width, corner[1]],
            [corner[0] + width, corner[1] + height],
            [corner[0], corner[1] + height],
        ]
        .iter()
        .map(|uv| {
            let p = frame.point_at(*uv);
            Vector3::new(p[0], p[1], p[2])
        })
        .collect();
        let mut lines = [Handle::NULL; 4];
        for side in 0..4 {
            let line = Line::from_points(corners[side], corners[(side + 1) % 4]);
            lines[side] = self.add_entity(EntityType::Line(line));
        }

        let mut table = self.named_parameters().clone();
        table
            .set(&width_parameter, &format_value(width))
            .and_then(|_| table.set(&height_parameter, &format_value(height)))
            .map_err(|error| PartError::Invalid(error.to_string()))?;
        self.record_undo_named_parameters_before();
        self.named_parameters = table;

        let scope = ParametricScope::ModelSpace;
        let before = self
            .parametric_constraint_set(scope)
            .cloned()
            .unwrap_or_else(|| super::parametric_constraints::ParametricConstraintSet::new(scope));
        self.record_undo_parametric_constraints_before(scope, before);
        let x_axis = Vector3::new(frame.x_axis[0], frame.x_axis[1], frame.x_axis[2]);
        let y_axis = Vector3::new(frame.y_axis[0], frame.y_axis[1], frame.y_axis[2]);
        let set = self.parametric_constraint_set_mut(scope);
        for side in 0..4 {
            set.add(
                ConstraintKind::Coincident,
                vec![
                    ParametricRef::point(lines[side], 1),
                    ParametricRef::point(lines[(side + 1) % 4], 0),
                ],
                None,
            );
        }
        for side in [0, 2] {
            set.add_axis_constraint(
                ConstraintKind::Horizontal,
                vec![ParametricRef::whole(lines[side])],
                x_axis,
            );
        }
        for side in [1, 3] {
            set.add_axis_constraint(
                ConstraintKind::Vertical,
                vec![ParametricRef::whole(lines[side])],
                y_axis,
            );
        }
        let width_refs = vec![
            ParametricRef::point(lines[0], 0),
            ParametricRef::point(lines[0], 1),
        ];
        let height_refs = vec![
            ParametricRef::point(lines[1], 0),
            ParametricRef::point(lines[1], 1),
        ];
        let width_id = set.add(
            ConstraintKind::Distance,
            width_refs,
            Some(DrivingValue::Named(width_parameter.clone())),
        );
        let height_id = set.add(
            ConstraintKind::Distance,
            height_refs,
            Some(DrivingValue::Named(height_parameter.clone())),
        );
        // The dimensions a user sees and double-clicks to edit, offset out
        // of the rectangle.
        let offset = 0.15 * width.min(height);
        let place = |uv: [f64; 2]| {
            let p = frame.point_at(uv);
            Vector3::new(p[0], p[1], p[2])
        };
        let dims = [
            (
                width_id,
                width_parameter.clone(),
                width,
                lines[0],
                corners[0],
                corners[1],
                place([corner[0] + width / 2.0, corner[1] - offset]),
            ),
            (
                height_id,
                height_parameter.clone(),
                height,
                lines[1],
                corners[1],
                corners[2],
                place([corner[0] + width + offset, corner[1] + height / 2.0]),
            ),
        ];
        self.ensure_dynamic_dimension_layer();
        for (id, parameter, value, line, first, second, location) in dims {
            let text = super::parametric_constraints::dynamic_dimension_text(
                &parameter,
                value,
                self.constraint_name_format,
                false,
                Some(&format_value(value)),
                false,
                None,
            );
            let dvec = |v: Vector3| glam::DVec3::new(v.x, v.y, v.z);
            let mut entity = crate::modules::annotate::aligned_dim::aligned_dimension_entity(
                dvec(first),
                dvec(second),
                dvec(location),
                Some(text),
            );
            entity
                .as_entity_mut()
                .set_layer(super::parametric_constraints::DYNAMIC_DIMENSION_LAYER.to_string());
            entity.common_mut().color = codec::types::Color::Rgb {
                r: 103,
                g: 109,
                b: 118,
            };
            let dimension = self.add_entity(entity);
            self.parametric_constraint_set_mut(scope)
                .dimensions
                .insert(id, dimension);
            self.attach_dimension_association(dimension, vec![Some(line), Some(line)]);
        }
        self.bump_constraints_epoch();
        let changes: Vec<(Handle, ChangeKind)> =
            lines.iter().map(|h| (*h, ChangeKind::Modified)).collect();
        self.bump_entities(&changes);

        let feature = self.part_add_sketch(plane, &lines)?;
        Ok(RectangleSketch {
            feature,
            lines,
            width_parameter,
            height_parameter,
        })
    }

    /// Adds an extrusion of `sketch`. Refused, with nothing changed, when the
    /// part cannot rebuild with it.
    pub fn part_add_extrude(
        &mut self,
        sketch: &str,
        depth: f64,
        operation: ExtrudeOperation,
        reverse: bool,
    ) -> Result<(FeatureId, RegenReport), PartError> {
        let mut part = self.require_part()?;
        let sketch_feature = part
            .find(sketch)
            .ok_or_else(|| PartError::UnknownFeature(sketch.to_string()))?;
        if !matches!(sketch_feature.kind, FeatureKind::Sketch(_)) {
            return Err(PartError::Invalid(format!(
                "{} is not a sketch.",
                sketch_feature.name
            )));
        }
        let sketch_id = sketch_feature.id;
        let name = part.next_name(if operation == ExtrudeOperation::Cut {
            "Cut-Extrude"
        } else {
            "Extrude"
        });
        let id = part.push(
            name,
            FeatureKind::Extrude(ExtrudeFeature {
                sketch: sketch_id,
                depth,
                reverse,
                operation,
            }),
        );
        let report = self.commit_part_edit(part)?;
        Ok((id, report))
    }

    /// Regenerates a candidate tree and, only if every feature builds, makes
    /// it the part and its body the part's body.
    fn commit_part_edit(&mut self, mut part: PartDocument) -> Result<RegenReport, PartError> {
        let mut trial = self.part_cache.clone();
        let report = {
            let sketches = DocumentSketches {
                document: &self.document,
            };
            regen::regenerate(&part, &sketches, &mut trial, false)
        };
        // An edit may leave failures the part already had — suppressing the
        // feature that fails is how a user gets out of one — but it may not
        // add any.
        let failing_before: Vec<FeatureId> = self
            .part()
            .ok()
            .flatten()
            .map(|current| {
                current
                    .features
                    .iter()
                    .filter(|f| f.status.is_failure())
                    .map(|f| f.id)
                    .collect()
            })
            .unwrap_or_default();
        let new_failures = report
            .statuses
            .iter()
            .any(|(id, status)| status.is_failure() && !failing_before.contains(id));
        if new_failures {
            let reason = describe_errors(&part, &report);
            return Err(PartError::Rebuild(if reason.is_empty() {
                "a feature depends on one that failed.".to_string()
            } else {
                reason
            }));
        }
        self.part_cache = trial;
        apply_statuses(&mut part, &report);
        if report.complete() {
            self.apply_part_body(&mut part, report.body.clone())
                .map_err(PartError::Rebuild)?;
        }
        self.write_part(&part)?;
        Ok(report)
    }

    /// The dimensions driving `sketch`.
    pub fn part_sketch_dimensions(&self, sketch: FeatureId) -> Vec<SketchDimension> {
        let Ok(Some(part)) = self.part() else {
            return Vec::new();
        };
        let Some(feature) = part.feature(sketch) else {
            return Vec::new();
        };
        let FeatureKind::Sketch(data) = &feature.kind else {
            return Vec::new();
        };
        let prefix = format!("{}_", feature.name);
        let mut found: Vec<SketchDimension> = Vec::new();
        for set in &self.parametric_constraints {
            for constraint in &set.constraints {
                let Some(DrivingValue::Named(parameter)) = &constraint.driving_param else {
                    continue;
                };
                if !constraint.enabled
                    || !constraint
                        .refs
                        .iter()
                        .any(|r| data.entities.contains(&r.entity.value()))
                    || found.iter().any(|d| &d.parameter == parameter)
                {
                    continue;
                }
                let value = self
                    .named_parameters()
                    .resolve(parameter)
                    .unwrap_or(f64::NAN);
                let label = parameter
                    .strip_prefix(&prefix)
                    .unwrap_or(parameter)
                    .to_string();
                found.push(SketchDimension {
                    label,
                    parameter: parameter.clone(),
                    value,
                });
            }
        }
        found
    }

    /// Sets one parameter of a feature: an extrusion's `depth`, or a
    /// sketch dimension by label (`width`) or parameter name.
    ///
    /// An extrusion edit that would not rebuild is refused unapplied. A
    /// sketch edit re-solves the sketch first, so it is applied, and the
    /// regeneration that follows reports any failure on the features; the
    /// body keeps its last good shape until the part rebuilds again.
    pub fn part_set_parameter(
        &mut self,
        feature: &str,
        parameter: &str,
        value: f64,
    ) -> Result<RegenReport, PartError> {
        let mut part = self.require_part()?;
        let target = part
            .find(feature)
            .ok_or_else(|| PartError::UnknownFeature(feature.to_string()))?
            .clone();
        if !value.is_finite() {
            return Err(PartError::Invalid("The value must be a number.".into()));
        }
        match &target.kind {
            FeatureKind::Extrude(_) => {
                let Some(FeatureKind::Extrude(extrude)) =
                    part.feature_mut(target.id).map(|f| &mut f.kind)
                else {
                    unreachable!("found above");
                };
                match parameter.to_ascii_lowercase().as_str() {
                    "depth" | "distance" | "d" => extrude.depth = value,
                    "reverse" | "flip" => extrude.reverse = value != 0.0,
                    _ => {
                        return Err(PartError::UnknownParameter {
                            feature: target.name.clone(),
                            parameter: parameter.to_string(),
                        })
                    }
                }
                self.commit_part_edit(part)
            }
            FeatureKind::Sketch(_) => {
                let dimension = self
                    .part_sketch_dimensions(target.id)
                    .into_iter()
                    .find(|d| {
                        d.label.eq_ignore_ascii_case(parameter)
                            || d.parameter.eq_ignore_ascii_case(parameter)
                    })
                    .ok_or_else(|| PartError::UnknownParameter {
                        feature: target.name.clone(),
                        parameter: parameter.to_string(),
                    })?;
                if value <= 0.0 {
                    return Err(PartError::Invalid(format!(
                        "{} must be a positive length.",
                        dimension.label
                    )));
                }
                self.part_last_report = None;
                self.set_named_parameter_and_solve(&dimension.parameter, &format_value(value))?;
                // The solve's change pipeline rebuilt the part; report what
                // that rebuild did. If nothing moved, it did not run.
                let _ = &mut part;
                match self.part_last_report.take() {
                    Some(report) => Ok(report),
                    None => self.part_rebuild(false),
                }
            }
            FeatureKind::DatumPlane { .. } => Err(PartError::Invalid(format!(
                "{} has no editable parameters.",
                target.name
            ))),
        }
    }

    /// Redefines named parameter `name` and re-solves what it drives, as the
    /// dimension value prompt does.
    pub fn set_named_parameter_and_solve(
        &mut self,
        name: &str,
        expression: &str,
    ) -> Result<(), PartError> {
        let mut table = self.named_parameters().clone();
        table
            .set(name, expression)
            .map_err(|error| PartError::Invalid(error.to_string()))?;
        if !table.resolve(name).is_ok_and(f64::is_finite) {
            return Err(PartError::Invalid("Invalid expression.".into()));
        }
        self.record_undo_named_parameters_before();
        self.named_parameters = table;
        let readers: Vec<_> = self
            .parametric_constraints
            .iter()
            .flat_map(|set| set.constraints.iter())
            .filter(|c| {
                c.enabled && matches!(&c.driving_param, Some(DrivingValue::Named(n)) if n == name)
            })
            .cloned()
            .collect();
        let anchors: Vec<ParametricRef> = readers
            .iter()
            .flat_map(|c| {
                super::parametric_constraints::dimensional_anchor_refs(&self.document, &c.refs)
            })
            .collect();
        let mut changes: Vec<(Handle, ChangeKind)> = Vec::new();
        for reference in readers.iter().flat_map(|c| c.refs.iter()) {
            if !changes.iter().any(|(h, _)| *h == reference.entity) {
                changes.push((reference.entity, ChangeKind::Modified));
            }
        }
        if changes.is_empty() {
            self.sync_native_parametric_graph();
        } else {
            self.bump_entities_with_parametric_policy(&changes, &anchors, false);
        }
        self.refresh_dynamic_dimension_texts();
        Ok(())
    }

    /// Suppresses or restores a feature. Refused when the part would not
    /// rebuild.
    pub fn part_set_suppressed(
        &mut self,
        feature: &str,
        suppressed: bool,
    ) -> Result<RegenReport, PartError> {
        let mut part = self.require_part()?;
        let id = part
            .find(feature)
            .ok_or_else(|| PartError::UnknownFeature(feature.to_string()))?
            .id;
        let entry = part.feature_mut(id).expect("found above");
        if matches!(entry.kind, FeatureKind::DatumPlane { .. }) {
            return Err(PartError::Invalid(
                "Origin planes cannot be suppressed.".into(),
            ));
        }
        entry.suppressed = suppressed;
        self.commit_part_edit(part)
    }

    /// Regenerates the part from its current inputs and saves the outcome.
    /// Unlike an edit, a rebuild always records what it found: the statuses
    /// are written even when features fail, and then the body is left as it
    /// was rather than replaced by a partial result.
    pub fn part_rebuild(&mut self, force: bool) -> Result<RegenReport, PartError> {
        let mut part = self.require_part()?;
        let report = self.regenerate_part(&mut part, force);
        if report.complete() {
            self.apply_part_body(&mut part, report.body.clone())
                .map_err(PartError::Rebuild)?;
        }
        if self.part().ok().flatten().as_ref() != Some(&part) {
            self.write_part(&part)?;
        }
        Ok(report)
    }

    /// Regenerates `part` against the live drawing and stores the statuses
    /// in it (not in the drawing).
    fn regenerate_part(&mut self, part: &mut PartDocument, force: bool) -> RegenReport {
        let mut cache = std::mem::take(&mut self.part_cache);
        let report = {
            let sketches = DocumentSketches {
                document: &self.document,
            };
            regen::regenerate(part, &sketches, &mut cache, force)
        };
        self.part_cache = cache;
        apply_statuses(part, &report);
        report
    }

    /// Shows `body` as the part's Solid3D: replaced in place when it exists,
    /// created when it does not, erased when the part has no body.
    fn apply_part_body(
        &mut self,
        part: &mut PartDocument,
        body: Option<kernel::brep::Body>,
    ) -> Result<(), String> {
        let existing = part.body.map(Handle::new).filter(|handle| {
            matches!(
                self.document.get_entity(*handle),
                Some(EntityType::Solid3D(_))
            )
        });
        let Some(body) = body else {
            if let Some(handle) = existing {
                self.erase_entities(&[handle]);
            }
            part.body = None;
            return Ok(());
        };
        let sat = crate::scene::convert::acis_export::solid_to_sat(&body)
            .ok_or("The rebuilt solid could not be encoded as ACIS.")?;
        match existing {
            Some(handle) => {
                let display = self
                    .prepare_solid_model_display(handle, &body)
                    .ok_or("The rebuilt solid could not be displayed.")?;
                let before = self
                    .document
                    .get_entity(handle)
                    .cloned()
                    .map(std::sync::Arc::new);
                self.record_undo_before(handle, before);
                if let Some(EntityType::Solid3D(solid)) = self.document.get_entity_mut(handle) {
                    solid.set_sat_document(&sat);
                }
                self.solid_models.remove(&handle);
                self.register_prepared_solid_model(handle, body, display);
            }
            None => {
                let mut solid = codec::entities::Solid3D::new();
                solid.set_sat_document(&sat);
                solid.wires = crate::scene::model::solid_model::edge_wires(&body);
                let handle = self.add_entity(EntityType::Solid3D(solid));
                if handle.is_null() || !self.register_solid_model(handle, body) {
                    if !handle.is_null() {
                        self.rollback_new_entities(&[handle]);
                    }
                    return Err("The rebuilt solid could not be displayed.".into());
                }
                part.body = Some(handle.value());
            }
        }
        Ok(())
    }

    /// The change-pipeline step: features reading curves that just changed
    /// are rebuilt. The part's own body changing (undo restoring it, an edit
    /// already rebuilding it) is left alone.
    pub(crate) fn refresh_part_features(
        &mut self,
        changes: &[(Handle, ChangeKind)],
    ) -> Vec<(Handle, ChangeKind)> {
        if store::record_handle(&self.document).is_none() || self.part_refresh_active {
            return Vec::new();
        }
        let Ok(Some(part)) = self.part() else {
            return Vec::new();
        };
        if part
            .body
            .is_some_and(|body| changes.iter().any(|(h, _)| h.value() == body))
        {
            return Vec::new();
        }
        let touched = changes
            .iter()
            .any(|(handle, _)| part.sketches_using(handle.value()).next().is_some());
        if !touched {
            return Vec::new();
        }
        self.part_refresh_active = true;
        let before = part.body;
        let outcome = self.part_rebuild(false);
        self.part_refresh_active = false;
        if let Ok(report) = &outcome {
            self.part_last_report = Some(report.clone());
        }
        match (outcome, self.part().ok().flatten().and_then(|p| p.body)) {
            (Ok(_), Some(body)) => {
                let kind = if before == Some(body) {
                    ChangeKind::Modified
                } else {
                    ChangeKind::Added
                };
                vec![(Handle::new(body), kind)]
            }
            _ => Vec::new(),
        }
    }
}

fn apply_statuses(part: &mut PartDocument, report: &RegenReport) {
    for (id, status) in &report.statuses {
        if let Some(feature) = part.feature_mut(*id) {
            feature.status = status.clone();
        }
    }
}

/// One line per failing feature.
pub fn describe_errors(part: &PartDocument, report: &RegenReport) -> String {
    report
        .errors()
        .iter()
        .map(|(id, message)| {
            let name = part
                .feature(*id)
                .map_or_else(|| id.to_string(), |f| f.name.clone());
            format!("{name}: {message}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A length as a parameter expression: no exponent, no trailing zeros.
pub fn format_value(value: f64) -> String {
    let text = format!("{value:.10}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-" {
        "0".to_string()
    } else {
        text.to_string()
    }
}
