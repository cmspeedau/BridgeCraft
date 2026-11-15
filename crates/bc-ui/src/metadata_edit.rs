//! Metadata panel view-state and write queue (issue #11).
//!
//! The panel draws through this module so its logic — dirty tracking, revert,
//! applying a form to a [`Metadata`] snapshot, resolving the #5 write policy,
//! and queueing per-file writes that continue past failures — is testable
//! without an egui context. All parsing/writing goes through `bc-metadata`.

use std::path::{Path, PathBuf};

use bc_metadata::{
    ratings::{ColorLabel, Flag, Rating},
    Metadata, Value, WritePolicy, NS_EXIF,
};
use egui::Color32;

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

// ---------------------------------------------------------------------------
// Rating, color label, flag and keywords (issue #10's missing UI)
// ---------------------------------------------------------------------------

/// The label chips, in panel order. The name is what gets stored in
/// `photoshop:Label`. Red, yellow, green, blue and purple are the names Adobe's
/// published Photoshop namespace documents; orange and grey are label names
/// BridgeCraft defines for its own chips (the property is free text, so other
/// tools read them as custom labels rather than losing them).
///
/// These colors are data encoding, not decoration, which is why they sit
/// outside the app's single-accent chrome palette.
pub const LABEL_CHOICES: &[(&str, Color32)] = &[
    ("", NO_LABEL),
    ("Red", RED),
    ("Orange", ORANGE),
    ("Yellow", YELLOW),
    ("Green", GREEN),
    ("Blue", BLUE),
    ("Purple", PURPLE),
    ("Grey", GREY_LABEL),
];

// Each color is picked to clear WCAG's 3:1 non-text contrast against this app's
// dark panel, which `label_colors_clear_non_text_contrast` checks. They are
// pinned values rather than theme colors because one label must look the same
// in the grid, the rows, the panel and the bulk editor.
const RED: Color32 = Color32::from_rgb(226, 88, 79);
const ORANGE: Color32 = Color32::from_rgb(230, 138, 58);
const YELLOW: Color32 = Color32::from_rgb(216, 194, 74);
const GREEN: Color32 = Color32::from_rgb(96, 190, 108);
const BLUE: Color32 = Color32::from_rgb(86, 149, 226);
const PURPLE: Color32 = Color32::from_rgb(170, 116, 222);
const GREY_LABEL: Color32 = Color32::from_rgb(158, 164, 170);
/// "No label": drawn as a hollow chip so the control stays visible and clickable.
const NO_LABEL: Color32 = Color32::from_rgb(120, 126, 132);

/// The chip color for a stored label name. Unknown names (from another tool)
/// keep their own chip but are shown by name, so nothing is silently recolored.
pub fn label_color(name: &str) -> Color32 {
    LABEL_CHOICES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, color)| *color)
        .unwrap_or(GREY_LABEL)
}

/// Whether a label name is one of BridgeCraft's documented chips.
pub fn is_known_label(name: &str) -> bool {
    LABEL_CHOICES.iter().any(|(known, _)| *known == name)
}

/// The four tag properties as edited in the panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagState {
    pub stars: u8,
    pub label: String,
    pub flag: Flag,
    pub keywords: Vec<String>,
}

impl TagState {
    pub fn from_metadata(meta: &Metadata) -> Self {
        Self {
            stars: meta.rating().stars(),
            label: meta
                .color_label()
                .as_label_str()
                .unwrap_or_default()
                .to_string(),
            flag: meta.flag(),
            keywords: bc_metadata::keywords::subjects(meta),
        }
    }

    /// Write this state's four properties into `meta`.
    pub fn apply_to(&self, meta: &mut Metadata) {
        meta.set_rating(Rating::new(self.stars));
        meta.set_color_label(ColorLabel::from_label_str(&self.label));
        meta.set_flag(self.flag);
        bc_metadata::keywords::set_subjects(meta, self.keywords.clone());
    }
}

/// The tag editor for the current selection, with dirty tracking.
#[derive(Debug, Clone, Default)]
pub struct TagsForm {
    original: TagState,
    current: TagState,
}

impl TagsForm {
    pub fn load(&mut self, meta: &Metadata) {
        self.original = TagState::from_metadata(meta);
        self.current = self.original.clone();
    }

