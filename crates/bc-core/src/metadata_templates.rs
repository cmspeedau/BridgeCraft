//! Saved metadata templates: named bundles of field values, applied with a mode.
//!
//! The *format* and the merge rules live in [`bc_metadata::templates`]; this
//! module is only about keeping them. That split is deliberate: the merge logic
//! is pure and shared with the export pipeline's projections, while where the
//! files go, how they are written and what happens to a file this build cannot
//! read is a persistence question, and this crate has already been bitten by
//! answering it twice in different places.
//!
//! Templates are stored the same way saved searches are — one JSON document in
//! the data directory, written atomically, versioned, and never overwritten when
//! it cannot be parsed. A template is user-authored content that exists nowhere
//! else, so a torn write is not a cache miss.
//!
//! One consequence of the single-document choice, stated rather than hidden:
//! there is no per-template file to hand to a colleague. Import and export of an
//! individual template is a follow-up, and it belongs with this module rather
//! than in the format, because it is the same question — what happens to the
//! document — asked from the other side.

use std::path::{Path, PathBuf};

use bc_metadata::templates::MetadataTemplate;
use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

const FILE_NAME: &str = "metadata-templates.json";

/// Bumping this is a deliberate act: the reader refuses anything it does not
/// understand rather than guessing, so a new version needs a migration beside
/// it, not just a bigger number.
const DOCUMENT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    templates: Vec<MetadataTemplate>,
}

/// The user's saved templates, in one document in the data directory.
#[derive(Debug, Clone)]
pub struct MetadataTemplates {
    file: PathBuf,
    entries: Vec<MetadataTemplate>,
}

impl MetadataTemplates {
    /// Open the current user's templates. No file yet is an empty list, which is
    /// the normal first run and not an error.
    pub fn open_default() -> Result<Self> {
        Self::open(crate::paths::data_file(FILE_NAME)?)
    }

    /// Open a template store at `file`.
    ///
    /// A file that will not parse is reported and left alone. Nothing here
    /// overwrites it: one bad byte in the JSON would otherwise destroy every
    /// template on the next unrelated save, while the data is still in the file
    /// and one edit away from recoverable.
    pub fn open(file: impl Into<PathBuf>) -> Result<Self> {
        let file = file.into();
        let entries = match std::fs::read(&file) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|error| {
                    CoreError::InvalidOperation(format!(
                        "{} is not valid text, so the templates in it cannot be read: {error}",
                        file.display()
                    ))
                })?;
                parse_document(&text, &file)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let mut store = Self { file, entries };
        store.sort();
        Ok(store)
    }

    /// The document these templates live in, named because the only useful thing
    /// an interface can say about an unreadable store is where to look at it.
    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn list(&self) -> &[MetadataTemplate] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&MetadataTemplate> {
        self.entries
            .iter()
            .find(|entry| equal_names(&entry.name, name))
    }

    /// Whether that name is taken. Reported separately from `save` so the
    /// interface can say "this replaces X" *before* the click.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Store `template`, replacing anything already using its name. Returns
    /// whether an existing template was overwritten.
    ///
    /// The write happens before the in-memory change, so a failed persist cannot
    /// leave the interface showing a template that is not on disk.
    pub fn save(&mut self, template: &MetadataTemplate) -> Result<bool> {
        let name = template.name.trim();
        if name.is_empty() {
            return Err(CoreError::InvalidOperation(
                "a template needs a name".to_string(),
            ));
        }
        let replaced = self.contains(name);
        let mut entries = self.entries.clone();
        entries.retain(|entry| !equal_names(&entry.name, name));
        entries.push(MetadataTemplate {
            name: name.to_string(),
            ..template.clone()
        });
        sort_by_name(&mut entries);
        persist(&self.file, &entries)?;
        self.entries = entries;
        Ok(replaced)
    }

    /// Remove a template. Returns whether anything went, because a delete that
    /// matched nothing means the list the user was looking at had already lost it,
    /// and the interface needs to re-read rather than assume.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let before = self.entries.len();
        let mut entries = self.entries.clone();
        entries.retain(|entry| !equal_names(&entry.name, name));
        if entries.len() == before {
            return Ok(false);
        }
        persist(&self.file, &entries)?;
        self.entries = entries;
        Ok(true)
    }

    fn sort(&mut self) {
        sort_by_name(&mut self.entries);
    }
}

