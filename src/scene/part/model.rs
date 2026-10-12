//! The part feature tree: what a parametric part is made of, as data.
//!
//! A part is an ordered list of features. Each one has a stable id that is
//! never reused, a kind with its own parameters, and references to the
//! features it reads. Nothing here touches geometry — [`super::regen`] turns
//! a tree into a solid, and [`super::store`] keeps it in the drawing.
//!
//! The sketch geometry itself stays what CREATESKETCH has always produced:
//! ordinary model-space curves, constrained and dimensioned by the drawing's
//! own parametric constraints and named parameters. A sketch feature records
//! which curves are its own and which plane they lie on; the dimensions that
//! drive them are the dimensional constraints touching those curves.

use serde::{Deserialize, Serialize};

/// Format version written with every saved tree. A reader refuses a newer
/// version rather than dropping what it does not understand.
pub const PART_FORMAT_VERSION: u32 = 1;

/// A feature's identity within its part. Assigned once, never reused, and
/// independent of the feature's name or position in the tree — renaming or
/// reordering a feature leaves every reference to it intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FeatureId(pub u32);

impl std::fmt::Display for FeatureId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The three origin planes. Named as Onshape names them for a Z-up model,
/// which is how this drawing's world is oriented: Top is the XY ground plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DatumPlane {
    /// XY, normal +Z.
    Top,
    /// XZ, normal -Y (looking at the part from the front).
    Front,
    /// YZ, normal +X.
    Right,
}

impl DatumPlane {
    pub const ALL: [DatumPlane; 3] = [DatumPlane::Top, DatumPlane::Front, DatumPlane::Right];

    /// The plane's frame: origin, sketch X axis, normal.
    pub fn frame(self) -> ([f64; 3], [f64; 3], [f64; 3]) {
        match self {
            DatumPlane::Top => ([0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            DatumPlane::Front => ([0.0; 3], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0]),
            DatumPlane::Right => ([0.0; 3], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        }
    }

    pub fn kernel_plane(self) -> kernel::space::Plane {
        let (origin, x_axis, normal) = self.frame();
        kernel::space::Plane::orthonormal(origin, x_axis, normal)
            .expect("datum plane frames are orthonormal")
    }

    pub fn label(self) -> &'static str {
        match self {
            DatumPlane::Top => "Top Plane",
            DatumPlane::Front => "Front Plane",
            DatumPlane::Right => "Right Plane",
        }
    }

    /// The coordinate-plane name CREATESKETCH takes for this plane.
    pub fn axes_name(self) -> &'static str {
        match self {
            DatumPlane::Top => "XY",
            DatumPlane::Front => "XZ",
            DatumPlane::Right => "YZ",
        }
    }

    /// Accepts `XY`/`XZ`/`YZ` or `Top`/`Front`/`Right`, any case.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "XY" | "TOP" | "TOP PLANE" => Some(DatumPlane::Top),
            "XZ" | "FRONT" | "FRONT PLANE" => Some(DatumPlane::Front),
            "YZ" | "RIGHT" | "RIGHT PLANE" => Some(DatumPlane::Right),
            _ => None,
        }
    }
}

/// How an extrusion combines with the part's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtrudeOperation {
    /// Starts the body. Only the first solid feature may do this; parts are
    /// single-body for now.
    NewBody,
    /// Unites with the body built so far.
    Join,
    /// Subtracts from the body built so far.
    Cut,
}

impl ExtrudeOperation {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "NEW" | "NEWBODY" | "BOSS" => Some(Self::NewBody),
            "JOIN" | "ADD" | "UNION" => Some(Self::Join),
            "CUT" | "REMOVE" | "SUBTRACT" => Some(Self::Cut),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SketchFeature {
    /// The datum plane feature this sketch lies on.
    pub plane: FeatureId,
    /// The sketch's curves, by drawing handle value. Handles survive DWG and
    /// DXF round trips, which is what makes them usable as persistent
    /// references.
    pub entities: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtrudeFeature {
    /// The sketch whose closed profile is extruded.
    pub sketch: FeatureId,
    /// Distance along the sketch plane's normal, in drawing units.
    pub depth: f64,
    /// Extrude against the normal instead of along it.
    #[serde(default)]
    pub reverse: bool,
    pub operation: ExtrudeOperation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FeatureKind {
    DatumPlane { plane: DatumPlane },
    Sketch(SketchFeature),
    Extrude(ExtrudeFeature),
}

impl FeatureKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            FeatureKind::DatumPlane { .. } => "Plane",
            FeatureKind::Sketch(_) => "Sketch",
            FeatureKind::Extrude(extrude) => match extrude.operation {
                ExtrudeOperation::Cut => "Cut-Extrude",
                _ => "Extrude",
            },
        }
    }

    /// Whether this feature changes the part's body.
    pub fn is_solid(&self) -> bool {
        matches!(self, FeatureKind::Extrude(_))
    }
}

/// The outcome of a feature's last regeneration. Saved with the tree so a
/// reopened drawing shows the same errors it was saved with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail")]
pub enum FeatureStatus {
    /// Not regenerated since it was created or loaded.
    NotBuilt,
    Ok,
    /// Turned off by the user.
    Suppressed,
    /// Turned off because a feature it depends on is suppressed.
    SuppressedByParent(FeatureId),
    /// Its own inputs are invalid or its geometry could not be built.
    Error(String),
    /// A feature it depends on failed.
    DependencyFailed(FeatureId),
}