    pub fn current(&self) -> &TagState {
        &self.current
    }

    pub fn is_dirty(&self) -> bool {
        self.original != self.current
    }

    pub fn revert(&mut self) {
        self.current = self.original.clone();
    }

    /// Set the rating. Clicking the current rating again clears it, which is
    /// how the row stays five controls instead of six with a separate "clear"
    /// button competing for attention.
    pub fn set_stars(&mut self, stars: u8) {
        self.current.stars = if self.current.stars == stars && stars > 0 {
            0
        } else {
            stars.min(5)
        };
    }

    pub fn set_label(&mut self, label: &str) {
        self.current.label = label.to_string();
    }

    pub fn set_flag(&mut self, flag: Flag) {
        self.current.flag = flag;
    }

    /// Add a keyword. Empty, whitespace-only and duplicate entries are
    /// rejected by returning false, so the UI can say so instead of writing a
    /// blank tag.
    pub fn add_keyword(&mut self, keyword: &str) -> bool {
        let trimmed = keyword.trim();
        if trimmed.is_empty() || self.current.keywords.iter().any(|k| k == trimmed) {
            return false;
        }
        self.current.keywords.push(trimmed.to_string());
        true
    }

    pub fn remove_keyword(&mut self, keyword: &str) {
        self.current.keywords.retain(|k| k != keyword);
    }
}

/// Where a tag write lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagTarget {
    /// An `.xmp` sidecar next to the original.
    Sidecar,
}

/// Save the tags for one file.
///
/// Rating, label, flag and keywords have no EXIF equivalent, so they always go
/// into the XMP sidecar and the original file is never touched. Embedded XMP
/// packets (roadmap Phase 3, remaining work) will let JPEG and TIFF carry the
/// same properties in place; until then one honest location beats a split that
/// quietly drops tags on export.
pub fn save_tags(path: &Path, tags: &TagState) -> std::result::Result<TagTarget, String> {
    let sidecar = bc_metadata::xmp::sidecar_path(path);
    let mut meta = if sidecar.exists() {
        bc_metadata::xmp::read_xmp_file(&sidecar).map_err(|e| e.to_string())?
    } else {
        Metadata::new()
    };
    tags.apply_to(&mut meta);
    bc_metadata::xmp::write_xmp_file(&sidecar, &meta).map_err(|e| e.to_string())?;
    Ok(TagTarget::Sidecar)
}

/// Per-file tag write counts, continuing past individual failures like the bulk
/// text editor does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagBulkReport {
    pub succeeded: usize,
    pub failures: Vec<(PathBuf, String)>,
}

/// Stamp only the tag properties the user actually changed, leaving the rest of
/// each file's tags alone. A field set to `None` is left as it was on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagStamp {
    pub stars: Option<u8>,
    pub label: Option<String>,
    pub flag: Option<Flag>,
    /// Keywords to add to every file rather than replace.
    pub add_keywords: Vec<String>,
    pub remove_keywords: Vec<String>,
}

impl TagStamp {
    pub fn is_empty(&self) -> bool {
        self.stars.is_none()
            && self.label.is_none()
            && self.flag.is_none()
            && self.add_keywords.is_empty()
            && self.remove_keywords.is_empty()
    }

    /// Merge the stamp into one file's existing tags.
    pub fn apply(&self, meta: &mut Metadata) {
        if let Some(stars) = self.stars {
            meta.set_rating(Rating::new(stars));
        }
        if let Some(label) = &self.label {
            meta.set_color_label(ColorLabel::from_label_str(label));
        }
        if let Some(flag) = self.flag {
            meta.set_flag(flag);
        }
        for keyword in &self.remove_keywords {
            bc_metadata::keywords::remove_keyword(meta, keyword);
        }
        for keyword in &self.add_keywords {
            bc_metadata::keywords::add_keyword(meta, keyword);
        }
    }
}

