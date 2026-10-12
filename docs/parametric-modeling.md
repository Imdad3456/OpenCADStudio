# Parametric part modeling

Status: **milestone 1** (sketch → extrude → edit → rebuild → save → reopen → undo/redo).
Code: `src/scene/part/`, `src/app/commands/part.rs`, `src/ui/window/browser.rs`.
Tests: `tests/part_feature_tree.rs` (scene level), `src/app/part_workflow_tests.rs` (whole editor through the automation/MCP operations).

## 1. What the codebase already had

Inspected at upstream `cd07141c` (October 2026).

| Area | What exists | Used for the part feature tree? |
|---|---|---|
| Geometry kernel (`opencadkernel` @ f051138) | Exact B-rep: `brep::extrude` (analytic side walls from a 2D chain), `revolve`, `combine` (booleans), `fillet_edges`/`chamfer_edges`, `mass_properties`, `body_bounds`, `validate`; ACIS SAT/SAB read/write | **Reused as is.** Extrude, booleans, measurement. No new geometry algorithms were written. |
| 2D constraints (`opencadkernel-constraints`, `scene/parametric_constraints.rs`, `parametric_solve.rs`) | Coincident, horizontal/vertical, distance, angle, radius, … solved over model-space curves, persisted as the DWG-native associative constraint network, with DOF and conflict diagnosis | **Reused as is.** A sketch is held in shape by these constraints. |
| Named parameters (`scene/named_parameters.rs`) | Expression table (`d1 = 2*d2`), drives dimensional constraints, persisted in the DWG | **Reused.** Sketch dimensions are named parameters (`Sketch1_width`). |
| Sketch mode (`CREATESKETCH`/`FINISHSKETCH`, `app/document.rs::SketchSession`) | A *mode*: UCS + plan view + snaps; produces ordinary model-space curves | **Extended.** Finishing a sketch on an origin plane now adds a Sketch feature when the drawing has a part. |
| Solid history (`scene/model/solid_history.rs`) | DWG `AcDbShHistory` per Solid3D: primitive parameters re-editable through grips/properties, composites replayed via `kernel::acis::rebuild_history_tree` | Not used for parts. It is per-solid and has no cross-entity dependencies; it stays for DWG compatibility. |
| Associative surfaces (`scene/model/surface_sources.rs`, `Scene::refresh_associative_surfaces`) | Surfaces re-made from their profile when it changes, from the entity change pipeline | **Pattern reused.** The part rebuild hooks into the same pipeline (`bump_entities_with_solve_policy`), after the sketch solve. |
| Undo/redo (`app/history.rs`) | Delta snapshots of entities, document objects, constraint sets and parameters | **Reused.** The feature tree is a document object, so its edits are recorded like any other. |
| Browser panel (`ui/window/browser.rs`) | Origin planes, open sketch, list of solids — documented as *not* a feature tree | **Extended** with the Part section. |
| STEP (`io/step.rs`) | AP214 export of exact solids | Unchanged. Part bodies are ordinary exact Solid3Ds, so EXPORTSTEP should export them like any other solid (not covered by a test yet). |
| Automation / MCP (`app/automation.rs`, `mcp.rs`) | JSON ops (`new`, `run`, `save`, `open`, `undo`, `redo`, …) forwarded from the MCP bridge to the editor | **Used for testing.** The editor workflow test drives exactly these ops headlessly. |

Missing before this milestone: any object that ties a solid to the sketch it came from across entities; dependency ordering; a regeneration that re-runs features; a persistent feature tree.

## 2. Architecture

```
PartDocument (XRecord "OCS_PART" in the named-object dictionary, JSON)
  ├─ Feature { id: FeatureId (never reused), name, kind, suppressed, status }
  │    kind = DatumPlane{Top|Front|Right}
  │         | Sketch{ plane: FeatureId, entities: [handle] }
  │         | Extrude{ sketch: FeatureId, depth, reverse, operation: NewBody|Join|Cut }
  └─ body: handle of the Solid3D that shows the result
```

* **Sketch geometry stays ordinary DWG geometry.** Lines/arcs/circles/polylines in model space; constraints and dimensions in the drawing's own associative network. Other DWG readers still see a valid drawing; the tree is extra data they carry through.
* **Dependencies** are explicit references (`Sketch → plane`, `Extrude → sketch`) plus one implicit edge: a Join/Cut reads the body of the previous non-suppressed solid feature (`PartDocument::dependencies`).
* **Persistence**: one XRecord, JSON split into 240-character group-1 strings, with a format version (code 90) and piece count (91). A newer format version is refused rather than misread. Handles are the persistent references to sketch curves and the body; DWG and DXF round trips preserve them (tested).
* **Body**: an exact ACIS Solid3D, written with the same `solid_to_sat` + display path EXTRUDE uses. Never a mesh.

### Regeneration (`scene/part/regen.rs`)