impl FeatureStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, FeatureStatus::Ok)
    }

    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            FeatureStatus::Error(_) | FeatureStatus::DependencyFailed(_)
        )
    }

    pub fn is_suppressed(&self) -> bool {
        matches!(
            self,
            FeatureStatus::Suppressed | FeatureStatus::SuppressedByParent(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    pub id: FeatureId,
    pub name: String,
    #[serde(default)]
    pub suppressed: bool,
    pub kind: FeatureKind,
    #[serde(default = "not_built")]
    pub status: FeatureStatus,
}

fn not_built() -> FeatureStatus {
    FeatureStatus::NotBuilt
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartDocument {
    pub version: u32,
    pub name: String,
    pub next_id: u32,
    pub features: Vec<Feature>,
    /// The Solid3D that shows the part's body, by handle value. `None` until
    /// the first solid feature builds, and again while every solid feature is
    /// suppressed.
    #[serde(default)]
    pub body: Option<u64>,
}

impl PartDocument {
    /// A new part with its three origin planes.
    pub fn new(name: impl Into<String>) -> Self {
        let mut part = Self {
            version: PART_FORMAT_VERSION,
            name: name.into(),
            next_id: 1,
            features: Vec::new(),
            body: None,
        };
        for plane in DatumPlane::ALL {
            part.push(plane.label().to_string(), FeatureKind::DatumPlane { plane });
        }
        part
    }

    /// Appends a feature, assigning it the next id.
    pub fn push(&mut self, name: String, kind: FeatureKind) -> FeatureId {
        let id = FeatureId(self.next_id);
        self.next_id += 1;
        self.features.push(Feature {
            id,
            name,
            suppressed: false,
            kind,
            status: FeatureStatus::NotBuilt,
        });
        id
    }

    pub fn feature(&self, id: FeatureId) -> Option<&Feature> {
        self.features.iter().find(|feature| feature.id == id)
    }

    pub fn feature_mut(&mut self, id: FeatureId) -> Option<&mut Feature> {
        self.features.iter_mut().find(|feature| feature.id == id)
    }

    /// A feature by name (any case) or by `#id`.
    pub fn find(&self, key: &str) -> Option<&Feature> {
        let key = key.trim();
        if let Some(id) = key.strip_prefix('#').and_then(|id| id.parse::<u32>().ok()) {
            return self.feature(FeatureId(id));
        }
        self.features
            .iter()
            .find(|feature| feature.name.eq_ignore_ascii_case(key))
    }

    pub fn datum(&self, plane: DatumPlane) -> Option<FeatureId> {
        self.features.iter().find_map(|feature| match feature.kind {
            FeatureKind::DatumPlane { plane: p } if p == plane => Some(feature.id),
            _ => None,
        })
    }

    /// `Sketch3` when two sketches exist, and so on — the first unused
    /// number, so a deleted feature's name is not handed to a new one that
    /// would then be mistaken for it.
    pub fn next_name(&self, prefix: &str) -> String {
        (1..)
            .map(|n| format!("{prefix}{n}"))
            .find(|name| {
                self.features
                    .iter()
                    .all(|f| !f.name.eq_ignore_ascii_case(name))
            })
            .expect("an unused name exists")
    }

    /// The features `id` reads, in the order they are read.
    ///
    /// A Join or Cut reads the body built so far, so it depends on the solid
    /// feature before it in the tree that is not suppressed; that is the one
    /// implicit edge. Every other edge is a reference the feature stores.
    pub fn dependencies(&self, id: FeatureId) -> Vec<FeatureId> {
        let Some(feature) = self.feature(id) else {
            return Vec::new();
        };
        match &feature.kind {
            FeatureKind::DatumPlane { .. } => Vec::new(),
            FeatureKind::Sketch(sketch) => vec![sketch.plane],
            FeatureKind::Extrude(extrude) => {
                let mut deps = vec![extrude.sketch];
                if extrude.operation != ExtrudeOperation::NewBody {
                    if let Some(previous) = self.previous_solid(id) {
                        deps.push(previous);
                    }
                }
                deps
            }
        }
    }

    /// The nearest solid feature before `id` in the tree that the user has
    /// not suppressed.
    pub fn previous_solid(&self, id: FeatureId) -> Option<FeatureId> {
        let index = self.features.iter().position(|f| f.id == id)?;
        self.features[..index]
            .iter()
            .rev()
            .find(|f| f.kind.is_solid() && !f.suppressed)
            .map(|f| f.id)
    }

    /// Every feature that reads `id`, directly or through others.
    pub fn dependents(&self, id: FeatureId) -> Vec<FeatureId> {
        let mut found: Vec<FeatureId> = Vec::new();
        let mut frontier = vec![id];
        while let Some(current) = frontier.pop() {
            for feature in &self.features {
                if !found.contains(&feature.id)
                    && feature.id != id
                    && self.dependencies(feature.id).contains(&current)
                {
                    found.push(feature.id);
                    frontier.push(feature.id);
                }
            }
        }
        found.sort_by_key(|dep| self.features.iter().position(|f| f.id == *dep));
        found
    }

    /// Sketch features whose curves include `handle`.
    pub fn sketches_using(&self, handle: u64) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(move |feature| {
            matches!(&feature.kind, FeatureKind::Sketch(sketch) if sketch.entities.contains(&handle))
        })
    }
}
