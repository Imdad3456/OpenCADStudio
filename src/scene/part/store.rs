//! Keeping a part's feature tree in the drawing.
//!
//! The tree is one XRecord under the named-object dictionary, entry
//! [`PART_RECORD`]: its JSON split into short strings (group code 1), after
//! a format version (90) and the number of pieces (91). An XRecord is the
//! standard place for application data in a DWG or DXF; other programs carry
//! it through untouched, and it leaves the drawing's entities as ordinary
//! entities every program can still read. The body the tree builds is a
//! normal ACIS Solid3D beside it.
//!
//! Being a document object rather than scene state is also what makes undo
//! work: an edit records the XRecord's before-image like any other object,
//! and undo puts it back.

use super::model::{PartDocument, PART_FORMAT_VERSION};
use codec::objects::{ObjectType, XRecord, XRecordEntry, XRecordValue};
use codec::{CadDocument, Handle};

pub const PART_RECORD: &str = "OCS_PART";

/// Pieces are kept well under the 255-character limit older DXF readers
/// place on a string value.
const CHUNK: usize = 240;

#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// Saved by a newer build in a format this one does not know.
    NewerVersion(u32),
    /// The record is there but does not read as a feature tree.
    Corrupt(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NewerVersion(version) => write!(
                f,
                "the part was saved in feature-tree format {version}, newer than this build reads ({PART_FORMAT_VERSION})"
            ),
            LoadError::Corrupt(reason) => write!(f, "the part's feature tree is unreadable: {reason}"),
        }
    }
}

/// The handle of the part's XRecord, if the drawing has one.
pub fn record_handle(document: &CadDocument) -> Option<Handle> {
    let root = document.header.named_objects_dict_handle;
    let ObjectType::Dictionary(dictionary) = document.objects.get(&root)? else {
        return None;
    };
    let handle = dictionary.get(PART_RECORD)?;
    matches!(document.objects.get(&handle), Some(ObjectType::XRecord(_))).then_some(handle)
}

/// The part saved in the drawing. `Ok(None)` when there is none.
pub fn load(document: &CadDocument) -> Result<Option<PartDocument>, LoadError> {
    let Some(handle) = record_handle(document) else {
        return Ok(None);
    };
    let Some(ObjectType::XRecord(record)) = document.objects.get(&handle) else {
        return Ok(None);
    };
    let mut version = None;
    let mut expected = None;
    let mut json = String::new();
    let mut pieces = 0usize;
    for entry in &record.entries {
        match (entry.code, &entry.value) {
            (90, value) => version = value_int(value),
            (91, value) => expected = value_int(value),
            (1, XRecordValue::String(text)) => {
                json.push_str(text);
                pieces += 1;
            }
            _ => {}
        }
    }
    let version = version.ok_or_else(|| LoadError::Corrupt("no format version".into()))?;
    if version > PART_FORMAT_VERSION {
        return Err(LoadError::NewerVersion(version));
    }
    if expected.is_some_and(|expected| expected as usize != pieces) {
        return Err(LoadError::Corrupt(format!(
            "{pieces} of {} pieces present",
            expected.unwrap_or_default()
        )));
    }
    serde_json::from_str::<PartDocument>(&json)
        .map(Some)
        .map_err(|error| LoadError::Corrupt(error.to_string()))
}

fn value_int(value: &XRecordValue) -> Option<u32> {
    match value {
        XRecordValue::Int32(v) => u32::try_from(*v).ok(),
        XRecordValue::Int16(v) => u32::try_from(*v).ok(),
        XRecordValue::Int64(v) => u32::try_from(*v).ok(),
        XRecordValue::Byte(v) => Some(u32::from(*v)),
        _ => None,
    }
}

fn entries_for(part: &PartDocument) -> Vec<XRecordEntry> {
    let json = serde_json::to_string(part).expect("a feature tree always serializes");
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in json.chars() {
        if current.len() + c.len_utf8() > CHUNK {
            pieces.push(std::mem::take(&mut current));
        }
        current.push(c);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    let mut entries = vec![
        XRecordEntry::int32(90, PART_FORMAT_VERSION as i32),
        XRecordEntry::int32(91, pieces.len() as i32),
    ];
    entries.extend(
        pieces
            .into_iter()
            .map(|piece| XRecordEntry::string(1, piece)),
    );
    entries
}

/// What [`save`] changed, so the caller can record undo before-images:
/// each object as it was, `None` for one that did not exist.
pub struct SaveTouched {
    pub record: Handle,
    pub before: Vec<(Handle, Option<ObjectType>)>,
}

/// Writes `part` into the drawing, creating the record on first save.
pub fn save(document: &mut CadDocument, part: &PartDocument) -> Option<SaveTouched> {
    let entries = entries_for(part);
    if let Some(handle) = record_handle(document) {
        let before = document.objects.get(&handle).cloned();
        if let Some(ObjectType::XRecord(record)) = document.objects.get_mut(&handle) {
            record.entries = entries;
            record.entries_complete = true;
            record.raw_data.clear();
        }
        return Some(SaveTouched {
            record: handle,
            before: vec![(handle, before)],
        });
    }
    let root = document.header.named_objects_dict_handle;
    let root_before = match document.objects.get(&root) {
        Some(object @ ObjectType::Dictionary(_)) => object.clone(),
        _ => return None,
    };
    let handle = document.allocate_handle();
    let mut record = XRecord::named(PART_RECORD);
    record.handle = handle;
    record.owner = root;
    record.entries = entries;
    record.entries_complete = true;
    document.objects.insert(handle, ObjectType::XRecord(record));
    if let Some(ObjectType::Dictionary(dictionary)) = document.objects.get_mut(&root) {
        dictionary.add_entry(PART_RECORD, handle);
    }
    Some(SaveTouched {
        record: handle,
        before: vec![(root, Some(root_before)), (handle, None)],
    })
}
