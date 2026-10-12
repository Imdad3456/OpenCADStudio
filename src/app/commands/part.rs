//! Part commands: the feature tree from the command line.
//!
//! NEWPART starts a part; PARTRECT draws a constrained, dimensioned
//! rectangle sketch on an origin plane; PARTSKETCH makes a sketch of curves
//! already drawn; PARTEXTRUDE extrudes a sketch; PARTEDIT changes a feature
//! parameter or sketch dimension; PARTSUPPRESS / PARTUNSUPPRESS turn a
//! feature off and on; PARTREBUILD regenerates; PARTTREE lists the tree;
//! PARTSELECT selects what a feature made. Every edit is one undo step.
//!
//! A sketch made with CREATESKETCH on an origin plane joins the tree when
//! FINISHSKETCH closes it (see `layerprops.rs`).

use crate::app::{Message, OpenCADStudio};
use crate::scene::part::{
    BodyMeasure, DatumPlane, ExtrudeOperation, FeatureKind, FeatureStatus, PartDocument, PartError,
    RegenReport,
};
use codec::Handle;
use iced::Task;

const USAGE: &[(&str, &str)] = &[
    ("NEWPART", "NEWPART [name]"),
    ("PARTRECT", "PARTRECT <XY|XZ|YZ> <x,y> <width> <height>"),
    (
        "PARTSKETCH",
        "PARTSKETCH <XY|XZ|YZ> [handle ...] (no handles: the Previous selection)",
    ),
    (
        "PARTEXTRUDE",
        "PARTEXTRUDE <sketch> <depth> [New|Join|Cut] [Reverse]",
    ),
    ("PARTEDIT", "PARTEDIT <feature> <parameter> <value>"),
    ("PARTSUPPRESS", "PARTSUPPRESS <feature>"),
    ("PARTUNSUPPRESS", "PARTUNSUPPRESS <feature>"),
    ("PARTREBUILD", "PARTREBUILD [All]"),
    ("PARTTREE", "PARTTREE"),
    ("PARTSELECT", "PARTSELECT <feature>"),
];

/// Every command this family answers.
pub(crate) const PART_COMMANDS: &[&str] = &[
    "NEWPART",
    "PARTRECT",
    "PARTSKETCH",
    "PARTEXTRUDE",
    "PARTEDIT",
    "PARTSUPPRESS",
    "PARTUNSUPPRESS",
    "PARTREBUILD",
    "PARTTREE",
    "PARTSELECT",
];

fn usage(verb: &str) -> String {
    let line = USAGE
        .iter()
        .find(|(name, _)| *name == verb)
        .map_or(verb, |(_, line)| line);
    format!("Usage: {line}")
}

fn parse_number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn parse_xy(text: &str) -> Option<[f64; 2]> {
    let (x, y) = text.split_once(',')?;
    Some([parse_number(x)?, parse_number(y)?])
}

fn status_text(part: &PartDocument, status: &FeatureStatus) -> String {
    let name = |id| {
        part.feature(id)
            .map_or_else(|| format!("{id}"), |f| f.name.clone())
    };
    match status {
        FeatureStatus::NotBuilt => "not built".into(),
        FeatureStatus::Ok => "ok".into(),
        FeatureStatus::Suppressed => "suppressed".into(),
        FeatureStatus::SuppressedByParent(id) => format!("suppressed with {}", name(*id)),
        FeatureStatus::Error(message) => format!("ERROR: {message}"),
        FeatureStatus::DependencyFailed(id) => format!("ERROR: {} failed", name(*id)),
    }
}

fn measure_text(measure: &BodyMeasure) -> String {
    let [x, y, z] = measure.size();
    format!(
        "{} x {} x {}, volume {}",
        trim(x),
        trim(y),
        trim(z),
        trim(measure.volume)
    )
}

fn trim(value: f64) -> String {
    let rounded = (value * 1e6).round() / 1e6;
    crate::scene::part::format_value(rounded)
}

