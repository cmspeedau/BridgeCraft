//! Token-based batch rename engine (issue #14).
//!
//! The engine is a pure planner plus an apply/undo layer on top of
//! [`crate::file_ops::rename_path`]. Patterns use a small, explicit token
//! grammar:
//!
//! - `{name}` — current base name (filename without extension)
//! - `{name:upper}` / `{name:lower}` — case transforms; also valid on `{ext}`
//! - `{date:FORMAT}` — file mtime formatted with strftime-style specifiers
//!   (`%Y`, `%m`, `%d`, `%H`, `%M`, `%S`; `%%` is a literal percent)
//! - `{seq:START>WIDTH}` — counter, e.g. `{seq:1>3}` → `001`, `002`, …
//! - `{ext}` — extension without the dot, transformed per its own case rules
//! - `{keyword#N}` — N-th metadata keyword (1-based); empty when absent
//! - literal text is copied through; `{{` and `}}` escape braces
//!
//! Extension handling: the extension is never part of `{name}` and is
//! appended to every planned name so a rename cannot silently change a
//! file's container format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::file_ops::rename_path;
use crate::{CoreError, Result};

/// One file in a rename batch, with the fields tokens may consume.
#[derive(Debug, Clone)]
pub struct RenameInput {
    pub path: PathBuf,
    /// Metadata keywords for `{keyword#n}` (1-based); empty when none.
    pub keywords: Vec<String>,
}

impl RenameInput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            keywords: Vec::new(),
        }
    }
}

/// A single planned old→new rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePlanEntry {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// The full plan: entries plus the errors that made some inputs unplannable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RenamePlan {
    pub entries: Vec<RenamePlanEntry>,
    pub errors: Vec<(PathBuf, CoreError)>,
}

impl RenamePlan {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.errors.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    Name,
    NameCase(Case),
    Extension,
    ExtCase(Case),
    Date(String),
    Sequence { start: u64, width: u32 },
    Keyword(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
}

impl Case {
    fn apply(self, text: &str) -> String {
        match self {
            Case::Upper => text.to_uppercase(),
            Case::Lower => text.to_lowercase(),
        }
    }
}

fn parse_case(spec: &str) -> Option<Case> {
    match spec {
        "upper" => Some(Case::Upper),
        "lower" => Some(Case::Lower),
        _ => None,
    }
}

fn parse_error(pattern: &str, message: &str) -> CoreError {
    CoreError::InvalidOperation(format!("invalid rename pattern {pattern:?}: {message}"))
}

/// Tokenize a pattern. Pattern errors are global (the whole pattern is
/// rejected) so a bad template never produces a half-parsed plan.
fn tokenize(pattern: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            literal.push(c);
            continue;
        }
        match chars.next() {
            Some('{') => literal.push('{'),
            Some(first) => {
                let mut inner_text = String::new();
                let mut closed = false;
                while let Some(&cc) = chars.peek() {
                    chars.next();
                    if cc == '}' {
                        closed = true;
                        break;
                    }
                    inner_text.push(cc);
                }
                if !closed {
                    return Err(parse_error(pattern, "unterminated token (missing '}')"));
                }
                if !literal.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut literal)));
                }
                tokens.push(parse_token(pattern, first, &inner_text)?);
            }
            None => return Err(parse_error(pattern, "unterminated token (missing '}')")),
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    Ok(tokens)
}

fn parse_token(pattern: &str, first: char, rest: &str) -> Result<Token> {
    let body = format!("{first}{rest}");
    let (name, spec) = match body.split_once(':') {
        Some((n, s)) => (n, Some(s)),
        None => (body.as_str(), None),
    };
    match name {
        "name" => match spec {
            None => Ok(Token::Name),
            Some(s) => parse_case(s)
                .map(Token::NameCase)
                .ok_or_else(|| parse_error(pattern, "unknown {name:…} transform")),
        },
        "ext" => match spec {
            None => Ok(Token::Extension),
            Some(s) => parse_case(s)
                .map(Token::ExtCase)
                .ok_or_else(|| parse_error(pattern, "unknown {ext:…} transform")),
        },
        "date" => {
            let format = spec.ok_or_else(|| parse_error(pattern, "{date} requires a format"))?;
            if format.is_empty() {
                return Err(parse_error(pattern, "{date} format must not be empty"));
            }
            Ok(Token::Date(format.to_string()))
        }
        "seq" => {
            let spec = spec.ok_or_else(|| parse_error(pattern, "{seq} requires START>WIDTH"))?;
            let (start, width) = spec
                .split_once('>')
                .ok_or_else(|| parse_error(pattern, "{seq} must look like START>WIDTH, e.g. 1>3"))?;
            let start: u64 = start
                .parse()
                .map_err(|_| parse_error(pattern, "{seq} start must be a non-negative integer"))?;
            let width: u32 = width
                .parse()
                .map_err(|_| parse_error(pattern, "{seq} width must be a non-negative integer"))?;
            if width > 20 {
                return Err(parse_error(pattern, "{seq} width must be 20 or less"));
            }
            Ok(Token::Sequence { start, width })
        }
        _ if body.starts_with("keyword#") => {
            let index: usize = body["keyword#".len()..]
                .parse()
                .map_err(|_| parse_error(pattern, "{keyword#N} requires a 1-based index"))?;
            if index < 1 {
                return Err(parse_error(pattern, "{keyword#N} index starts at 1"));
            }
            Ok(Token::Keyword(index))
        }
        _ => Err(parse_error(pattern, "unknown token")),
    }
}