pub fn apply_stamp_to_many(paths: &[PathBuf], stamp: &TagStamp) -> TagBulkReport {
    let mut report = TagBulkReport::default();
    if stamp.is_empty() {
        return report;
    }
    for path in paths {
        let sidecar = bc_metadata::xmp::sidecar_path(path);
        let outcome = (|| -> std::result::Result<(), String> {
            let mut meta = if sidecar.exists() {
                bc_metadata::xmp::read_xmp_file(&sidecar).map_err(|e| e.to_string())?
            } else {
                Metadata::new()
            };
            stamp.apply(&mut meta);
            bc_metadata::xmp::write_xmp_file(&sidecar, &meta).map_err(|e| e.to_string())
        })();
        match outcome {
            Ok(()) => report.succeeded += 1,
            Err(error) => report.failures.push((path.clone(), error)),
        }
    }
    report
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

/// Build the panel's view of a file from every metadata layer it has.
///
/// Layers are merged lowest to highest priority, so a higher layer wins
/// property by property, which is the read precedence
/// `docs/design/0005-structured-metadata-model.md` §3 specifies: EXIF, then
/// IPTC-IIM, then an XMP packet embedded in the file, then the `.xmp` sidecar.
///
/// The file is read once and every layer parses those bytes: `exif::read_exif_file`
/// used to do its own read, so a RAW file was pulled off disk twice per panel
/// refresh for the same information.
pub fn read_panel_metadata(path: &Path) -> Metadata {
    let Ok(bytes) = std::fs::read(path) else {
        return Metadata::new();
    };
    let mut meta = bc_metadata::exif::read_exif(&bytes).unwrap_or_default();

    if let Ok(iptc) = bc_metadata::iptc::read_iptc(&bytes) {
        for (name, value) in iptc.properties() {
            meta.set(name.namespace.clone(), name.name.clone(), value.clone());
        }
    }

    if let Ok(Some(embedded)) = bc_metadata::xmp::read_embedded_xmp(&bytes) {
        merge_xmp(&mut meta, &embedded);
    }

    let sidecar = bc_metadata::xmp::sidecar_path(path);
    if sidecar.exists() {
        if let Ok(side) = bc_metadata::xmp::read_xmp_file(&sidecar) {
            merge_xmp(&mut meta, &side);
        }
    }
    meta
}

/// Project an XMP packet (embedded or sidecar) onto the panel's view.
///
/// The packet wins property by property, and it is not filtered down to EXIF
/// any more: rating (`xmp:Rating`), color label (`photoshop:Label`), flag and
/// keywords (`dc:subject`) exist only in XMP, so filtering to EXIF made the tag
/// editor write values the panel could never read back.
pub fn merge_xmp(meta: &mut Metadata, packet: &Metadata) {
    for (name, value) in packet.properties() {
        meta.set(name.namespace.clone(), name.name.clone(), value.clone());
        // EXIF and TIFF properties are additionally projected onto the short
        // EXIF namespace so the text fields show the same names regardless of
        // which container the value came from.
        if name.namespace == XMP_EXIF_URI || name.namespace == XMP_TIFF_URI {
            if let Value::Text(text) = value {
                meta.set_text(NS_EXIF, name.name.clone(), text.clone());
            }
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

    /// A JPEG carrying one marker segment before EOI, so the panel's layer
    /// chain has something real to find.
    fn jpeg_with_segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        out.push(0xFF);
        out.push(marker);
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    /// An XMP packet with the given rating, as embedded writers store it.
    fn xmp_packet(rating: &str) -> Vec<u8> {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:exif="http://ns.adobe.com/exif/1.0/">
   <xmp:Rating>{rating}</xmp:Rating>
   <exif:Make>From The Packet</exif:Make>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#
        )
        .into_bytes()
    }

    #[test]
    fn panel_layers_resolve_in_the_documented_precedence_order() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("layers.jpg");

        // Layer 3 of 4: an XMP packet embedded in the file itself, rating 3.
        let mut body = bc_metadata::embedded::XMP_JPEG_SIGNATURE.to_vec();
        body.extend_from_slice(&xmp_packet("3"));
        std::fs::write(&file, jpeg_with_segment(0xE1, &body)).unwrap();

        let meta = read_panel_metadata(&file);
        assert_eq!(
            meta.rating().stars(),
            3,
            "the embedded packet is read at all"
        );
        assert_eq!(
            meta.text(NS_EXIF, "Make"),
            Some("From The Packet"),
            "exif:Make inside XMP projects onto the panel's EXIF fields"
        );

        // Layer 4: the sidecar claims rating 5 and must win, property by
        // property, without erasing the Make only the packet carries.
        let sidecar = bc_metadata::xmp::sidecar_path(&file);
        let mut side = Metadata::new();
        side.set_rating(bc_metadata::ratings::Rating::new(5));
        side.set_text(NS_EXIF, "Model", "From The Sidecar");
        bc_metadata::xmp::write_xmp_file(&sidecar, &side).unwrap();

        let meta = read_panel_metadata(&file);
        assert_eq!(
            meta.rating().stars(),
            5,
            "the sidecar outranks embedded XMP"
        );
        assert_eq!(
            meta.text(NS_EXIF, "Make"),
            Some("From The Packet"),
            "a property the sidecar does not carry keeps the embedded value"
        );
        assert_eq!(meta.text(NS_EXIF, "Model"), Some("From The Sidecar"));

        // No file: the panel shows an empty form rather than inventing one.
        assert!(read_panel_metadata(&temp.path().join("absent.jpg")).is_empty());
    }

    #[test]
    fn a_file_with_no_metadata_layers_reads_empty_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("bare.jpg");
        std::fs::write(&file, jpeg_stub()).unwrap();
        let meta = read_panel_metadata(&file);
        assert!(meta.is_empty());
        let tags = TagState::from_metadata(&meta);
        assert_eq!(
            (tags.stars, tags.label.clone(), tags.keywords.len()),
            (0, String::new(), 0)
        );
    }

    /// The same real-world gate as `bc-metadata`'s `XMP_SAMPLE` check, one
    /// layer up: it proves the panel shows what another application wrote, not
    /// just that a locator found the bytes.
    #[test]
    fn the_panel_shows_tags_written_by_another_application() {
        let Some(sample) = std::env::var_os("XMP_SAMPLE") else {
            eprintln!("XMP_SAMPLE not set; skipping third-party panel check");
            return;
        };
        let meta = read_panel_metadata(Path::new(&sample));
        let tags = TagState::from_metadata(&meta);
        assert!(
            tags.stars > 0 || !tags.label.is_empty() || !tags.keywords.is_empty(),
            "the panel read no tags from {}: {tags:?}",
            Path::new(&sample).display()
        );
        // Report what was found so the run says something concrete, the way
        // the RAW_SAMPLE check does.
        eprintln!(
            "panel read {} : stars={} label={:?} keywords={:?} properties={}",
            Path::new(&sample).display(),
            tags.stars,
            tags.label,
            tags.keywords,
            meta.properties().len()
        );
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
    fn sidecar_wins_and_exif_namespaces_also_project_onto_the_form() {
        let mut base = Metadata::new();
        base.set_text(NS_EXIF, "Make", "Embedded");
        let mut side = Metadata::new();
        side.set_text(XMP_EXIF_URI, "Make", "Sidecar");
        side.set_text(bc_metadata::NS_DC, "title", "From the sidecar");
        side.set_text(bc_metadata::NS_XMP, "Rating", "4");
        side.set_text(bc_metadata::NS_PHOTOSHOP, "Label", "Orange");
        merge_xmp(&mut base, &side);
        assert_eq!(base.text(NS_EXIF, "Make"), Some("Sidecar"));
        assert_eq!(
            base.text(bc_metadata::NS_DC, "title"),
            Some("From the sidecar")
        );
        // The tag properties stay in their own namespaces, which is where
        // `TagState::from_metadata` reads them.
        let tags = TagState::from_metadata(&base);
        assert_eq!(tags.stars, 4);
        assert_eq!(tags.label, "Orange");
    }

    #[test]
    fn tag_round_trip_survives_the_sidecar_and_the_panel_read() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("asset.jpg");
        std::fs::write(&image, b"not really a jpeg, but the sidecar does not care").unwrap();
        let tags = TagState {
            stars: 3,
            label: "Teal".to_string(),
            flag: Flag::Approved,
            keywords: vec!["Clients|ACME".to_string(), "Keep".to_string()],
        };
        assert_eq!(
            save_tags(&image, &tags),
            Ok(TagTarget::Sidecar),
            "tags always land in the sidecar"
        );
        let sidecar_path = bc_metadata::xmp::sidecar_path(&image);
        assert!(
            sidecar_path.exists(),
            "the sidecar should sit next to the original"
        );
        // Interoperability check on the bytes themselves, not just our own
        // reader: a sidecar is only honest as a storage place if it is a real
        // XMP packet carrying the standard property names.
        let bytes = std::fs::read(&sidecar_path).unwrap();
        let text = String::from_utf8(bytes).expect("sidecar must be UTF-8 XML");
        for needle in [
            "x:xmpmeta",
            "xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"",
            "xmp:Rating",
            "photoshop:Label",
            "dc:subject",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        let read = TagState::from_metadata(&read_panel_metadata(&image));
        assert_eq!(read, tags, "what the panel wrote is what the panel reads");
    }

    #[test]
    fn stamp_changes_only_what_it_was_asked_to_and_adds_to_each_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.jpg");
        let second = dir.path().join("b.jpg");
        for (path, label) in [(&first, "Red"), (&second, "Blue")] {
            std::fs::write(path, b"x").unwrap();
            let existing = TagState {
                stars: 2,
                label: label.to_string(),
                flag: Flag::None,
                keywords: vec!["Old".to_string()],
            };
            save_tags(path, &existing).unwrap();
        }

        let stamp = TagStamp {
            stars: Some(5),
            label: None,
            flag: Some(Flag::Rejected),
            add_keywords: vec!["New".to_string()],
            remove_keywords: vec!["Old".to_string()],
        };
        let report = apply_stamp_to_many(&[first.clone(), second.clone()], &stamp);
        assert_eq!((report.succeeded, report.failures.len()), (2, 0));

        let a = TagState::from_metadata(&read_panel_metadata(&first));
        let b = TagState::from_metadata(&read_panel_metadata(&second));
        assert_eq!((a.stars, b.stars), (5, 5), "the stamp set the rating");
        assert_eq!((a.flag, b.flag), (Flag::Rejected, Flag::Rejected));
        assert_eq!(
            (a.label.as_str(), b.label.as_str()),
            ("Red", "Blue"),
            "the label was left alone because the stamp did not ask for it"
        );
        assert_eq!(a.keywords, vec!["New".to_string()], "Old was removed");
    }

    #[test]
    fn tag_form_tracks_edits_and_clears_on_repeat_click() {
        let mut form = TagsForm::default();
        form.load(&Metadata::new());
        assert!(!form.is_dirty());

        form.set_stars(3);
        assert_eq!(form.current().stars, 3);
        // Clicking the same star again clears the rating, Bridge-style.
        form.set_stars(3);
        assert_eq!(form.current().stars, 0);
        form.set_stars(9);
        assert_eq!(form.current().stars, 5, "the row has five stars");

        assert!(form.add_keyword("Redwood"));
        assert!(!form.add_keyword("  "), "blank keywords are refused");
        assert!(!form.add_keyword("Redwood"), "and duplicates are refused");
        form.remove_keyword("Redwood");
        assert!(form.current().keywords.is_empty());

        form.set_stars(4);
        assert!(form.is_dirty());
        form.revert();
        assert!(!form.is_dirty());
    }

    #[test]
    fn label_colors_clear_non_text_contrast_against_the_dark_panel() {
        // WCAG 1.4.11 wants 3:1 for meaningful graphics. The panel is egui's
        // dark default, whose central surface is rgb(27, 27, 27).
        let panel = (27u8, 27u8, 27u8);
        for (name, color) in LABEL_CHOICES {
            let ratio = contrast(*color, panel);
            assert!(
                ratio >= 3.0,
                "label {name:?} at {:?} only reaches {ratio:.2}:1 on the panel",
                color
            );
        }
    }

    fn contrast(foreground: egui::Color32, background: (u8, u8, u8)) -> f64 {
        let channel = |value: u8| {
            let s = f64::from(value) / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powi(24 / 10)
            }
        };
        let luminance = |(r, g, b): (f64, f64, f64)| 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let l1 = luminance((
            channel(foreground.r()),
            channel(foreground.g()),
            channel(foreground.b()),
        ));
        let l2 = luminance((
            channel(background.0),
            channel(background.1),
            channel(background.2),
        ));
        let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }
}
