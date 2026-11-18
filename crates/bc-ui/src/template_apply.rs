//! Metadata templates as the panel applies them.
//!
//! [`bc_metadata::templates`] holds the format and the merge rules; this module
//! answers the question the interface actually asks: *what would applying this
//! template change on this file?* The answer is expressed in the only two write
//! paths this application has — a list of [`FieldChange`] for the text fields and
//! a [`TagStamp`] for the four tag properties — so applying a template cannot
//! reach a writer the panel does not already use, and cannot disagree with it
//! about where a value goes.
//!
//! The one rule that makes this safe is that **a template changes only the
//! properties it names.** `TemplateMode::Replace` replaces the whole property map
//! in the merged view, which for a template holding a copyright line would mean
//! "clear the description, the artist, the keywords, everything else the file
//! has". No user means that by "overwrite existing values", and no preview would
//! have warned them in time. So the plan is built field by field over the
//! template's own properties, and the mode decides what happens to those.
//!
//! The mode semantics survive that restriction intact, because they are applied
//! per property:
//!
//! - **Replace** — a named property takes the template's value.
//! - **Merge** — a named property is written only where the file has none.
//! - **Append** — named keyword lists are unioned; named scalars are replaced.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bc_metadata::templates::{apply_template, MetadataTemplate, TemplateMode};
use bc_metadata::{Metadata, WritePolicy, NS_BRIDGECRAFT, NS_DC, NS_PHOTOSHOP, NS_XMP};

use crate::metadata_edit::{
    apply_changes_to_path, apply_stamp_to_many, read_panel_metadata, FieldChange, TagStamp,
    EDITABLE_FIELDS,
};

/// What one template would change on one file: nothing at all, or the two sets of
/// writes the panel already knows how to perform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplatePlan {
    pub changes: Vec<FieldChange>,
    pub stamp: TagStamp,
}

impl TemplatePlan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.stamp.is_empty()
    }

    /// How many properties would be written. The count, not a field list, is what
    /// fits in a button: the field list is the template's own preview, next to it.
    pub fn property_count(&self) -> usize {
        let keywords = self.stamp.add_keywords.len() + self.stamp.remove_keywords.len();
        self.changes.len()
            + usize::from(self.stamp.stars.is_some())
            + usize::from(self.stamp.label.is_some())
            + usize::from(self.stamp.flag.is_some())
            + keywords
    }
}

/// Whether `template` says anything about that property. This is the whole
/// difference between a template and an erase button.
fn names(template: &MetadataTemplate, namespace: &str, name: &str) -> bool {
    template.metadata.get(namespace, name).is_some()
}

/// What applying `template` would change on a file whose merged metadata is
/// `base`.
pub fn plan(base: &Metadata, template: &MetadataTemplate) -> TemplatePlan {
    // The engine does the merge, so the three modes mean one thing everywhere
    // they are used. The plan then reads *only* the properties the template
    // names, which is what keeps a whole-map Replace from becoming a wipe.
    let merged = apply_template(base, template);
    let mut plan = TemplatePlan::default();

    for spec in EDITABLE_FIELDS {
        if !names(template, spec.id.namespace, spec.id.name) {
            continue;
        }
        let before = base.text(spec.id.namespace, spec.id.name).unwrap_or("");
        let after = merged.text(spec.id.namespace, spec.id.name).unwrap_or("");
        if before != after {
            plan.changes.push(FieldChange {
                id: spec.id,
                value: after.to_string(),
            });
        }
    }

    if names(template, NS_XMP, "Rating") {
        let stars = merged.rating().stars();
        if base.rating().stars() != stars {
            plan.stamp.stars = Some(stars);
        }
    }
    if names(template, NS_PHOTOSHOP, "Label") {
        let label = merged
            .color_label()
            .as_label_str()
            .unwrap_or_default()
            .to_string();
        let before = base.color_label();
        let before = before.as_label_str().unwrap_or_default();
        if before != label {
            plan.stamp.label = Some(label);
        }
    }
    if names(template, NS_BRIDGECRAFT, "Flag") {
        let flag = merged.flag();
        if base.flag() != flag {
            plan.stamp.flag = Some(flag);
        }
    }
    if names(template, NS_DC, "subject") {
        // Set difference on the full hierarchical strings, so "Clients" and
        // "Clients|ACME" stay two keywords rather than one parent and a change.
        let have: BTreeSet<String> = bc_metadata::keywords::subjects(base).into_iter().collect();
        let want: BTreeSet<String> = bc_metadata::keywords::subjects(&merged)
            .into_iter()
            .collect();
        plan.stamp.add_keywords = want.difference(&have).cloned().collect();
        plan.stamp.remove_keywords = have.difference(&want).cloned().collect();
    }
    plan
}