/// Names are compared ignoring case, because a template is found by reading its
/// label rather than by typing it, and two entries whose labels differ only in
/// letter case are one entry with a typo in it. It also keeps the list honest on
/// a case-insensitive filesystem, where the document is the only place the
/// difference would survive — and only until the next save, which would collapse
/// them.
fn equal_names(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn sort_by_name(entries: &mut [MetadataTemplate]) {
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn parse_document(text: &str, file: &Path) -> Result<Vec<MetadataTemplate>> {
    let document: Document = serde_json::from_str(text).map_err(|error| {
        CoreError::InvalidOperation(format!(
            "{} could not be read ({error}); nothing will overwrite it until it is \
             fixed or removed",
            file.display()
        ))
    })?;
    if document.version != DOCUMENT_VERSION {
        return Err(CoreError::InvalidOperation(format!(
            "{} holds templates written by version {}, and this build reads version \
             {} only",
            file.display(),
            document.version,
            DOCUMENT_VERSION
        )));
    }
    Ok(document.templates)
}

fn persist(file: &Path, entries: &[MetadataTemplate]) -> Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let document = Document {
        version: DOCUMENT_VERSION,
        templates: entries.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| {
        CoreError::InvalidOperation(format!("could not encode the templates: {error}"))
    })?;
    crate::paths::write_atomic(file, &bytes).map_err(|error| {
        CoreError::InvalidOperation(format!("could not write {}: {error}", file.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_metadata::{keywords, ratings::Rating, Metadata, NS_EXIF};

    fn meta(pairs: &[(&str, &str)]) -> Metadata {
        let mut out = Metadata::new();
        for (name, value) in pairs {
            out.set(
                NS_EXIF,
                *name,
                bc_metadata::Value::Text((*value).to_string()),
            );
        }
        out
    }

    fn template(name: &str, description: &str) -> MetadataTemplate {
        MetadataTemplate::new(name, meta(&[("ImageDescription", description)]))
    }

    fn store(dir: &Path) -> MetadataTemplates {
        MetadataTemplates::open(dir.join(FILE_NAME)).unwrap()
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let saved = store(dir.path());
        assert!(saved.list().is_empty());
        assert!(
            !saved.path().exists(),
            "opening created a file it does not own"
        );
    }

    #[test]
    fn a_saved_template_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        let one = template("client work", "Shot for ACME")
            .with_mode(bc_metadata::templates::TemplateMode::Merge);
        saved.save(&one).unwrap();

        let reopened = store(dir.path());
        assert_eq!(reopened.list().len(), 1);
        let stored = reopened.get("client work").expect("gone after reopen");
        assert_eq!(stored, &one);
        // The mode is part of what was saved: a template that comes back as
        // "replace" after it was stored as "merge" would overwrite on a second
        // application, which is the one thing the user chose to avoid.
        assert_eq!(stored.mode, bc_metadata::templates::TemplateMode::Merge);
    }

    #[test]
    fn the_name_and_mode_are_kept_beside_the_values_not_inside_them() {
        // The template's Metadata is what gets applied, so anything the store
        // adds must not leak into it: a `bridgecraft:name` property written into
        // a template would land in every file it is applied to.
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        saved.save(&template("trip", "Holiday 2024")).unwrap();
        let reopened = store(dir.path());
        let stored = reopened.get("trip").unwrap();
        assert_eq!(stored.metadata.properties.len(), 1);
        assert!(stored.metadata.get(NS_EXIF, "ImageDescription").is_some());
        assert!(stored.description.is_none());

        let described = template("t2", "x").with_description("from the shoot day");
        saved.save(&described).unwrap();
        assert_eq!(
            store(dir.path()).get("t2").unwrap().description.as_deref(),
            Some("from the shoot day")
        );
    }

    #[test]
    fn saving_the_same_name_replaces_and_says_so_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        assert!(
            !saved.save(&template("trip", "one")).unwrap(),
            "first save has nothing to replace"
        );
        assert!(
            saved.save(&template("trip", "two")).unwrap(),
            "second save should report a replacement"
        );
        assert_eq!(saved.list().len(), 1, "the name made a second entry");
        let stored = saved.get("trip").unwrap();
        assert_eq!(
            stored.metadata.text(NS_EXIF, "ImageDescription"),
            Some("two")
        );
    }

    #[test]
    fn a_name_differing_only_in_case_is_the_same_template() {
        // Two entries whose labels differ only by a letter case are one template
        // with a typo, and a case-insensitive filesystem would not keep both
        // anyway. Saving the second replaces the first rather than quietly
        // producing a list with two near-identical rows.
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        saved.save(&template("Client Work", "one")).unwrap();
        assert!(
            saved.save(&template("client work", "two")).unwrap(),
            "the case difference made a second template"
        );
        assert_eq!(saved.list().len(), 1);
        // The spelling the user typed most recently is kept, not the first.
        assert_eq!(saved.list()[0].name, "client work");
    }

    #[test]
    fn the_list_is_name_ordered_however_it_was_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        for name in ["zebra", "Alpha", "mango"] {
            saved.save(&template(name, "x")).unwrap();
        }
        let listed: Vec<&str> = saved.list().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(listed, vec!["Alpha", "mango", "zebra"]);
        let reopened = store(dir.path());
        let again: Vec<&str> = reopened.list().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(again, listed, "order came from insertion, not storage");
    }

    #[test]
    fn an_empty_name_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        for blank in ["", "   "] {
            let error = saved.save(&template(blank, "x")).unwrap_err().to_string();
            assert!(error.contains("needs a name"), "{error} for {blank:?}");
        }
        assert!(
            !saved.path().exists(),
            "a refused save still created the file"
        );
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        saved.save(&template("keep", "a")).unwrap();
        assert!(saved.remove("keep").unwrap());
        assert!(saved.list().is_empty());
        assert!(
            !saved.remove("keep").unwrap(),
            "a second delete claimed to remove something"
        );
    }

    #[test]
    fn an_unreadable_store_is_reported_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(
            &file,
            r#"{"version":1,"templates":[{"name":"one","metadata":{"properties":[]}}]} leftover"#,
        )
        .unwrap();
        let before = std::fs::read(&file).unwrap();

        let error = MetadataTemplates::open(&file).unwrap_err().to_string();
        assert!(error.contains("could not be read"), "{error}");
        assert!(
            error.contains(&file.display().to_string()),
            "the message has to say which file to look at: {error}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "a failed open still rewrote the user's templates"
        );
    }

    #[test]
    fn a_document_from_a_future_version_is_refused_rather_than_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(&file, r#"{"version":2,"templates":[]}"#).unwrap();
        let error = MetadataTemplates::open(&file).unwrap_err().to_string();
        assert!(error.contains("version 2"), "{error}");
        assert!(file.exists(), "the refused file was deleted");
    }

    #[test]
    fn keywords_and_ratings_survive_the_round_trip_as_properties() {
        // A template that sets the tag properties is the common case (a shoot's
        // keywords), and the store must not flatten them into strings on the way
        // out: `dc:subject` is an array, and a lost array becomes a lost
        // keyword list in every file the template is applied to.
        let dir = tempfile::tempdir().unwrap();
        let mut metadata = meta(&[("Artist", "J. Rivera")]);
        keywords::add_keyword(&mut metadata, "Clients|ACME");
        keywords::add_keyword(&mut metadata, "Holiday");
        metadata.set_rating(Rating::new(4));

        let mut saved = store(dir.path());
        saved
            .save(&MetadataTemplate::new("shoot", metadata.clone()))
            .unwrap();

        let reopened = store(dir.path());
        let stored = reopened.get("shoot").unwrap();
        assert_eq!(&stored.metadata, &metadata);
        assert_eq!(
            keywords::subjects(&stored.metadata),
            vec!["Clients|ACME".to_string(), "Holiday".to_string()]
        );
        assert_eq!(stored.metadata.rating(), metadata.rating());
    }

    #[test]
    fn the_templates_the_sidebar_lists_are_the_ones_on_disk() {
        // Persistence that no reader uses is the failure this repository keeps
        // having, so assert the path the interface takes: `open_default` reads
        // the same document `save` wrote, in the data directory.
        // The environment is isolated through the same helper `paths::tests` uses:
        // four process-wide variables are not a resource two tests can share, and
        // this one raced them (green under a filter, red in a full run) until it
        // was put under that lock.
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::paths::tests::isolate_env(dir.path());
        let mut saved = MetadataTemplates::open_default().unwrap();
        saved.save(&template("trip", "Holiday")).unwrap();
        let path = saved.path().to_path_buf();
        assert_eq!(
            path,
            crate::paths::tests::expected_root("DATA")
                .join("BridgeCraft")
                .join("metadata-templates.json"),
            "the document is not where the data-directory rule puts it"
        );
        assert!(
            path.starts_with(dir.path()),
            "the isolation did not take: {path:?} is outside {:?}",
            dir.path()
        );

        let reopened = MetadataTemplates::open_default().unwrap();
        assert_eq!(reopened.list().len(), 1);
        assert!(
            path.exists(),
            "the document the store names is not the file"
        );
    }
}