1. **Order**: topological sort of the dependency graph, ties in tree order. A reference to a missing feature, a self-reference or a cycle marks the feature `Error` with the reason.
2. **Per feature, in order**: user-suppressed → `Suppressed`; an input suppressed → `SuppressedByParent(input)`; an input failed → `DependencyFailed(input)`; otherwise evaluate.
3. **Caching / dirty propagation**: each feature's key = hash(its definition, its inputs' keys, and for a sketch a digest of its curves' geometry). Same key → reuse the cached result; different key → rebuild. A change therefore rebuilds exactly the features downstream of it, and the cache is safe across undo, redo and reopening (stale entries simply miss). `RegenReport::rebuilt` lists what actually ran; editing an extrusion depth rebuilds only that extrusion, editing a sketch rebuilds the sketch and everything reading it (tested).
4. **Evaluation**: datum → plane; sketch → its curves projected into the plane and checked to be one closed loop (clear errors for open, branching, multi-loop, off-plane or missing curves); extrude → `kernel::brep::extrude` along the plane normal, validated, then united/subtracted with the running body.
5. **Atomic body update**: the Solid3D is replaced only when *every* feature rebuilt. On failure the statuses are saved (so the browser and PARTTREE show what broke) and the body keeps its last good shape.

### When it runs

* **Feature edits** (`PARTEXTRUDE`, `PARTEDIT <extrude> depth`, `PARTSUPPRESS`): the candidate tree is regenerated first; an edit that would add a failure is refused and nothing changes.
* **Sketch edits** go through the constraint solver: `PARTEDIT Sketch1 width 150` (or double-clicking the dynamic dimension, or `-PARAMETERS`) sets the named parameter, the solver moves the lines, and the entity change pipeline calls `Scene::refresh_part_features`, which rebuilds the part. Any edit of a sketch curve that goes through `bump_entities` (grips, MOVE, STRETCH, …) does the same. The part's own body appearing in the change set (undo restoring it, the rebuild writing it) is skipped, so there is no feedback loop.
* **Undo/redo**: every command is one undo step holding the curve, parameter, constraint, XRecord and Solid3D before-images together.

## 3. Commands

| Command | Does |
|---|---|
| `NEWPART [name]` | Starts the drawing's part with Top (XY), Front (XZ), Right (YZ) planes. One part per drawing. |
| `PARTRECT <XY\|XZ\|YZ> <x,y> <w> <h>` | Rectangle sketch: 4 lines, 4 coincident, 2 horizontal, 2 vertical, 2 driving dimensions (`SketchN_width`, `SketchN_height`) with visible dynamic dimensions. |
| `CREATESKETCH XY` … `FINISHSKETCH` | Draw anything (LINE, ARC, RECTANG, CIRCLE, constraints, DIMCONSTRAINT); on finish it becomes a sketch feature. |
| `PARTSKETCH <plane> [handles]` | Makes a sketch feature from existing curves (default: the Previous selection). |
| `PARTEXTRUDE <sketch> <depth> [New\|Join\|Cut] [Reverse]` | Extrude / cut-extrude. |
| `PARTEDIT <feature> <param> <value>` | `depth`/`reverse` on an extrusion; a dimension label (`width`) or parameter name on a sketch. |
| `PARTSUPPRESS` / `PARTUNSUPPRESS <feature>` | Suppression; dependents follow. |
| `PARTREBUILD [All]` | Regenerate (All ignores the cache). |
| `PARTTREE` | Prints the tree, statuses, parameters and the body's size and volume. |
| `PARTSELECT <feature>` | Selects a sketch's curves or the body and prints how to edit it. |

The Browser panel (`BROWSER`) shows the tree with statuses; clicking a plane opens a sketch on it, clicking a feature runs PARTSELECT.

## 4. Designed-for, not built

**Stable face/edge references.** Sketch-on-face, fillets and chamfers need references that survive topology changes. Planned scheme: name faces by *generator*, not index — an extrude's side face is `(feature, sketch curve handle)`, its caps are `(feature, start|end)`; a boolean result face inherits the name of the tool or target face it came from (the kernel already tracks per-node `Provenance`; booleans would need to carry a source-face map). A reference stores the generator name plus a geometric fingerprint (normal/centroid) as a fallback for re-matching, and a failed match surfaces as `Error("reference lost")` rather than silently binding to another face. This slots into `FeatureKind` as new reference variants; `SketchFeature::plane` becomes a `PlaneRef` enum (`Datum(FeatureId)` | `Face(FaceRef)`).

**Next features** reuse the same evaluate step: Revolve (`kernel::brep::revolve`), Fillet/Chamfer (`fillet_edges`/`chamfer_edges`, needs edge references), patterns/mirror (`kernel::brep::transform` + union).