/// Format an mtime with the supported strftime-style subset.
fn format_date(mtime: std::time::SystemTime, format: &str) -> Result<String> {
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| CoreError::InvalidOperation(format!("file mtime is before 1970: {e}")))?
        .as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-calendar day computation (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('H') => out.push_str(&format!("{h:02}")),
            Some('M') => out.push_str(&format!("{mi:02}")),
            Some('S') => out.push_str(&format!("{s:02}")),
            Some('%') => out.push('%'),
            Some(other) => {
                return Err(CoreError::InvalidOperation(format!(
                    "unsupported date specifier %{other}"
                )))
            }
            None => {
                return Err(CoreError::InvalidOperation(
                    "dangling '%' in date format".to_string(),
                ))
            }
        }
    }
    Ok(out)
}

fn render(tokens: &[Token], input: &RenameInput, seq: &mut BTreeMap<String, u64>) -> Result<String> {
    let path = &input.path;
    let base = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| {
            CoreError::InvalidOperation(format!("path has no file name: {}", path.display()))
        })?;
    // The extension is kept separate from the base name everywhere so a
    // rename cannot silently change a file's container format.
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mtime = std::fs::metadata(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CoreError::PathNotFound(path.display().to_string())
            } else {
                CoreError::Io(e)
            }
        })?
        .modified()
        .map_err(CoreError::Io)?;
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Literal(text) => out.push_str(text),
            Token::Name => out.push_str(&base),
            Token::NameCase(kase) => out.push_str(&kase.apply(&base)),
            Token::Extension => out.push_str(&ext),
            Token::ExtCase(kase) => out.push_str(&kase.apply(&ext)),
            Token::Date(format) => out.push_str(&format_date(mtime, format)?),
            Token::Sequence { start, width } => {
                let key = format!("seq:{start}>{width}");
                let value = seq.entry(key).or_insert(*start);
                out.push_str(&format!("{:0width$}", value, width = *width as usize));
                // Saturating increment: oversized batches keep producing the
                // max value instead of wrapping or panicking.
                *value = value.saturating_add(1);
            }
            Token::Keyword(n) => {
                if let Some(keyword) = input.keywords.get(n.saturating_sub(1)) {
                    out.push_str(keyword);
                }
            }
        }
    }
    Ok(out)
}

/// Compute the full old→new plan for a pattern. Never touches the filesystem
/// beyond stat'ing sources for `{date}`; per-file problems are collected in
/// [`RenamePlan::errors`].
pub fn build_plan(pattern: &str, inputs: &[RenameInput]) -> Result<RenamePlan> {
    let tokens = tokenize(pattern)?;
    let mut plan = RenamePlan::default();
    let mut seq = BTreeMap::new();
    for input in inputs {
        let rendered = match render(&tokens, input, &mut seq) {
            Ok(rendered) => rendered,
            Err(error) => {
                plan.errors.push((input.path.clone(), error));
                continue;
            }
        };
        if rendered.is_empty() {
            plan.errors.push((
                input.path.clone(),
                CoreError::InvalidOperation("pattern produced an empty name".to_string()),
            ));
            continue;
        }
        if rendered.contains('/') || rendered.contains('\\') || rendered == ".." || rendered == "."
        {
            plan.errors.push((
                input.path.clone(),
                CoreError::InvalidOperation(format!(
                    "pattern produced a non-name value: {rendered:?}"
                )),
            ));
            continue;
        }
        let parent = input
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // The extension is always appended unchanged unless the pattern
        // itself renders it, so a rename cannot silently change containers.
        let mut new_name = rendered;
        if !new_name
            .ends_with(&format!(".{ext}"))
            || new_name.len() <= ext.len() + 1
        {
            // Only append when the rendered text does not already end with
            // the original extension (e.g. a literal `{ext}` in the pattern).
            if !ext.is_empty() && !new_name.ends_with(&format!(".{ext}")) {
                new_name.push('.');
                new_name.push_str(&ext);
            }
        }
        plan.entries.push(RenamePlanEntry {
            from: input.path.clone(),
            to: parent.join(new_name),
        });
    }
    Ok(plan)
}

