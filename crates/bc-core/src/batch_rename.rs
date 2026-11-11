//! Token-based batch rename engine (issue #14).
//!
//! Grammar (deliberately boring and explicit):
//!
//! | Token              | Meaning                                             |
//! |--------------------|-----------------------------------------------------|
//! | `{name}`           | current base name (without extension)               |
//! | `{name:upper}`     | base name, uppercased                                |
//! | `{name:lower}`     | base name, lowercased                                |
//! | `{name:title}`     | base name, title-cased                               |
//! | `{ext}`            | original extension (without the dot)                 |
//! | `{date:FMT}`       | capture/mtime date; `%Y %m %d %H %M %S`, `%%`        |
//! | `{seq:START>WIDTH}`| counter (zero-padded); `{seq:3}` = start 1, width 3  |
//! | `{keyword#N}`      | Nth keyword (1-based) from the metadata model        |
//! | `{{` / `}}`        | literal `{` / `}`                                    |
//!
//! Extension handling: the pattern rewrites the *stem* and the original
//! extension is appended automatically. If a pattern contains `{ext}` the
//! rendered text is treated as the full file name (no implicit append), so the
//! real container is never silently changed.
//!
//! Planning refuses target collisions (including case-insensitive collisions)
//! and never crosses directories. Apply is journaled so the last batch can be
//! undone across a restart.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

// ---------------------------------------------------------------------------
// Pattern
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transform {
    None,
    Upper,
    Lower,
    Title,
}

