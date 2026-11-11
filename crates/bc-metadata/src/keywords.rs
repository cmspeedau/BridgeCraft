//! Hierarchical keyword tagging (issue #12).
//!
//! Keywords are stored as standard XMP `dc:subject` bags using the
//! hierarchical-keyword convention (`Parent|Child|Grandchild`) from XMP
//! Specification Part 1, so other tools see the same tree. This module holds
//! the pure (egui-free) logic: normalisation, add/remove on [`Metadata`],
//! tree building, type-ahead suggestions, and tag counts.
//!
//! Clean-room: XMP Specification Part 1 (hierarchical keywords).

use std::collections::{BTreeMap, BTreeSet};

use crate::{Metadata, Value, NS_DC};

/// A node in a keyword tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordNode {
    /// Segment name (the last path component).
    pub name: String,
    /// Full hierarchical path (`Parent|Child`).
    pub path: String,
    /// Number of assets tagged with this exact keyword.
    pub count: usize,
    pub children: Vec<KeywordNode>,
}

/// Normalise a single keyword segment: trim and collapse runs of whitespace.
/// Never case-folds.
pub fn normalize_segment(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalise a (possibly hierarchical) keyword: each `|`-separated segment is
/// normalised; empty segments are dropped.
pub fn normalize_keyword(s: &str) -> String {
    s.split('|')
        .map(normalize_segment)
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("|")
}

/// Read the keyword list from `dc:subject`.
pub fn subjects(meta: &Metadata) -> Vec<String> {
    match meta.get(NS_DC, "subject") {
        Some(Value::Array(_, items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        Some(Value::Text(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Replace the keyword list.
pub fn set_subjects(meta: &mut Metadata, subjects: Vec<String>) {
    if subjects.is_empty() {
        meta.remove(NS_DC, "subject");
    } else {
        meta.set(
            NS_DC,
            "subject",
            Value::Array(
                crate::ArrayKind::Bag,
                subjects.into_iter().map(Value::Text).collect(),
            ),
        );
    }
}

/// Add a keyword (normalised, de-duplicated).
pub fn add_keyword(meta: &mut Metadata, keyword: &str) {
    let kw = normalize_keyword(keyword);
    if kw.is_empty() {
        return;
    }
    let mut list = subjects(meta);
    if !list.iter().any(|s| s == &kw) {
        list.push(kw);
    }
    set_subjects(meta, list);
}

/// Remove a keyword (normalised).
pub fn remove_keyword(meta: &mut Metadata, keyword: &str) {
    let kw = normalize_keyword(keyword);
    let list: Vec<String> = subjects(meta).into_iter().filter(|s| s != &kw).collect();
    set_subjects(meta, list);
}

/// Whether a keyword is present.
pub fn has_keyword(meta: &Metadata, keyword: &str) -> bool {
    let kw = normalize_keyword(keyword);
    subjects(meta).iter().any(|s| s == &kw)
}

// ---------------------------------------------------------------------------
// Tree building
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TrieNode {
    count: usize,
    children: BTreeMap<String, TrieNode>,
}

impl TrieNode {
    fn insert(&mut self, segments: &[&str], count: usize) {
        if segments.is_empty() {
            return;
        }
        let seg = segments[0];
        let child = self
            .children
            .entry(seg.to_string())
            .or_insert_with(|| TrieNode {
                count: 0,
                children: BTreeMap::new(),
            });
        if segments.len() == 1 {
            child.count += count;
        } else {
            child.insert(&segments[1..], count);
        }
    }
}

fn to_nodes(node: &TrieNode, path_prefix: &str) -> Vec<KeywordNode> {
    node.children
        .iter()
        .map(|(name, child)| {
            let path = if path_prefix.is_empty() {
                name.clone()
            } else {
                format!("{}|{}", path_prefix, name)
            };
            KeywordNode {
                name: name.clone(),
                path: path.clone(),
                count: child.count,
                children: to_nodes(child, &path),
            }
        })
        .collect()
}

/// Build a keyword tree from a flat list of keyword instances (duplicates
/// across assets are counted).
pub fn build_tree(all_keywords: &[String]) -> Vec<KeywordNode> {
    let mut freq: BTreeMap<String, usize> = BTreeMap::new();
    for k in all_keywords {
        let n = normalize_keyword(k);
        if !n.is_empty() {
            *freq.entry(n).or_insert(0) += 1;
        }
    }
    let mut root = TrieNode::default();
    for (kw, count) in &freq {
        let segments: Vec<&str> = kw.split('|').collect();
        root.insert(&segments, *count);
    }
    to_nodes(&root, "")
}

/// Total count under a node (itself + all descendants).
pub fn subtree_count(node: &KeywordNode) -> usize {
    node.count + node.children.iter().map(subtree_count).sum::<usize>()
}

/// Type-ahead suggestions: existing keywords that start with `prefix`
/// (normalised). Returns sorted, de-duplicated.
pub fn suggest(keywords: &[String], prefix: &str) -> Vec<String> {
    let p = normalize_keyword(prefix);
    let mut set: BTreeSet<String> = BTreeSet::new();
    for k in keywords {
        let n = normalize_keyword(k);
        if n.is_empty() {
            continue;
        }
        let matches = if p.is_empty() {
            true
        } else {
            n.starts_with(&p) && n != p
        };
        if matches {
            set.insert(n);
        }
    }
    set.into_iter().collect()
}

/// Direct children of a given parent path (empty parent = top level).
pub fn children_of<'a>(tree: &'a [KeywordNode], parent_path: &str) -> Vec<&'a KeywordNode> {
    if parent_path.is_empty() {
        return tree.iter().collect();
    }
    // navigate
    fn find<'a>(nodes: &'a [KeywordNode], path: &str) -> Option<&'a KeywordNode> {
        for n in nodes {
            if n.path == path {
                return Some(n);
            }
            if let Some(found) = find(&n.children, path) {
                return Some(found);
            }
        }
        None
    }
    find(tree, parent_path)
        .map(|n| n.children.iter().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_segment("  a   b  "), "a b");
        assert_eq!(normalize_keyword("Travel |  Europe "), "Travel|Europe");
        assert_eq!(normalize_keyword("a||b"), "a|b");
    }

    #[test]
    fn add_remove_dedup() {
        let mut m = Metadata::new();
        add_keyword(&mut m, "Travel | Europe");
        add_keyword(&mut m, "travel | europe"); // not case-folded -> distinct
                                                // case differs, so both present
        assert_eq!(subjects(&m).len(), 2);
        add_keyword(&mut m, "Travel|Europe");
        assert_eq!(subjects(&m).len(), 2);
        remove_keyword(&mut m, "Travel|Europe");
        assert_eq!(subjects(&m).len(), 1);
        assert!(has_keyword(&m, "travel | europe"));
    }

    #[test]
    fn roundtrip_through_xmp() {
        let mut m = Metadata::new();
        add_keyword(&mut m, "Travel|Europe");
        add_keyword(&mut m, "Travel|Asia");
        let xml = crate::xmp::write_xmp(&m);
        let parsed = crate::xmp::parse_xmp(xml.as_bytes()).unwrap();
        assert!(has_keyword(&parsed, "Travel|Europe"));
        assert!(has_keyword(&parsed, "Travel|Asia"));
    }

    #[test]
    fn build_tree_counts_and_structure() {
        let kws = vec![
            "Travel|Europe".to_string(),
            "Travel|Europe".to_string(),
            "Travel|Asia".to_string(),
            "Food".to_string(),
        ];
        let tree = build_tree(&kws);
        // top-level sorted: Food, Travel
        assert_eq!(tree[0].name, "Food");
        assert_eq!(tree[0].count, 1);
        let travel = &tree[1];
        assert_eq!(travel.name, "Travel");
        assert_eq!(travel.count, 0);
        assert_eq!(subtree_count(travel), 3);
        let europe = travel.children.iter().find(|n| n.name == "Europe").unwrap();
        assert_eq!(europe.count, 2);
        assert_eq!(europe.path, "Travel|Europe");
    }

    #[test]
    fn suggest_typeahead() {
        let kws = vec![
            "Travel|Europe".to_string(),
            "Travel|Asia".to_string(),
            "Food".to_string(),
        ];
        let s = suggest(&kws, "Travel|");
        assert_eq!(
            s,
            vec!["Travel|Asia".to_string(), "Travel|Europe".to_string()]
        );
        let s2 = suggest(&kws, "Tr");
        assert!(s2.contains(&"Travel|Asia".to_string()));
    }

    #[test]
    fn children_of_navigation() {
        let kws = vec!["Travel|Europe".to_string(), "Travel|Asia".to_string()];
        let tree = build_tree(&kws);
        let top = children_of(&tree, "");
        assert_eq!(top.len(), 1);
        let kids = children_of(&tree, "Travel");
        assert_eq!(kids.len(), 2);
    }
}
