//! Metadata panel view-state and write queue (issue #11).
//!
//! The panel draws through this module so its logic — dirty tracking, revert,
//! applying a form to a [`Metadata`] snapshot, resolving the #5 write policy,
//! and queueing per-file writes that continue past failures — is testable
//! without an egui context. All parsing/writing goes through `bc-metadata`.

use std::path::{Path, PathBuf};

use bc_metadata::{Metadata, Value, WritePolicy, NS_EXIF};

/// EXIF namespace URI used by the XMP writer for aliased EXIF properties.
const XMP_EXIF_URI: &str = "http://ns.adobe.com/exif/1.0/";
/// TIFF namespace URI, also projected onto the EXIF view.
const XMP_TIFF_URI: &str = "http://ns.adobe.com/tiff/1.0/";

/// One editable metadata property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldId {
    pub namespace: &'static str,
    pub name: &'static str,
}

/// A field the panel can edit, in stable display order.
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    pub id: FieldId,
    pub label: &'static str,
    pub multiline: bool,
    /// Also offered in the multi-selection bulk editor.
    pub bulk: bool,
}

/// Editable camera/metadata fields. Order is the panel's order.
pub const EDITABLE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "ImageDescription",
        },
        label: "Description",
        multiline: true,
        bulk: true,
    },
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "Artist",
        },
        label: "Artist",
        multiline: false,
        bulk: true,
    },
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "Copyright",
        },
        label: "Copyright",
        multiline: false,
        bulk: true,
    },
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "Make",
        },
        label: "Make",
        multiline: false,
        bulk: false,
    },
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "Model",
        },
        label: "Model",
        multiline: false,
        bulk: false,
    },
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "Software",
        },
        label: "Software",
        multiline: false,
        bulk: false,
    },
    FieldSpec {
        id: FieldId {
            namespace: NS_EXIF,
            name: "DateTimeOriginal",
        },
        label: "Date Taken",
        multiline: false,
        bulk: false,
    },
];

/// One field's original and edited value.
#[derive(Debug, Clone)]
pub struct FormValue {
    pub spec: &'static FieldSpec,
    pub original: String,
    pub current: String,
}

impl FormValue {
    pub fn is_dirty(&self) -> bool {
        self.original != self.current
    }
}

/// The editable form for one selection.
#[derive(Debug, Clone, Default)]
pub struct MetadataForm {
    values: Vec<FormValue>,
}

impl MetadataForm {
    pub fn load(&mut self, meta: &Metadata) {
        self.values = EDITABLE_FIELDS
            .iter()
            .map(|spec| {
                let original = meta
                    .text(spec.id.namespace, spec.id.name)
                    .unwrap_or("")
                    .to_string();
                FormValue {
                    spec,
                    current: original.clone(),
                    original,
                }
            })
            .collect();
    }

    #[cfg(test)]
    pub fn values(&self) -> &[FormValue] {
        &self.values
    }

    pub fn values_mut(&mut self) -> &mut [FormValue] {
        &mut self.values
    }

    pub fn is_dirty(&self) -> bool {
        self.values.iter().any(FormValue::is_dirty)
    }

    pub fn dirty_count(&self) -> usize {
        self.values.iter().filter(|v| v.is_dirty()).count()
    }

    pub fn revert(&mut self) {
        for value in &mut self.values {
            value.current.clone_from(&value.original);
        }
    }

    /// The changed fields, ready for the write queue.
    pub fn dirty_changes(&self) -> Vec<FieldChange> {
        self.values
            .iter()
            .filter(|v| v.is_dirty())
            .map(|v| FieldChange {
                id: v.spec.id,
                value: v.current.clone(),
            })
            .collect()
    }

    /// Apply the edited fields on top of `base` (untouched fields stay put).
    #[cfg(test)]
    pub fn apply(&self, base: &Metadata) -> Metadata {
        let mut meta = base.clone();
        for change in self.dirty_changes() {
            change.apply(&mut meta);
        }
        meta
    }
}

/// A single property change: empty value clears the property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldChange {
    pub id: FieldId,
    pub value: String,
}

impl FieldChange {
    pub fn apply(&self, meta: &mut Metadata) {
        if self.value.is_empty() {
            let _ = meta.remove(self.id.namespace, self.id.name);
        } else {
            meta.set_text(self.id.namespace, self.id.name, self.value.clone());
        }
    }
}

/// Where a save lands, per the #5 write policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteTarget {
    /// Rewrite the EXIF block in place (JPEG/TIFF-family).
    InPlaceExif,
    /// Write an `.xmp` sidecar next to the original (RAW and other formats).
    Sidecar,
}

