//! Regeneration: turning a feature tree into a solid.
//!
//! Every regeneration walks the whole dependency graph in topological order,
//! but only *recomputes* a feature whose inputs changed. Each feature's
//! result is cached under a key hashed from its own definition, the keys of
//! the features it reads, and — for a sketch — a digest of its curves'
//! current geometry. A feature whose key is unchanged reuses its cached
//! result; one whose key changed is rebuilt, which changes its key and so
//! every key downstream of it. Dirty propagation therefore falls out of the
//! keys rather than being tracked by hand, and it stays right across undo,
//! redo and reopening a file, where hand-kept dirty flags go stale.
//!
//! The walk never stops at the first failure: every feature gets a status,
//! and a feature whose input failed reports which one. Whether a failed
//! regeneration may change the part's body is the caller's decision — see
//! [`RegenReport::complete`].

use super::model::{
    ExtrudeFeature, ExtrudeOperation, FeatureId, FeatureKind, FeatureStatus, PartDocument,
    SketchFeature,
};
use crate::scene::model::solid_model::{boolean_result, Bool};
use kernel::brep::Body;
use kernel::geom2d::Curve as Curve2;
use kernel::space::Plane;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Smallest extrusion depth accepted, in drawing units.
pub const MIN_DEPTH: f64 = 1e-6;

/// A sketch's closed profile in its plane's coordinates.
#[derive(Debug, Clone)]
pub struct SketchProfile {
    pub plane: Plane,
    pub curves: Vec<Curve2>,
}

/// What the tree reads from outside itself: the sketch curves.
pub trait SketchGeometry {
    /// The sketch's curves as one closed profile on `plane`, or why not.
    fn sketch_profile(
        &self,
        sketch: &SketchFeature,
        plane: &Plane,
    ) -> Result<SketchProfile, String>;
    /// A digest of everything [`Self::sketch_profile`] reads, so a sketch
    /// whose curves did not move is not rebuilt.
    fn sketch_digest(&self, sketch: &SketchFeature) -> u64;
}

#[derive(Debug, Clone)]
pub enum FeatureOutput {
    Plane(Plane),
    Profile(SketchProfile),
    Body(Body),
}

/// Per-feature results from earlier regenerations, keyed by input hash.
/// Never persisted: a reopened part rebuilds it on its first regeneration.
#[derive(Debug, Clone, Default)]
pub struct RegenCache {
    entries: HashMap<FeatureId, (u64, FeatureOutput)>,
}

impl RegenCache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct RegenReport {
    /// Every feature, in the order it was evaluated.
    pub order: Vec<FeatureId>,
    /// Features actually recomputed this time (the rest reused their cache).
    pub rebuilt: Vec<FeatureId>,
    pub statuses: Vec<(FeatureId, FeatureStatus)>,
    /// The body the last solid feature produced. `None` when the part has no
    /// active solid feature, or when the regeneration failed.
    pub body: Option<Body>,
}

impl RegenReport {
    pub fn status(&self, id: FeatureId) -> Option<&FeatureStatus> {
        self.statuses.iter().find(|(f, _)| *f == id).map(|(_, s)| s)
    }

    /// The features whose own regeneration failed, with why.
    pub fn errors(&self) -> Vec<(FeatureId, String)> {
        self.statuses
            .iter()
            .filter_map(|(id, status)| match status {
                FeatureStatus::Error(message) => Some((*id, message.clone())),
                _ => None,
            })
            .collect()
    }

    /// True when no feature failed, so `body` is the part's whole result.
    pub fn complete(&self) -> bool {
        self.statuses.iter().all(|(_, status)| !status.is_failure())
    }
}