**Assemblies.** A separate `AssemblyDocument` keyed the same way: `Component { id, source: PartRef (this file | external file + part name), transform, fixed, suppressed }`, nested subassemblies as components whose source is an assembly, and `Mate { id, kind: Coincident|Concentric|Distance|Angle|Revolute|Slider, a: GeometryRef, b: GeometryRef, params }` where `GeometryRef = (component path, part face/edge reference)` — the same stable references above. A part only ever exposes its body and named references, so the part engine does not change when assemblies arrive. The mate solver is a 3D rigid-body problem; `opencadkernel-constraints` is 2D today, so this needs a new solver crate.

**Collaboration.** The tree is plain JSON with stable ids, which is what diffing, conflict detection and version comparison need (feature-level diff by id). A server would store revisions of whole DWG files plus the extracted tree; exclusive locks already have a seed in `io/edit_lock.rs` (native edit lease).

## 5. Known limitations (milestone 1)

* One part per drawing; one body per part (a second New Body extrusion is refused).
* Sketches must lie on an origin plane; sketch-on-face is not implemented.
* A profile is one closed loop (lines + arcs, or a single circle / closed polyline / ellipse). Holes and islands in one sketch are refused with a message; cut them with a second sketch + Cut-Extrude.
* Only Extrude (New/Join/Cut, blind depth, reversible). No revolve, fillet, chamfer, pattern, mirror yet.
* The body Solid3D can still be edited directly (MOVE, solid editing); the next rebuild replaces it with the tree's result.
* The rebuilt body is the *same entity* with new ACIS data, so other objects referring to it by handle stay attached; nothing yet refers to its faces.
* Deleting a feature is not a command yet (undo covers mistakes).
* A sketch dimension edit that leaves the sketch unsolvable or the profile broken is applied (the solver already moved the curves); the part reports the failure and keeps the last good body until fixed or undone. Feature-parameter edits are refused instead.

## 6. Trying it and testing it

Desktop:

```
cargo run --release -- --new
```

then at the command line:

```
NEWPART
PARTRECT XY 0,0 100 60        (or: CREATESKETCH XY, draw, constrain, FINISHSKETCH)
PARTEXTRUDE Sketch1 20
PARTEDIT Sketch1 width 150    (or double-click the width dimension and type 150)
PARTEDIT Extrude1 depth 30
PARTTREE                      (sizes and volume from the kernel)
BROWSER                       (the feature tree panel)
```

Save as DWG/DXF, close, reopen: the tree, sketch constraints, dimensions and the body come back and stay editable. Ctrl+Z / Ctrl+Y step through every edit.

Headless (same operations the MCP bridge forwards):

```
printf '%s\n' '{"op":"new"}' '{"op":"run","cmd":"NEWPART"}' \
  '{"op":"run","cmd":"PARTRECT XY 0,0 100 60"}' '{"op":"run","cmd":"PARTEXTRUDE Sketch1 20"}' \
  | target/release/OpenCADStudio --serve
```

Tests:

```
cargo test --test integration part_feature_tree   # 13 scene-level tests
cargo test --lib part_workflow                     # 3 whole-editor workflow tests
```

## 7. Verification record (milestone 1 delivery)

| Check | Result |
|---|---|
| `cargo test --lib` | 2128 passed, 0 failed, 24 ignored (includes 3 new editor workflow tests) |
| `cargo test --test integration` | 141 passed, 0 failed (includes 13 new part tests) |
| `cargo build --bin OpenCADStudio` (Linux x86_64) | builds |
| `cargo check --lib --target wasm32-unknown-unknown` | passes, no warnings from the new code |
| Live GUI over MCP (`--mcp` → running editor under Xvfb) | NEWPART / PARTRECT / PARTEXTRUDE / PARTEDIT / save_verified / open / undo / redo all completed; measured volumes 120000 → 180000 → 270000 → undo 180000 → redo 270000 |
| `--serve` headless JSON server on the built binary | full workflow including DWG save and reopen completes |

Not verified here: anything visual (the container has no GPU; the editor ran with "No GPU renderer", so the viewport and Browser panel were not seen), the web build in a browser (`trunk build` was not run, only the wasm type-check), Windows and macOS builds.

Upstream quirk seen while testing (not caused by this change): the MCP `measure` op reports zero bounds for any Solid3D after its drawing is reopened (a plain BOX does the same); volumes are right. The kernel-side `PARTTREE` measure is unaffected.

## 8. Recommended next milestone

Sketch-on-face plus Revolve and Fillet, because together they force the one hard piece the tree still lacks: **persistent face/edge naming** (section 4). Order: (1) generator-based face names on extrude output and through booleans, (2) `PlaneRef::Face` so a sketch can sit on a face and survive upstream edits, (3) Revolve, (4) Fillet/Chamfer on named edges, (5) feature delete/reorder/rename with dependency checks, (6) an interactive feature-edit dialog in the Browser instead of PARTEDIT. Assemblies come after naming, since mates reference the same faces.