/// Outcome of applying a template to a set of files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Files written.
    pub changed: Vec<PathBuf>,
    /// Files where the template named nothing the file did not already say.
    /// Counted separately because "applied to 0 of 200" and "200 already matched"
    /// look identical in a success message and mean opposite things.
    pub already_matching: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
}

impl ApplyReport {
    pub fn changed(&self) -> usize {
        self.changed.len()
    }
}

/// Apply a template to one file, reading it first so the plan is about this file
/// rather than about the template in the abstract.
pub fn apply_to_path(
    path: &Path,
    template: &MetadataTemplate,
    policy: WritePolicy,
) -> Result<(), (String, bool)> {
    let base = read_panel_metadata(path);
    let plan = plan(&base, template);
    if plan.is_empty() {
        // Write nothing. The alternative is worse than a wasted save call: the
        // tag path creates a sidecar when a file has none, so an apply that
        // changed nothing would still leave a `.xmp` behind on every file it
        // touched.
        return Err(("unchanged".to_string(), true));
    }
    apply_changes_to_path(path, &plan.changes, policy).map_err(|error| (error, false))?;
    if !plan.stamp.is_empty() {
        let report = apply_stamp_to_many(&[path.to_path_buf()], &plan.stamp);
        if let Some((_, error)) = report.failures.first() {
            return Err((error.clone(), false));
        }
    }
    Ok(())
}

/// Apply a template to every path, continuing after failures.
pub fn apply_to_many(
    paths: &[PathBuf],
    template: &MetadataTemplate,
    policy: WritePolicy,
) -> ApplyReport {
    let mut report = ApplyReport::default();
    for path in paths {
        match apply_to_path(path, template, policy) {
            Ok(()) => report.changed.push(path.clone()),
            Err((_, true)) => report.already_matching.push(path.clone()),
            Err((error, false)) => report.failures.push((path.clone(), error)),
        }
    }
    report
}

/// What a mode does, in the words a user chooses between rather than the words a
/// programmer named an enum variant with. "Replace" alone does not say whether the
/// fields a template leaves out are cleared as well, which is the question that
/// matters before pointing one at four hundred files.
pub fn mode_words(mode: TemplateMode) -> &'static str {
    match mode {
        TemplateMode::Merge => "fill empty fields only",
        TemplateMode::Replace => "overwrite existing values",
        TemplateMode::Append => "add keywords, overwrite the rest",
    }
}

/// A template's own words, in panel order: what it names, not what a given file
/// would gain from it. Kept separate from [`plan`] because the panel shows this
/// before any file has been read.
pub fn describes(template: &MetadataTemplate) -> Vec<String> {
    let mut lines = Vec::new();
    for spec in EDITABLE_FIELDS {
        if let Some(value) = template.metadata.text(spec.id.namespace, spec.id.name) {
            lines.push(format!("{}: {value}", spec.label));
        }
    }
    if names(template, NS_XMP, "Rating") {
        lines.push(format!("Rating: {}★", template.metadata.rating().stars()));
    }
    if names(template, NS_PHOTOSHOP, "Label") {
        lines.push(format!(
            "Label: {}",
            template
                .metadata
                .color_label()
                .as_label_str()
                .unwrap_or("none")
        ));
    }
    if names(template, NS_BRIDGECRAFT, "Flag") {
        lines.push(format!("Flag: {:?}", template.metadata.flag()));
    }
    if names(template, NS_DC, "subject") {
        lines.push(format!(
            "Keywords: {}",
            bc_metadata::keywords::subjects(&template.metadata).join(" | ")
        ));
    }
    lines
}