fn hash_of(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// The order features must be evaluated in: each after everything it reads,
/// ties kept in tree order. Features on a cycle, or reading a feature that
/// does not exist, are returned separately with the reason.
fn evaluation_order(part: &PartDocument) -> (Vec<FeatureId>, Vec<(FeatureId, String)>) {
    let ids: Vec<FeatureId> = part.features.iter().map(|f| f.id).collect();
    let known: HashSet<FeatureId> = ids.iter().copied().collect();
    let mut invalid: Vec<(FeatureId, String)> = Vec::new();
    let mut pending: Vec<(FeatureId, Vec<FeatureId>)> = Vec::new();
    for &id in &ids {
        let deps = part.dependencies(id);
        if let Some(missing) = deps.iter().find(|dep| !known.contains(dep)) {
            invalid.push((
                id,
                format!("references feature {missing}, which does not exist"),
            ));
        } else if deps.contains(&id) {
            invalid.push((id, "references itself".to_string()));
        } else {
            pending.push((id, deps));
        }
    }
    let mut order: Vec<FeatureId> = Vec::new();
    let mut placed: HashSet<FeatureId> = HashSet::new();
    let blocked: HashSet<FeatureId> = invalid.iter().map(|(id, _)| *id).collect();
    loop {
        // The first feature in tree order whose inputs are all placed.
        let ready = pending.iter().position(|(_, deps)| {
            deps.iter()
                .all(|dep| placed.contains(dep) || blocked.contains(dep))
        });
        let Some(index) = ready else { break };
        let (id, _) = pending.remove(index);
        placed.insert(id);
        order.push(id);
    }
    for (id, _) in pending {
        invalid.push((id, "is part of a circular reference".to_string()));
    }
    (order, invalid)
}

/// Regenerates `part`, reusing `cache` where inputs are unchanged. `force`
/// ignores the cache and rebuilds everything.
pub fn regenerate(
    part: &PartDocument,
    geometry: &impl SketchGeometry,
    cache: &mut RegenCache,
    force: bool,
) -> RegenReport {
    if force {
        cache.clear();
    }
    let (order, invalid) = evaluation_order(part);
    let mut statuses: HashMap<FeatureId, FeatureStatus> = HashMap::new();
    let mut keys: HashMap<FeatureId, u64> = HashMap::new();
    let mut outputs: HashMap<FeatureId, FeatureOutput> = HashMap::new();
    let mut rebuilt: Vec<FeatureId> = Vec::new();
    for (id, reason) in &invalid {
        statuses.insert(*id, FeatureStatus::Error(format!("This feature {reason}.")));
    }

    for &id in &order {
        let feature = part.feature(id).expect("ordered features exist");
        if feature.suppressed {
            statuses.insert(id, FeatureStatus::Suppressed);
            continue;
        }
        let deps = part.dependencies(id);
        // An input that is off turns this off too; an input that failed
        // fails this, naming the input.
        let inherited = deps.iter().find_map(|dep| match statuses.get(dep) {
            Some(FeatureStatus::Suppressed | FeatureStatus::SuppressedByParent(_)) => {
                Some(FeatureStatus::SuppressedByParent(*dep))
            }
            Some(status) if status.is_failure() => Some(FeatureStatus::DependencyFailed(*dep)),
            _ => None,
        });
        if let Some(status) = inherited {
            statuses.insert(id, status);
            continue;
        }

        let mut key_input: Vec<u64> = vec![hash_of(
            &serde_json::to_string(&feature.kind).unwrap_or_default(),
        )];
        key_input.extend(deps.iter().map(|dep| keys.get(dep).copied().unwrap_or(0)));
        if let FeatureKind::Sketch(sketch) = &feature.kind {
            key_input.push(geometry.sketch_digest(sketch));
        }
        // Whether a solid feature comes first decides if a new body is allowed.
        key_input.push(u64::from(part.previous_solid(id).is_some()));
        let key = hash_of(&key_input);

        let result = match cache.entries.get(&id) {
            Some((cached, output)) if *cached == key => Ok(output.clone()),
            _ => {
                rebuilt.push(id);
                let computed = evaluate(part, id, &feature.kind, &deps, &outputs, geometry);
                if let Ok(output) = &computed {
                    cache.entries.insert(id, (key, output.clone()));
                } else {
                    cache.entries.remove(&id);
                }
                computed
            }
        };
        match result {
            Ok(output) => {
                keys.insert(id, key);
                outputs.insert(id, output);
                statuses.insert(id, FeatureStatus::Ok);
            }
            Err(message) => {
                statuses.insert(id, FeatureStatus::Error(message));
            }
        }
    }
    // Results for features that no longer exist are dropped, so a deleted
    // and re-added id never picks up a stale body.
    let live: HashSet<FeatureId> = part.features.iter().map(|f| f.id).collect();
    cache.entries.retain(|id, _| live.contains(id));

    let statuses: Vec<(FeatureId, FeatureStatus)> = part
        .features
        .iter()
        .map(|f| {
            (
                f.id,
                statuses.remove(&f.id).unwrap_or(FeatureStatus::NotBuilt),
            )
        })
        .collect();
    let failed = statuses.iter().any(|(_, status)| status.is_failure());
    let body = if failed {
        None
    } else {
        order
            .iter()
            .rev()
            .filter(|id| part.feature(**id).is_some_and(|f| f.kind.is_solid()))
            .find_map(|id| match outputs.get(id) {
                Some(FeatureOutput::Body(body)) => Some(body.clone()),
                _ => None,
            })
    };
    let mut evaluated = order;
    evaluated.extend(invalid.iter().map(|(id, _)| *id));
    RegenReport {
        order: evaluated,
        rebuilt,
        statuses,
        body,
    }
}

fn evaluate(
    part: &PartDocument,
    id: FeatureId,
    kind: &FeatureKind,
    deps: &[FeatureId],
    outputs: &HashMap<FeatureId, FeatureOutput>,
    geometry: &impl SketchGeometry,
) -> Result<FeatureOutput, String> {
    match kind {
        FeatureKind::DatumPlane { plane } => Ok(FeatureOutput::Plane(plane.kernel_plane())),
        FeatureKind::Sketch(sketch) => {
            let Some(FeatureOutput::Plane(plane)) = outputs.get(&sketch.plane) else {
                return Err(format!(
                    "The sketch plane {} is not a plane.",
                    part.feature(sketch.plane).map_or("?", |f| f.name.as_str())
                ));
            };
            geometry
                .sketch_profile(sketch, plane)
                .map(FeatureOutput::Profile)
        }
        FeatureKind::Extrude(extrude) => {
            let target = deps.get(1).and_then(|dep| match outputs.get(dep) {
                Some(FeatureOutput::Body(body)) => Some(body),
                _ => None,
            });
            extrude_feature(part, id, extrude, outputs, target).map(FeatureOutput::Body)
        }
    }
}

fn extrude_feature(
    part: &PartDocument,
    id: FeatureId,
    extrude: &ExtrudeFeature,
    outputs: &HashMap<FeatureId, FeatureOutput>,
    target: Option<&Body>,
) -> Result<Body, String> {
    let Some(FeatureOutput::Profile(profile)) = outputs.get(&extrude.sketch) else {
        return Err(format!(
            "{} is not a sketch.",
            part.feature(extrude.sketch)
                .map_or("The referenced feature", |f| f.name.as_str())
        ));
    };
    if !extrude.depth.is_finite() || extrude.depth < MIN_DEPTH {
        return Err(format!(
            "The depth must be a positive number; it is {}.",
            extrude.depth
        ));
    }
    let normal = profile
        .plane
        .normal()
        .ok_or_else(|| "The sketch plane is degenerate.".to_string())?;
    let signed = if extrude.reverse {
        -extrude.depth
    } else {
        extrude.depth
    };
    let direction = normal.map(|component| component * signed);
    let tool =
        kernel::brep::extrude(profile.plane, &profile.curves, direction).ok_or_else(|| {
            "The sketch profile could not be extruded: it must be one closed loop of lines, \
         arcs or a circle that encloses an area."
                .to_string()
        })?;
    let flaws = tool.validate();
    if !flaws.is_empty() {
        return Err(format!("The extruded solid is invalid: {flaws:?}"));
    }
    match extrude.operation {
        ExtrudeOperation::NewBody => {
            if part.previous_solid(id).is_some() {
                return Err(
                    "A part holds one body: only the first solid feature may start a new body; \
                     use Join or Cut."
                        .to_string(),
                );
            }
            Ok(tool)
        }
        ExtrudeOperation::Join | ExtrudeOperation::Cut => {
            let target = target.ok_or_else(|| {
                "There is no body to combine with: add a new-body extrusion first.".to_string()
            })?;
            let how = if extrude.operation == ExtrudeOperation::Cut {
                Bool::Subtract
            } else {
                Bool::Union
            };
            let result = boolean_result(how, target, &tool)
                .map_err(|snag| format!("The geometry kernel refused the boolean: {snag:?}"))?;
            if result.lumps.is_empty() || kernel::brep::body_bounds(&result).is_none() {
                return Err("The cut removes the whole body.".to_string());
            }
            Ok(result)
        }
    }
}