const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "arw", "cr2", "cr3", "dng", "erf", "iiq", "k25", "kdc", "mef", "mos", "nef", "nrw",
    "orf", "pef", "raf", "raw", "rw2", "rwl", "sr2", "srf", "srw", "x3f",
];

/// Resolve the #5 write policy for a path. `Auto` prefers in-place EXIF for
/// JPEG/TIFF and falls back to a sidecar for RAW and everything else.
pub fn resolve_target(path: &Path, policy: WritePolicy) -> WriteTarget {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let raw = RAW_EXTENSIONS.contains(&extension.as_str());
    let exif_container = matches!(extension.as_str(), "jpg" | "jpeg" | "tif" | "tiff");
    match policy {
        WritePolicy::InPlace => WriteTarget::InPlaceExif,
        WritePolicy::Sidecar => WriteTarget::Sidecar,
        WritePolicy::Auto => {
            if exif_container && !raw {
                WriteTarget::InPlaceExif
            } else {
                WriteTarget::Sidecar
            }
        }
    }
}

/// Apply changes to one file. Errors are strings so one bad file can be
/// reported in the bulk summary without aborting the rest.
pub fn apply_changes_to_path(
    path: &Path,
    changes: &[FieldChange],
    policy: WritePolicy,
) -> std::result::Result<(), String> {
    if changes.is_empty() {
        return Ok(());
    }
    match resolve_target(path, policy) {
        WriteTarget::InPlaceExif => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let mut meta = bc_metadata::exif::read_exif(&bytes).unwrap_or_default();
            for change in changes {
                change.apply(&mut meta);
            }
            bc_metadata::exif_write::write_exif_file(path, &meta).map_err(|e| e.to_string())
        }
        WriteTarget::Sidecar => {
            let sidecar = bc_metadata::xmp::sidecar_path(path);
            let mut meta = if sidecar.exists() {
                bc_metadata::xmp::read_xmp_file(&sidecar).unwrap_or_default()
            } else {
                Metadata::new()
            };
            for change in changes {
                change.apply(&mut meta);
            }
            bc_metadata::xmp::write_xmp_file(&sidecar, &meta).map_err(|e| e.to_string())
        }
    }
}

/// Outcome of a multi-file write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BulkReport {
    pub succeeded: usize,
    pub failures: Vec<BulkFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkFailure {
    pub path: PathBuf,
    pub error: String,
}

impl BulkReport {
    #[cfg(test)]
    pub fn attempted(&self) -> usize {
        self.succeeded + self.failures.len()
    }
}

/// Apply the same changes to every path, continuing after failures.
pub fn apply_changes(
    paths: &[PathBuf],
    changes: &[FieldChange],
    policy: WritePolicy,
) -> BulkReport {
    let mut report = BulkReport::default();
    if changes.is_empty() {
        return report;
    }
    for path in paths {
        match apply_changes_to_path(path, changes, policy) {
            Ok(()) => report.succeeded += 1,
            Err(error) => report.failures.push(BulkFailure {
                path: path.clone(),
                error,
            }),
        }
    }
    report
}

/// Read the panel's metadata: embedded EXIF plus an XMP sidecar when present.
pub fn read_panel_metadata(path: &Path) -> Metadata {
    let mut meta = bc_metadata::exif::read_exif_file(path).unwrap_or_default();
    let sidecar = bc_metadata::xmp::sidecar_path(path);
    if sidecar.exists() {
        if let Ok(side) = bc_metadata::xmp::read_xmp_file(&sidecar) {
            merge_sidecar(&mut meta, &side);
        }
    }
    meta
}