fn case_folded(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Reject plans whose targets collide (case-insensitively, so Windows is safe
/// too) or that would rename a file across directories.
pub fn validate_plan(plan: &RenamePlan) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for entry in &plan.entries {
        if entry.from.parent() != entry.to.parent() {
            return Err(CoreError::InvalidOperation(format!(
                "rename would cross directories: {} -> {}",
                entry.from.display(),
                entry.to.display()
            )));
        }
        if !seen.insert(case_folded(&entry.to)) {
            return Err(CoreError::PathAlreadyExists(format!(
                "two renames would produce the same target (case-insensitively): {}",
                entry.to.display()
            )));
        }
    }
    Ok(())
}

/// A journaled rename operation: what was applied and what to undo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameUndo {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub applied: Vec<RenameUndo>,
    pub errors: Vec<(PathBuf, CoreError)>,
}

impl ApplyReport {
    /// Undo the applied renames in reverse order. Per-file failures are
    /// collected; the undo keeps going so as much as possible is restored.
    pub fn undo(&self) -> Vec<(PathBuf, CoreError)> {
        let mut failures = Vec::new();
        for entry in self.applied.iter().rev() {
            let name = entry
                .old_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Err(error) = rename_path(&entry.new_path, &name) {
                failures.push((entry.new_path.clone(), error));
            }
        }
        failures
    }
}