impl OpenCADStudio {
    pub(super) fn dispatch_part(&mut self, cmd: &str, i: usize) -> Option<Task<Message>> {
        let cmd = cmd.trim();
        let (verb, rest) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
        let verb = verb.to_ascii_uppercase();
        if !PART_COMMANDS.contains(&verb.as_str()) {
            return None;
        }
        let args: Vec<&str> = rest.split_whitespace().collect();
        match verb.as_str() {
            "NEWPART" => {
                let name = rest.trim();
                self.part_edit(i, "NEWPART", true, |scene| {
                    scene
                        .create_part(Some(name))
                        .map(|part| format!("{} created with its origin planes.", part.name))
                });
            }
            "PARTRECT" => {
                let parsed = match args.as_slice() {
                    [plane, corner, width, height] => DatumPlane::parse(plane)
                        .zip(parse_xy(corner))
                        .zip(parse_number(width).zip(parse_number(height))),
                    _ => None,
                };
                let Some(((plane, corner), (width, height))) = parsed else {
                    self.command_line.push_error(&usage(&verb));
                    return Some(Task::none());
                };
                // The dynamic-dimension layer may be created: a structural
                // change, so this step keeps a whole-structure snapshot.
                self.part_edit(i, "PARTRECT", false, |scene| {
                    let made = scene.part_add_rectangle_sketch(plane, corner, width, height)?;
                    let name = scene
                        .part()?
                        .and_then(|part| part.feature(made.feature).map(|f| f.name.clone()))
                        .unwrap_or_default();
                    Ok(format!(
                        "{name}: rectangle on the {} driven by {} = {} and {} = {}.",
                        plane.label(),
                        made.width_parameter,
                        trim(width),
                        made.height_parameter,
                        trim(height)
                    ))
                });
            }
            "PARTSKETCH" => {
                let Some(plane) = args.first().and_then(|p| DatumPlane::parse(p)) else {
                    self.command_line.push_error(&usage(&verb));
                    return Some(Task::none());
                };
                let handles: Vec<Handle> = if args.len() > 1 {
                    let parsed: Option<Vec<Handle>> = args[1..]
                        .iter()
                        .map(|h| u64::from_str_radix(h, 16).ok().map(Handle::new))
                        .collect();
                    let Some(parsed) = parsed else {
                        self.command_line.push_error(&usage(&verb));
                        return Some(Task::none());
                    };
                    parsed
                } else {
                    self.tabs[i].prev_selection.clone()
                };
                self.part_edit(i, "PARTSKETCH", true, |scene| {
                    let id = scene.part_add_sketch(plane, &handles)?;
                    let part = scene.part()?.ok_or(PartError::NoPart)?;
                    let feature = part.feature(id).expect("just added");
                    Ok(format!(
                        "{} added on the {} ({}).",
                        feature.name,
                        plane.label(),
                        status_text(&part, &feature.status)
                    ))
                });
            }
            "PARTEXTRUDE" => {
                let (sketch, depth) = match args.as_slice() {
                    [sketch, depth, ..] => (sketch.to_string(), parse_number(depth)),
                    _ => (String::new(), None),
                };
                let mut operation = ExtrudeOperation::NewBody;
                let mut reverse = false;
                let mut valid = depth.is_some();
                for option in args.iter().skip(2) {
                    if option.eq_ignore_ascii_case("reverse") || option.eq_ignore_ascii_case("flip")
                    {
                        reverse = true;
                    } else if let Some(op) = ExtrudeOperation::parse(option) {
                        operation = op;
                    } else {
                        valid = false;
                    }
                }
                let Some(depth) = depth.filter(|_| valid) else {
                    self.command_line.push_error(&usage(&verb));
                    return Some(Task::none());
                };
                self.part_edit(i, "PARTEXTRUDE", true, |scene| {
                    let (id, report) =
                        scene.part_add_extrude(&sketch, depth, operation, reverse)?;
                    let part = scene.part()?.ok_or(PartError::NoPart)?;
                    let name = part.feature(id).map(|f| f.name.clone()).unwrap_or_default();
                    Ok(format!("{name} built. {}", body_summary(scene, &report)))
                });
            }
            "PARTEDIT" => {
                let parsed = match args.as_slice() {
                    [feature, parameter, value] => {
                        parse_number(value).map(|v| (feature.to_string(), parameter.to_string(), v))
                    }
                    _ => None,
                };
                let Some((feature, parameter, value)) = parsed else {
                    self.command_line.push_error(&usage(&verb));
                    return Some(Task::none());
                };
                self.part_edit(i, "PARTEDIT", true, |scene| {
                    let report = scene.part_set_parameter(&feature, &parameter, value)?;
                    let part = scene.part()?.ok_or(PartError::NoPart)?;
                    if !report.complete() {
                        return Ok(format!(
                            "{feature}.{parameter} = {}, but the part does not rebuild: {}",
                            trim(value),
                            crate::scene::part::describe_errors(&part, &report)
                        ));
                    }
                    Ok(format!(
                        "{feature}.{parameter} = {}; rebuilt {} feature(s). {}",
                        trim(value),
                        report.rebuilt.len(),
                        body_summary(scene, &report)
                    ))
                });
            }
            "PARTSUPPRESS" | "PARTUNSUPPRESS" => {
                let Some(feature) = args.first().map(|f| f.to_string()) else {
                    self.command_line.push_error(&usage(&verb));
                    return Some(Task::none());
                };
                let suppressed = verb == "PARTSUPPRESS";
                self.part_edit(i, &verb, true, |scene| {
                    let report = scene.part_set_suppressed(&feature, suppressed)?;
                    Ok(format!(
                        "{feature} {}. {}",
                        if suppressed { "suppressed" } else { "restored" },
                        body_summary(scene, &report)
                    ))
                });
            }
            "PARTREBUILD" => {
                let force = args.first().is_some_and(|a| a.eq_ignore_ascii_case("all"));
                self.part_edit(i, "PARTREBUILD", true, |scene| {
                    let report = scene.part_rebuild(force)?;
                    let part = scene.part()?.ok_or(PartError::NoPart)?;
                    if report.complete() {
                        Ok(format!(
                            "Rebuilt {} of {} feature(s). {}",
                            report.rebuilt.len(),
                            part.features.len(),
                            body_summary(scene, &report)
                        ))
                    } else {
                        Ok(format!(
                            "The part does not rebuild; the body keeps its last good shape. {}",
                            crate::scene::part::describe_errors(&part, &report)
                        ))
                    }
                });
            }
            "PARTTREE" => self.print_part_tree(i),
            "PARTSELECT" => {
                let Some(feature) = args.first() else {
                    self.command_line.push_error(&usage(&verb));
                    return Some(Task::none());
                };
                self.select_part_feature(i, feature);
            }
            _ => return None,
        }
        Some(Task::none())
    }