/// The properties a template from this file's metadata would name: every
/// stampable field and tag property that actually holds a value.
///
/// Empty values are left out rather than stored as blanks, because a stored blank
/// is a property the template *names* — and a named property is one applying the
/// template will write. A capture that recorded "Description: (empty)" would
/// clear the description of every file it was later applied to, which is the
/// opposite of what "save these values as a template" means. The rating, label and
/// flag have the same shape by construction: the metadata model cannot tell
/// "no rating" from "rating not mentioned", so a template never clears a tag.
pub fn capture(
    name: &str,
    description: Option<String>,
    mode: TemplateMode,
    source: &Metadata,
) -> MetadataTemplate {
    let mut metadata = Metadata::new();
    // Only the fields the bulk editor would stamp: a template is by definition
    // applied to more than one file, and this application has already decided that
    // camera make, model, software and date taken do not belong in a many-file
    // write. Capturing them from a photograph would produce a template that stamps
    // "Canon EOS 5D" onto the Nikon files it is later pointed at.
    //
    // [`plan`] does not repeat the restriction. It honours whatever a template
    // names, because the stored document is the user's, and a hand-edited template
    // that does want to write a Make should be obeyed rather than second-guessed.
    for spec in EDITABLE_FIELDS.iter().filter(|spec| spec.bulk) {
        if let Some(value) = source.text(spec.id.namespace, spec.id.name) {
            if !value.trim().is_empty() {
                metadata.set_text(spec.id.namespace, spec.id.name, value.to_string());
            }
        }
    }
    if source.rating().stars() > 0 {
        metadata.set_rating(source.rating());
    }
    match source.color_label() {
        bc_metadata::ratings::ColorLabel::None => {}
        label => {
            if let Some(text) = label.as_label_str() {
                metadata.set_color_label(bc_metadata::ratings::ColorLabel::from_label_str(text));
            }
        }
    }
    if source.flag() != bc_metadata::ratings::Flag::None {
        metadata.set_flag(source.flag());
    }
    // The keyword list is copied as a list. Reusing the merge helper here would
    // be tempting and wrong: it copies *every* property the source carries, and a
    // source read off a real file carries camera make, exposure time and a
    // software tag — so a "template" captured from a photograph would overwrite
    // those fields on every file it was later applied to. A template holds what
    // the panel shows the user, field by field.
    let subjects = bc_metadata::keywords::subjects(source);
    if !subjects.is_empty() {
        bc_metadata::keywords::set_subjects(&mut metadata, subjects);
    }
    let mut template = MetadataTemplate::new(name, metadata).with_mode(mode);
    template.description = description.filter(|text| !text.trim().is_empty());
    template
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_metadata::{keywords, ratings::Flag, Metadata, NS_EXIF};

    fn meta(pairs: &[(&str, &str)]) -> Metadata {
        let mut out = Metadata::new();
        for (name, value) in pairs {
            out.set_text(NS_EXIF, *name, (*value).to_string());
        }
        out
    }

    #[test]
    fn a_template_names_only_the_fields_that_hold_something() {
        // Capture is where a template's reach is decided. A blank carried in
        // would be a property the template names, and a named property is one
        // applying it writes — so capturing from a file with no copyright would
        // clear the copyright of everything it later touched.
        let source = meta(&[("Artist", "J. Rivera"), ("Copyright", "")]);
        let template = capture("shoot", None, TemplateMode::Replace, &source);
        assert_eq!(
            template
                .metadata
                .text(NS_EXIF, "Artist")
                .expect("artist lost"),
            "J. Rivera"
        );
        assert!(
            template.metadata.get(NS_EXIF, "Copyright").is_none(),
            "an empty copyright was captured as a name"
        );

        // And applying it to a file that has a copyright leaves that copyright
        // alone, in every mode: Replace governs the properties the template
        // mentions, not the ones it has never heard of.
        let target = meta(&[
            ("Copyright", "© Existing"),
            ("ImageDescription", "a description"),
        ]);
        for mode in [
            TemplateMode::Replace,
            TemplateMode::Merge,
            TemplateMode::Append,
        ] {
            let template = capture("shoot", None, mode, &source);
            let planned = plan(&target, &template);
            assert!(
                planned
                    .changes
                    .iter()
                    .all(|change| change.id.name != "Copyright"
                        && change.id.name != "ImageDescription"),
                "{mode:?} planned a write to a field the template never named: {:?}",
                planned.changes
            );
            assert!(
                planned
                    .changes
                    .iter()
                    .any(|change| change.id.name == "Artist"),
                "{mode:?} did not write the field the template does name"
            );
        }
    }

    #[test]
    fn replace_writes_the_named_field_and_merge_leaves_a_filled_one_alone() {
        let template_metadata = meta(&[("Artist", "J. Rivera")]);
        let target = meta(&[("Artist", "Someone Else")]);

        let replacing =
            MetadataTemplate::new("t", template_metadata.clone()).with_mode(TemplateMode::Replace);
        let planned = plan(&target, &replacing);
        assert_eq!(planned.changes.len(), 1);
        assert_eq!(planned.changes[0].value, "J. Rivera");

        let merging =
            MetadataTemplate::new("t", template_metadata.clone()).with_mode(TemplateMode::Merge);
        let planned = plan(&target, &merging);
        assert!(
            planned.changes.is_empty(),
            "merge overwrote a field that already had a value: {:?}",
            planned.changes
        );

        // Merge is not "do nothing": on a file that lacks the field it writes.
        let planned = plan(&meta(&[]), &merging);
        assert_eq!(planned.changes.len(), 1, "merge skipped an empty field");
    }

    #[test]
    fn keywords_follow_the_mode_and_are_compared_as_whole_paths() {
        let mut template_metadata = Metadata::new();
        keywords::add_keyword(&mut template_metadata, "Clients|ACME");
        keywords::add_keyword(&mut template_metadata, "Holiday");

        let mut target = meta(&[("Artist", "x")]);
        keywords::add_keyword(&mut target, "Holiday");
        keywords::add_keyword(&mut target, "Clients");

        // Append: the union, and nothing removed. "Clients" stays, because it is
        // a different keyword from "Clients|ACME" and not a parent to be folded.
        let appending =
            MetadataTemplate::new("t", template_metadata.clone()).with_mode(TemplateMode::Append);
        let planned = plan(&target, &appending);
        assert_eq!(
            planned.stamp.add_keywords,
            vec!["Clients|ACME".to_string()],
            "append added the wrong keywords"
        );
        assert!(
            planned.stamp.remove_keywords.is_empty(),
            "append removed keywords: {:?}",
            planned.stamp.remove_keywords
        );

        // Replace: the file ends up holding exactly the template's set. That is
        // *not* "remove everything then add everything" — "Holiday" is in both, so
        // it is neither added nor removed, and a plan that said otherwise would
        // rewrite a keyword list that was already right.
        let replacing =
            MetadataTemplate::new("t", template_metadata.clone()).with_mode(TemplateMode::Replace);
        let planned = plan(&target, &replacing);
        assert_eq!(
            planned.stamp.remove_keywords,
            vec!["Clients".to_string()],
            "replace removed more than the difference"
        );
        assert_eq!(
            planned.stamp.add_keywords,
            vec!["Clients|ACME".to_string()],
            "replace added back a keyword the file already had"
        );

        // Merge on a file that already has keywords: nothing to fill.
        let merging =
            MetadataTemplate::new("t", template_metadata.clone()).with_mode(TemplateMode::Merge);
        let planned = plan(&target, &merging);
        assert!(
            planned.stamp.add_keywords.is_empty() && planned.stamp.remove_keywords.is_empty(),
            "merge wrote keywords onto a file that had some: {:?}",
            planned.stamp
        );

        // Merge on a file with none: the whole set arrives.
        let planned = plan(&meta(&[("Artist", "x")]), &merging);
        assert_eq!(planned.stamp.add_keywords.len(), 2);
    }

    #[test]
    fn a_file_that_already_matches_needs_no_write() {
        // The empty plan is the one that protects files: the tag writer creates a
        // sidecar where none exists, so a "no change" apply that still called it
        // would leave 200 `.xmp` files behind for 200 files that were already
        // correct.
        let mut template_metadata = meta(&[("Artist", "J. Rivera")]);
        keywords::add_keyword(&mut template_metadata, "Holiday");
        let template =
            MetadataTemplate::new("t", template_metadata.clone()).with_mode(TemplateMode::Replace);

        let mut target = meta(&[("Artist", "J. Rivera")]);
        keywords::add_keyword(&mut target, "Holiday");
        let planned = plan(&target, &template);
        assert!(
            planned.is_empty(),
            "an identical file was planned a write: {planned:?}"
        );
        assert_eq!(planned.property_count(), 0);

        // A file one property away from matching gets exactly that property.
        let planned = plan(&meta(&[("Artist", "J. Rivera")]), &template);
        assert_eq!(
            planned.stamp.add_keywords,
            vec!["Holiday".to_string()],
            "the differing property was missed"
        );
        assert_eq!(planned.property_count(), 1);
    }

    #[test]
    fn a_template_never_clears_a_tag_it_does_not_name() {
        // The model has no way to record "explicitly unrated" — setting zero stars
        // removes the property — so a captured template cannot mention the rating
        // of a file with none, and applying it must leave a starred file starred.
        let source = meta(&[("Artist", "J. Rivera")]);
        let template = capture("shoot", None, TemplateMode::Replace, &source);

        let mut target = meta(&[("Artist", "other")]);
        target.set_rating(bc_metadata::ratings::Rating::new(5));
        target.set_flag(Flag::Approved);
        let planned = plan(&target, &template);
        assert!(
            planned.stamp.stars.is_none(),
            "the template planned a rating"
        );
        assert!(planned.stamp.flag.is_none(), "the template planned a flag");
        assert_eq!(planned.changes.len(), 1);
    }

    #[test]
    fn capture_skips_the_fields_the_bulk_editor_refuses_to_stamp() {
        // A JPEG read off disk carries its camera. If capture took every editable
        // field, the template would name `Make` and stamp that camera onto every
        // file it was later applied to — including files from a different camera,
        // where it is simply a lie about the photograph.
        let source = meta(&[
            ("Artist", "J. Rivera"),
            ("Make", "Canon"),
            ("Model", "EOS 5D"),
            ("Software", "BridgeCraft"),
            ("DateTimeOriginal", "2026:01:02 03:04:05"),
        ]);
        let template = capture("shoot", None, TemplateMode::Replace, &source);
        assert_eq!(
            template
                .metadata
                .text(NS_EXIF, "Artist")
                .expect("the stampable field was lost"),
            "J. Rivera"
        );
        for name in ["Make", "Model", "Software", "DateTimeOriginal"] {
            assert!(
                template.metadata.get(NS_EXIF, name).is_none(),
                "capture kept {name}, which the bulk editor will not stamp"
            );
        }
        // And because the template does not name them, applying it cannot write
        // them: the plan is built over the template's own properties.
        let mut target = meta(&[("Make", "Nikon")]);
        target.set_text(NS_EXIF, "Model", "Z6");
        let planned = plan(&target, &template);
        assert!(
            planned
                .changes
                .iter()
                .all(|change| !matches!(change.id.name, "Make" | "Model" | "Software")),
            "a template wrote camera data it never named: {:?}",
            planned.changes
        );
    }

    #[test]
    fn a_sidecar_selected_with_its_asset_is_its_own_target() {
        // Not a design choice, a fact worth pinning: `sidecar_path` is
        // `with_extension("xmp")`, so a `.xmp` in a selection maps to itself. The
        // panel lists sidecars as files, which means "apply to everything selected"
        // can write into a metadata document as though it were an asset. The bulk
        // tests avoid it by selecting images by name; the behaviour itself is
        // recorded in Tech debt as a listing-policy question, not papered over
        // here.
        let sidecar = std::path::Path::new("/lib/a.xmp");
        assert_eq!(
            bc_metadata::xmp::sidecar_path(sidecar),
            sidecar.to_path_buf()
        );
        assert_eq!(
            bc_metadata::xmp::sidecar_path(std::path::Path::new("/lib/a.png")),
            std::path::PathBuf::from("/lib/a.xmp")
        );
    }

    #[test]
    fn the_captured_mode_is_the_mode_that_gets_applied() {
        // A template is its name, its values *and* its mode: silently reapplying a
        // "merge" template as a "replace" would overwrite filled fields.
        let source = meta(&[("Artist", "J. Rivera")]);
        for mode in [
            TemplateMode::Replace,
            TemplateMode::Merge,
            TemplateMode::Append,
        ] {
            let template = capture("shoot", Some("notes".to_string()), mode, &source);
            assert_eq!(template.mode, mode);
            assert_eq!(template.description.as_deref(), Some("notes"));
            assert_eq!(template.name, "shoot");
        }
        // A blank description is left out rather than stored as an empty string:
        // the field is optional, and a stored "" reads as a value someone typed.
        let template = capture(
            "shoot",
            Some("   ".to_string()),
            TemplateMode::Merge,
            &source,
        );
        assert_eq!(template.description, None);
    }
}