/// Apply a plan. Existing targets are per-item failures (`rename_path` never
/// replaces). Returns a journal for undo; the journal is `Serialize` so the
/// caller can persist it (e.g. JSON sidecar under the app cache dir) and undo
/// across a process restart.
pub fn apply(plan: &RenamePlan) -> Result<ApplyReport> {
    validate_plan(plan)?;
    let mut report = ApplyReport::default();
    for entry in &plan.entries {
        let name = entry
            .to
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match rename_path(&entry.from, &name) {
            Ok(()) => report.applied.push(RenameUndo {
                old_path: entry.from.clone(),
                new_path: entry.to.clone(),
            }),
            Err(error) => report.errors.push((entry.from.clone(), error)),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_inputs(dir: &Path, names: &[&str]) -> Vec<RenameInput> {
        names
            .iter()
            .map(|n| {
                let path = dir.join(n);
                std::fs::write(&path, b"x").unwrap();
                RenameInput::new(path)
            })
            .collect()
    }

    #[test]
    fn tokenizes_literals_and_escapes() {
        let tokens = tokenize("a{{b}}{name}c").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Literal("a{b}".to_string()),
                Token::Name,
                Token::Literal("c".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_unterminated_and_unknown_tokens() {
        assert!(tokenize("{name").is_err());
        assert!(tokenize("{bogus}").is_err());
        assert!(tokenize("{name:shout}").is_err());
        assert!(tokenize("{seq:1-3}").is_err());
        assert!(tokenize("{keyword#0}").is_err());
    }

    #[test]
    fn renders_name_ext_case_and_literals() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Photo.JPG"), b"x").unwrap();
        let plan = build_plan(
            "{name:lower}_v2.{ext:lower}",
            &[RenameInput::new(dir.path().join("Photo.JPG"))],
        )
        .unwrap();
        assert_eq!(plan.errors.len(), 0);
        assert_eq!(
            plan.entries[0].to.file_name().unwrap().to_string_lossy(),
            "photo_v2.jpg"
        );
    }

    #[test]
    fn sequence_counts_and_pads() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = temp_inputs(dir.path(), &["a.txt", "b.txt", "c.txt"]);
        let plan = build_plan("img_{seq:7>3}", &inputs).unwrap();
        let names: Vec<_> = plan
            .entries
            .iter()
            .map(|e| e.to.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["img_007.txt", "img_008.txt", "img_009.txt"]);
    }

    #[test]
    fn sequence_overflow_saturates_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = temp_inputs(dir.path(), &["a.txt", "b.txt"]);
        let plan = build_plan("n_{seq:18446744073709551615>20}", &inputs).unwrap();
        assert_eq!(plan.errors.len(), 0);
        assert_eq!(
            plan.entries[1].to.file_name().unwrap().to_string_lossy(),
            format!("n_{:020}.txt", u64::MAX)
        );
    }

    #[test]
    fn keyword_token_uses_metadata_and_defaults_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = RenameInput::new(dir.path().join("file.txt"));
        input.keywords = vec!["travel".into(), "iceland".into()];
        let plan = build_plan("{keyword#2}_{keyword#5}", &[input]).unwrap();
        assert_eq!(
            plan.entries[0].to.file_name().unwrap().to_string_lossy(),
            "iceland_.txt"
        );
    }

    #[test]
    fn date_token_formats_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"x").unwrap();
        let plan = build_plan("{date:%Y%m%d}", &[RenameInput::new(&path)]).unwrap();
        assert_eq!(plan.errors.len(), 0);
        let name = plan.entries[0].to.file_name().unwrap().to_string_lossy();
        assert_eq!(name.len(), 12); // 8 date digits + ".txt"
        assert!(name.chars().take(8).all(|c| c.is_ascii_digit()), "{name}");
        assert!(build_plan("{date:%Q}", &[RenameInput::new(&path)]).is_err());
    }

    #[test]
    fn extension_is_preserved_when_pattern_has_no_ext_token() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        let plan = build_plan("{name}", &[RenameInput::new(dir.path().join("a.jpg"))]).unwrap();
        assert_eq!(
            plan.entries[0].to.file_name().unwrap().to_string_lossy(),
            "a.jpg"
        );
    }

    #[test]
    fn collision_in_plan_is_rejected_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = temp_inputs(dir.path(), &["a.txt", "b.txt"]);
        let plan = build_plan("Same", &inputs).unwrap();
        // Force both targets to fold to the same name apart from case.
        let mut entries = plan.entries;
        entries[1].to = entries[0].to.with_file_name("same.txt");
        let plan = RenamePlan {
            entries,
            errors: Vec::new(),
        };
        assert!(matches!(
            validate_plan(&plan),
            Err(CoreError::PathAlreadyExists(_))
        ));
    }

    #[test]
    fn directory_crossing_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let plan = RenamePlan {
            entries: vec![RenamePlanEntry {
                from: dir.path().join("a.txt"),
                to: dir.path().join("sub/a.txt"),
            }],
            errors: Vec::new(),
        };
        assert!(matches!(
            validate_plan(&plan),
            Err(CoreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn apply_and_undo_round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = temp_inputs(dir.path(), &["one.txt", "two.txt"]);
        let plan = build_plan("renamed_{seq:1>2}", &inputs).unwrap();
        let report = apply(&plan).unwrap();
        assert_eq!(report.errors.len(), 0);
        assert_eq!(report.applied.len(), 2);
        assert!(dir.path().join("renamed_01.txt").exists());
        assert!(dir.path().join("renamed_02.txt").exists());
        assert!(!dir.path().join("one.txt").exists());

        // The journal serialises so undo can work across a process restart.
        let json = serde_json::to_string(&report).unwrap();
        let restored: ApplyReport = serde_json::from_str(&json).unwrap();

        let failures = restored.undo();
        assert!(failures.is_empty(), "{failures:?}");
        assert!(dir.path().join("one.txt").exists());
        assert!(dir.path().join("two.txt").exists());
        assert!(!dir.path().join("renamed_01.txt").exists());
    }

    #[test]
    fn apply_reports_existing_targets_without_touching_others() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = temp_inputs(dir.path(), &["a.txt", "b.txt"]);
        std::fs::write(dir.path().join("target.txt"), b"occupied").unwrap();
        let plan = build_plan("target", &inputs).unwrap();
        let report = apply(&plan).unwrap();
        assert_eq!(report.errors.len(), 2);
        assert!(dir.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read(dir.path().join("target.txt")).unwrap(),
            b"occupied"
        );
    }

    #[test]
    fn unicode_names_survive_planning() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("photographie ☕.png");
        std::fs::write(&original, b"x").unwrap();
        let plan = build_plan(
            "[{name}]{seq:1>1}",
            &[RenameInput::new(original.clone())],
        )
        .unwrap();
        assert_eq!(plan.errors.len(), 0);
        assert_eq!(
            plan.entries[0].to.file_name().unwrap().to_string_lossy(),
            "[photographie ☕]1.png"
        );
    }
}