    /// Runs one part edit as one undo step and reports the outcome.
    fn part_edit(
        &mut self,
        i: usize,
        label: &str,
        delta_safe: bool,
        edit: impl FnOnce(&mut crate::scene::Scene) -> Result<String, PartError>,
    ) {
        let pending = self.begin_undo(i, label.to_string(), 0, delta_safe);
        let outcome = edit(&mut self.tabs[i].scene);
        match outcome {
            Ok(message) => {
                self.tabs[i].dirty = true;
                self.command_line.push_output(&message);
            }
            Err(error) => self.command_line.push_error(&error.to_string()),
        }
        if let Some(pending) = pending {
            self.commit_undo_delta(i, pending);
        }
        self.refresh_properties();
    }

    fn print_part_tree(&mut self, i: usize) {
        let scene = &mut self.tabs[i].scene;
        let part = match scene.part() {
            Ok(Some(part)) => part,
            Ok(None) => {
                self.command_line.push_error(&PartError::NoPart.to_string());
                return;
            }
            Err(error) => {
                self.command_line.push_error(&error.to_string());
                return;
            }
        };
        let body = scene
            .part_body_measure()
            .map_or_else(|| "no body".to_string(), |m| measure_text(&m));
        let mut lines = vec![format!("{}: {body}", part.name)];
        for feature in &part.features {
            let detail = match &feature.kind {
                FeatureKind::DatumPlane { plane } => format!("{} plane", plane.axes_name()),
                FeatureKind::Sketch(sketch) => {
                    let plane = part
                        .feature(sketch.plane)
                        .map_or("?".to_string(), |f| f.name.clone());
                    let dims: Vec<String> = scene
                        .part_sketch_dimensions(feature.id)
                        .iter()
                        .map(|d| format!("{}={}", d.label, trim(d.value)))
                        .collect();
                    format!(
                        "on {plane}, {} curve(s){}{}",
                        sketch.entities.len(),
                        if dims.is_empty() { "" } else { ", " },
                        dims.join(" ")
                    )
                }
                FeatureKind::Extrude(extrude) => {
                    let sketch = part
                        .feature(extrude.sketch)
                        .map_or("?".to_string(), |f| f.name.clone());
                    format!(
                        "{sketch}, depth={}{}, {:?}",
                        trim(extrude.depth),
                        if extrude.reverse { " reversed" } else { "" },
                        extrude.operation
                    )
                }
            };
            lines.push(format!(
                "  {} {} [{}]: {}",
                feature.id,
                feature.name,
                detail,
                status_text(&part, &feature.status)
            ));
        }
        for line in lines {
            self.command_line.push_output(&line);
        }
    }

