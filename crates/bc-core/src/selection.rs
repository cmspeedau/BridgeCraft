//! Selection model: multi-select with range support, order-preserving.

use std::collections::BTreeSet;

/// Tracks selected indices within the current item list.
/// Indices refer to positions in the (sorted) content list.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    indices: BTreeSet<usize>,
    /// Last clicked item, used as the anchor for shift-range selection.
    anchor: Option<usize>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.indices.clear();
        self.anchor = None;
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn contains(&self, idx: usize) -> bool {
        self.indices.contains(&idx)
    }

    /// Selected indices in ascending order.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices.iter().copied()
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// Ctrl-click behavior: toggle one item and move the anchor to it.
    pub fn toggle(&mut self, idx: usize) {
        if self.indices.contains(&idx) {
            self.indices.remove(&idx);
        } else {
            self.indices.insert(idx);
        }
        self.anchor = Some(idx);
    }

    /// Plain click: select only this item.
    pub fn select_only(&mut self, idx: usize) {
        self.indices.clear();
        self.indices.insert(idx);
        self.anchor = Some(idx);
    }

    /// Shift-click behavior: select the range from the anchor to `idx`.
    /// Without an anchor this behaves like a plain click.
    pub fn select_range(&mut self, idx: usize) {
        match self.anchor {
            None => self.select_only(idx),
            Some(anchor) => {
                let (lo, hi) = if anchor <= idx {
                    (anchor, idx)
                } else {
                    (idx, anchor)
                };
                for i in lo..=hi {
                    self.indices.insert(i);
                }
                // Per common file-manager behavior, the anchor stays fixed
                // across shift-clicks.
            }
        }
    }

    /// Select everything in `0..count`.
    pub fn select_all(&mut self, count: usize) {
        self.indices = (0..count).collect();
        self.anchor = count.checked_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_adds_and_removes() {
        let mut s = Selection::new();
        s.toggle(1);
        s.toggle(3);
        assert_eq!(s.indices().collect::<Vec<_>>(), vec![1, 3]);
        s.toggle(1);
        assert_eq!(s.indices().collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn range_select_forward_and_backward() {
        let mut s = Selection::new();
        s.select_only(2);
        s.select_range(5);
        assert_eq!(s.indices().collect::<Vec<_>>(), vec![2, 3, 4, 5]);
        // Backward from same anchor; anchor stays at 2.
        s.select_range(0);
        assert_eq!(s.indices().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn select_all_and_clear() {
        let mut s = Selection::new();
        s.select_all(4);
        assert_eq!(s.len(), 4);
        s.clear();
        assert!(s.is_empty());
    }
}