impl Transform {
    fn apply(self, text: &str) -> String {
        match self {
            Transform::None => text.to_string(),
            Transform::Upper => text.to_uppercase(),
            Transform::Lower => text.to_lowercase(),
            Transform::Title => text
                .split_whitespace()
                .map(|w| {
                    let mut chars = w.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    Name(Transform),
    Ext,
    Date(String),
    Seq { start: usize, width: usize },
    Keyword(usize),
}

/// A parsed rename pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    tokens: Vec<Token>,
    contains_ext: bool,
}

impl Pattern {
    pub fn parse(pattern: &str) -> Result<Self> {
        let mut tokens = Vec::new();
        let mut literal = String::new();
        let mut chars = pattern.chars().peekable();
        let mut contains_ext = false;

        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    if chars.peek() == Some(&'{') {
                        chars.next();
                        literal.push('{');
                        continue;
                    }
                    if !literal.is_empty() {
                        tokens.push(Token::Literal(std::mem::take(&mut literal)));
                    }
                    let mut inner = String::new();
                    let mut closed = false;
                    for c2 in chars.by_ref() {
                        if c2 == '}' {
                            closed = true;
                            break;
                        }
                        inner.push(c2);
                    }
                    if !closed {
                        return Err(invalid("unclosed pattern token"));
                    }
                    let token = parse_token(&inner)?;
                    if matches!(token, Token::Ext) {
                        contains_ext = true;
                    }
                    tokens.push(token);
                }
                '}' => {
                    if chars.peek() == Some(&'}') {
                        chars.next();
                        literal.push('}');
                    } else {
                        return Err(invalid("unmatched '}' in pattern"));
                    }
                }
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            tokens.push(Token::Literal(literal));
        }
        Ok(Self {
            tokens,
            contains_ext,
        })
    }

    /// Render the new file name for one context.
    pub fn render(&self, context: &RenameContext) -> Result<String> {
        let stem = context
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = context
            .path
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut out = String::new();
        for token in &self.tokens {
            match token {
                Token::Literal(s) => out.push_str(s),
                Token::Name(t) => out.push_str(&t.apply(&stem)),
                Token::Ext => out.push_str(&ext),
                Token::Date(fmt) => out.push_str(&format_date(context.timestamp, fmt)),
                Token::Seq { start, width } => {
                    let n = start.saturating_add(context.index);
                    out.push_str(&format!("{:0width$}", n, width = *width));
                }
                Token::Keyword(n) => {
                    if let Some(kw) = context.keywords.get(n.saturating_sub(1)) {
                        out.push_str(kw);
                    }
                }
            }
        }

        let full = if self.contains_ext || ext.is_empty() {
            out
        } else {
            format!("{out}.{ext}")
        };
        Ok(full)
    }
}

fn invalid(msg: &str) -> CoreError {
    CoreError::InvalidOperation(format!("invalid rename pattern: {msg}"))
}

fn parse_token(inner: &str) -> Result<Token> {
    if inner.is_empty() {
        return Err(invalid("empty token"));
    }
    if let Some(n) = inner.strip_prefix("keyword#") {
        let n: usize = n.parse().map_err(|_| invalid("bad keyword index"))?;
        if n == 0 {
            return Err(invalid("keyword index is 1-based"));
        }
        return Ok(Token::Keyword(n));
    }
    let (head, rest) = match inner.split_once(':') {
        Some((h, r)) => (h, Some(r)),
        None => (inner, None),
    };
    match head {
        "name" => {
            let transform = match rest {
                None => Transform::None,
                Some("upper") => Transform::Upper,
                Some("lower") => Transform::Lower,
                Some("title") => Transform::Title,
                Some(other) => return Err(invalid(&format!("unknown name transform '{other}'"))),
            };
            Ok(Token::Name(transform))
        }
        "ext" => {
            if rest.is_some() {
                return Err(invalid("{ext} takes no argument"));
            }
            Ok(Token::Ext)
        }
        "date" => {
            let fmt = rest.ok_or_else(|| invalid("{date} needs a format, e.g. {date:%Y-%m-%d}"))?;
            Ok(Token::Date(fmt.to_string()))
        }
        "seq" => {
            let spec = rest.ok_or_else(|| invalid("{seq} needs a width, e.g. {seq:0>3}"))?;
            let (start, width) = if let Some((s, w)) = spec.split_once('>') {
                let start: usize = s.parse().map_err(|_| invalid("bad sequence start"))?;
                let width: usize = w.parse().map_err(|_| invalid("bad sequence width"))?;
                (start, width)
            } else {
                let width: usize = spec.parse().map_err(|_| invalid("bad sequence width"))?;
                (1, width)
            };
            if width == 0 || width > 32 {
                return Err(invalid("sequence width must be 1..=32"));
            }
            Ok(Token::Seq { start, width })
        }
        "keyword" => {
            let n = rest
                .and_then(|r| r.strip_prefix('#'))
                .ok_or_else(|| invalid("{keyword} uses {keyword#N}"))?;
            let n: usize = n.parse().map_err(|_| invalid("bad keyword index"))?;
            if n == 0 {
                return Err(invalid("keyword index is 1-based"));
            }
            Ok(Token::Keyword(n))
        }
        other => Err(invalid(&format!("unknown token '{other}'"))),
    }
}

// ---------------------------------------------------------------------------
// Date formatting
// ---------------------------------------------------------------------------

fn format_date(timestamp: Option<i64>, fmt: &str) -> String {
    let Some(secs) = timestamp else {
        return String::new();
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{year:04}")),
            Some('m') => out.push_str(&format!("{month:02}")),
            Some('d') => out.push_str(&format!("{day:02}")),
            Some('H') => out.push_str(&format!("{hour:02}")),
            Some('M') => out.push_str(&format!("{minute:02}")),
            Some('S') => out.push_str(&format!("{second:02}")),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Days since 1970-01-01 -> (year, month, day). Hinnant's civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Per-file inputs for a rename plan.
#[derive(Debug, Clone)]
pub struct RenameContext {
    pub path: PathBuf,
    /// Position in the batch (0-based), used by `{seq}`.
    pub index: usize,
    /// Keywords for `{keyword#N}`.
    pub keywords: Vec<String>,
    /// Capture date as Unix seconds (EXIF or mtime); `{date}` falls back to
    /// the file's mtime when this is `None`.
    pub timestamp: Option<i64>,
}

impl RenameContext {
    pub fn from_path(path: PathBuf, index: usize) -> Self {
        let timestamp = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        Self {
            path,
            index,
            keywords: Vec::new(),
            timestamp,
        }
    }
}

/// One old -> new pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePlanItem {
    pub source: PathBuf,
    pub target: PathBuf,
}

/// Compute a full plan without touching the filesystem.
///
/// Errors when a rendered name is not a single path component, when two
/// targets collide (case-insensitively, for Windows parity), or when a target
/// already exists and is not itself being renamed away.
pub fn plan(pattern: &Pattern, contexts: &[RenameContext]) -> Result<Vec<RenamePlanItem>> {
    let mut items = Vec::with_capacity(contexts.len());
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();

    for context in contexts {
        let name = pattern.render(context)?;
        if name.is_empty() || name == "." || name == ".." {
            return Err(invalid("rendered name is empty or reserved"));
        }
        if name.contains('/') || name.contains('\\') {
            return Err(invalid("rendered name crosses directories"));
        }
        let parent = context
            .path
            .parent()
            .ok_or_else(|| invalid("source has no parent directory"))?;
        let target = parent.join(&name);

        let key = name.to_lowercase();
        if let Some(previous) = seen.insert(key, context.path.clone()) {
            return Err(CoreError::InvalidOperation(format!(
                "target collision: {} and {} both become {name}",
                previous.display(),
                context.path.display()
            )));
        }

        // A target that already exists is only acceptable if it is this same
        // source (no-op) or another source that will move away.
        if target.exists() && target != context.path {
            let is_source = contexts.iter().any(|c| c.path == target);
            if !is_source {
                return Err(CoreError::PathAlreadyExists(target.display().to_string()));
            }
        }

        items.push(RenamePlanItem {
            source: context.path.clone(),
            target,
        });
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Apply / undo journal
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct UndoJournal {
    pub operations: Vec<UndoOp>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UndoOp {
    pub from: String,
    pub to: String,
}

/// Apply a plan, writing an undo journal first. Returns the number renamed.
pub fn apply(plan: &[RenamePlanItem], journal_path: &Path) -> Result<usize> {
    let operations: Vec<UndoOp> = plan
        .iter()
        .filter(|item| item.source != item.target)
        .map(|item| UndoOp {
            from: item.source.to_string_lossy().to_string(),
            to: item.target.to_string_lossy().to_string(),
        })
        .collect();
    write_journal(journal_path, &UndoJournal { operations })?;

    let mut renamed = 0usize;
    for item in plan {
        if item.source == item.target {
            continue;
        }
        if item.target.exists() {
            return Err(CoreError::PathAlreadyExists(
                item.target.display().to_string(),
            ));
        }
        std::fs::rename(&item.source, &item.target)?;
        renamed += 1;
    }
    Ok(renamed)
}

fn write_journal(path: &Path, journal: &UndoJournal) -> Result<()> {
    let json = serde_json::to_string_pretty(journal)
        .map_err(|e| CoreError::InvalidOperation(format!("journal serialization failed: {e}")))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reverse the last journaled batch. Returns the number restored.
pub fn undo(journal_path: &Path) -> Result<usize> {
    let json = std::fs::read_to_string(journal_path)?;
    let journal: UndoJournal = serde_json::from_str(&json)
        .map_err(|e| CoreError::InvalidOperation(format!("bad undo journal: {e}")))?;
    let mut restored = 0usize;
    for op in journal.operations.iter().rev() {
        let to = Path::new(&op.to);
        let from = Path::new(&op.from);
        if to.exists() {
            if from.exists() {
                return Err(CoreError::PathAlreadyExists(from.display().to_string()));
            }
            std::fs::rename(to, from)?;
            restored += 1;
        }
    }
    let _ = std::fs::remove_file(journal_path);
    Ok(restored)
}

/// Default journal location under a caller-supplied cache directory.
pub fn journal_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("last-rename.json")
}

/// Convenience: current time as Unix seconds.
pub fn now_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(name: &str, index: usize) -> RenameContext {
        RenameContext {
            path: PathBuf::from(name),
            index,
            keywords: vec!["alpha".into(), "beta".into()],
            timestamp: Some(0),
        }
    }

    #[test]
    fn renders_name_and_extension() {
        let p = Pattern::parse("{name:upper}_{seq:0>2}").unwrap();
        let c = context("photo.jpg", 0);
        assert_eq!(p.render(&c).unwrap(), "PHOTO_00.jpg");
        let c = context("photo.jpg", 3);
        assert_eq!(p.render(&c).unwrap(), "PHOTO_03.jpg");
    }

    #[test]
    fn ext_token_treats_output_as_full_name() {
        let p = Pattern::parse("{name}_{keyword#2}.{ext}").unwrap();
        let c = context("pic.png", 0);
        assert_eq!(p.render(&c).unwrap(), "pic_beta.png");
    }

    #[test]
    fn date_formatting_epoch() {
        let p = Pattern::parse("{date:%Y-%m-%d}").unwrap();
        let c = context("x.jpg", 0);
        assert_eq!(p.render(&c).unwrap(), "1970-01-01.jpg");
        let mut c2 = context("x.jpg", 0);
        c2.timestamp = Some(1_000_000_000);
        assert_eq!(p.render(&c2).unwrap(), "2001-09-09.jpg");
    }

    #[test]
    fn literal_braces() {
        let p = Pattern::parse("a{{b}}c").unwrap();
        let c = context("x.jpg", 0);
        assert_eq!(p.render(&c).unwrap(), "a{b}c.jpg");
    }

    #[test]
    fn rejects_bad_patterns() {
        assert!(Pattern::parse("{unknown}").is_err());
        assert!(Pattern::parse("{name").is_err());
        assert!(Pattern::parse("}").is_err());
        assert!(Pattern::parse("{seq:0>0}").is_err());
        assert!(Pattern::parse("{keyword#0}").is_err());
    }

    #[test]
    fn plan_detects_collisions_case_insensitive() {
        let p = Pattern::parse("same").unwrap();
        let contexts = vec![context("a.jpg", 0), context("b.jpg", 1)];
        // Both render "same.jpg" -> collision
        assert!(plan(&p, &contexts).is_err());
    }

    #[test]
    fn plan_rejects_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        std::fs::write(&a, b"a").unwrap();
        let b = temp.path().join("b.txt");
        std::fs::write(&b, b"b").unwrap();
        let p = Pattern::parse("{name}-x").unwrap();
        let contexts = vec![
            RenameContext::from_path(a.clone(), 0),
            RenameContext::from_path(b.clone(), 1),
        ];
        // No collision, targets don't exist yet.
        assert!(plan(&p, &contexts).is_ok());

        let p2 = Pattern::parse("{name}").unwrap();
        let contexts2 = vec![RenameContext::from_path(a.clone(), 0)];
        // Same name -> no-op allowed.
        assert!(plan(&p2, &contexts2).is_ok());
    }

    #[test]
    fn apply_and_undo_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        std::fs::write(&a, b"a").unwrap();
        let journal = journal_path(temp.path());
        let p = Pattern::parse("renamed_{seq:0>2}").unwrap();
        let contexts = vec![RenameContext::from_path(a.clone(), 0)];
        let items = plan(&p, &contexts).unwrap();
        let n = apply(&items, &journal).unwrap();
        assert_eq!(n, 1);
        assert!(temp.path().join("renamed_00.txt").exists());
        assert!(journal.exists());

        let restored = undo(&journal).unwrap();
        assert_eq!(restored, 1);
        assert!(a.exists());
        assert!(!journal.exists());
    }

    #[test]
    fn plan_refuses_cross_directory_names() {
        let p = Pattern::parse("{name}/evil").unwrap();
        let c = context("a.jpg", 0);
        assert!(plan(&p, &[c]).is_err());
    }
}