    fn select_part_feature(&mut self, i: usize, key: &str) {
        let scene = &mut self.tabs[i].scene;
        let part = match scene.part() {
            Ok(Some(part)) => part,
            Ok(None) => return self.command_line.push_error(&PartError::NoPart.to_string()),
            Err(error) => return self.command_line.push_error(&error.to_string()),
        };
        let Some(feature) = part.find(key) else {
            return self
                .command_line
                .push_error(&PartError::UnknownFeature(key.to_string()).to_string());
        };
        let handles: Vec<Handle> = match &feature.kind {
            FeatureKind::Sketch(sketch) => {
                sketch.entities.iter().map(|h| Handle::new(*h)).collect()
            }
            FeatureKind::Extrude(_) => part.body.map(Handle::new).into_iter().collect(),
            FeatureKind::DatumPlane { .. } => Vec::new(),
        };
        scene.deselect_all();
        for handle in &handles {
            if scene.document.get_entity(*handle).is_some() {
                scene.select_entity(*handle, false);
            }
        }
        let hint = match &feature.kind {
            FeatureKind::Sketch(_) => {
                let dims: Vec<String> = scene
                    .part_sketch_dimensions(feature.id)
                    .iter()
                    .map(|d| format!("{}={}", d.label, trim(d.value)))
                    .collect();
                format!(
                    "{}: {}. Change one with PARTEDIT {} <dimension> <value>, or double-click its dimension.",
                    feature.name,
                    if dims.is_empty() { "no driving dimensions".to_string() } else { dims.join(", ") },
                    feature.name
                )
            }
            FeatureKind::Extrude(extrude) => format!(
                "{}: depth={}. Change it with PARTEDIT {} depth <value>.",
                feature.name,
                trim(extrude.depth),
                feature.name
            ),
            FeatureKind::DatumPlane { plane } => format!(
                "{}: CREATESKETCH {} opens a sketch on it.",
                feature.name,
                plane.axes_name()
            ),
        };
        self.command_line.push_output(&hint);
        self.refresh_properties();
    }
}

fn body_summary(scene: &mut crate::scene::Scene, report: &RegenReport) -> String {
    if !report.complete() {
        return "The body keeps its last good shape.".into();
    }
    match scene.part_body_measure() {
        Some(measure) => format!("Body: {}.", measure_text(&measure)),
        None => "The part has no body.".into(),
    }
}

inventory::submit!(crate::command::CommandRegistration {
    names: PART_COMMANDS
});