/// Project sidecar EXIF/TIFF properties onto the short EXIF namespace so the
/// panel shows the same field names regardless of where the value came from.
pub fn merge_sidecar(meta: &mut Metadata, sidecar: &Metadata) {
    for (name, value) in sidecar.properties() {
        if name.namespace != XMP_EXIF_URI && name.namespace != XMP_TIFF_URI {
            continue;
        }
        match value {
            Value::Text(text) => meta.set_text(NS_EXIF, name.name.clone(), text.clone()),
            Value::Integer(number) => meta.set_text(NS_EXIF, name.name.clone(), number.to_string()),
            Value::Real(number) => meta.set_text(NS_EXIF, name.name.clone(), number.to_string()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_index(name: &str) -> usize {
        EDITABLE_FIELDS
            .iter()
            .position(|f| f.id.name == name)
            .expect("known field")
    }

    #[test]
    fn form_tracks_dirty_fields_and_reverts() {
        let mut meta = Metadata::new();
        meta.set_text(NS_EXIF, "Artist", "Original");
        let mut form = MetadataForm::default();
        form.load(&meta);
        assert!(!form.is_dirty());
        assert_eq!(form.dirty_count(), 0);

        form.values_mut()[field_index("Artist")].current = "Edited".into();
        assert!(form.is_dirty());
        assert_eq!(form.dirty_count(), 1);

        let applied = form.apply(&meta);
        assert_eq!(applied.text(NS_EXIF, "Artist"), Some("Edited"));

        form.revert();
        assert!(!form.is_dirty());
        assert_eq!(form.values()[field_index("Artist")].current, "Original");
    }

    #[test]
    fn empty_value_clears_the_property() {
        let mut meta = Metadata::new();
        meta.set_text(NS_EXIF, "Copyright", "© Someone");
        let mut form = MetadataForm::default();
        form.load(&meta);
        form.values_mut()[field_index("Copyright")].current.clear();
        let applied = form.apply(&meta);
        assert!(applied.text(NS_EXIF, "Copyright").is_none());
        // Untouched fields survive.
        form.values_mut()[field_index("Artist")].current = "A".into();
        let applied = form.apply(&meta);
        assert_eq!(applied.text(NS_EXIF, "Artist"), Some("A"));
    }

    #[test]
    fn target_resolution_follows_write_policy() {
        assert_eq!(
            resolve_target(Path::new("a.jpg"), WritePolicy::Auto),
            WriteTarget::InPlaceExif
        );
        assert_eq!(
            resolve_target(Path::new("a.TIF"), WritePolicy::Auto),
            WriteTarget::InPlaceExif
        );
        assert_eq!(
            resolve_target(Path::new("a.cr2"), WritePolicy::Auto),
            WriteTarget::Sidecar
        );
        assert_eq!(
            resolve_target(Path::new("a.dng"), WritePolicy::Auto),
            WriteTarget::Sidecar
        );
        assert_eq!(
            resolve_target(Path::new("a.jpg"), WritePolicy::Sidecar),
            WriteTarget::Sidecar
        );
        assert_eq!(
            resolve_target(Path::new("a.cr2"), WritePolicy::InPlace),
            WriteTarget::InPlaceExif
        );
    }

    fn jpeg_stub() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xD9]
    }

    #[test]
    fn bulk_queue_continues_after_failures() {
        let temp = tempfile::tempdir().unwrap();
        let good = temp.path().join("good.jpg");
        std::fs::write(&good, jpeg_stub()).unwrap();
        let missing = temp.path().join("missing.jpg");

        let changes = vec![FieldChange {
            id: FieldId {
                namespace: NS_EXIF,
                name: "Artist",
            },
            value: "Bulk Artist".into(),
        }];
        let report = apply_changes(
            &[good.clone(), missing.clone()],
            &changes,
            WritePolicy::Auto,
        );
        assert_eq!(report.succeeded, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, missing);
        assert_eq!(report.attempted(), 2);

        let meta = bc_metadata::exif::read_exif_file(&good).unwrap();
        assert_eq!(meta.text(NS_EXIF, "Artist"), Some("Bulk Artist"));
    }

    #[test]
    fn raw_writes_sidecar_and_leaves_original_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let raw = temp.path().join("shot.cr2");
        let original = b"pretend raw bytes".to_vec();
        std::fs::write(&raw, &original).unwrap();

        let changes = vec![FieldChange {
            id: FieldId {
                namespace: NS_EXIF,
                name: "Copyright",
            },
            value: "© 2026".into(),
        }];
        apply_changes_to_path(&raw, &changes, WritePolicy::Auto).unwrap();

        assert_eq!(std::fs::read(&raw).unwrap(), original);
        let sidecar = bc_metadata::xmp::sidecar_path(&raw);
        assert!(sidecar.exists());

        let mutated = vec![FieldChange {
            id: FieldId {
                namespace: NS_EXIF,
                name: "Artist",
            },
            value: "Sidecar Artist".into(),
        }];
        apply_changes(std::slice::from_ref(&raw), &mutated, WritePolicy::Auto);
        let panel = read_panel_metadata(&raw);
        assert_eq!(panel.text(NS_EXIF, "Artist"), Some("Sidecar Artist"));
        assert_eq!(panel.text(NS_EXIF, "Copyright"), Some("© 2026"));
    }

    #[test]
    fn sidecar_merge_ignores_unrelated_namespaces() {
        let mut base = Metadata::new();
        base.set_text(NS_EXIF, "Make", "Embedded");
        let mut side = Metadata::new();
        side.set_text(XMP_EXIF_URI, "Make", "Sidecar");
        side.set_text("http://purl.org/dc/elements/1.1/", "title", "Ignored");
        merge_sidecar(&mut base, &side);
        assert_eq!(base.text(NS_EXIF, "Make"), Some("Sidecar"));
        assert!(base.text(NS_EXIF, "title").is_none());
    }
}
