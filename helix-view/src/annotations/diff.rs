//! Inline display of a document's changes against its diff base.
use std::ops::Range;

use helix_core::text_annotations::LineAnnotation;
use helix_core::{Position, Rope, RopeSlice};

use crate::Document;

/// Removed base lines shown below a document line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedBlock {
    /// The document line the removed lines are drawn after.
    pub anchor: usize,
    /// Line ranges in the diff base, in order.
    pub ranges: Vec<Range<usize>>,
}

impl RemovedBlock {
    /// The number of removed lines, which is also the number of virtual lines.
    pub fn len(&self) -> usize {
        self.ranges.iter().map(|range| range.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Base line indices in display order.
    pub fn lines(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(|range| range.clone())
    }
}

/// A snapshot of the document's hunks for one render.
#[derive(Debug, Clone)]
pub struct InlineDiff {
    pub base: Rope,
    /// Document line ranges that were added or changed, in ascending order.
    pub added: Vec<Range<usize>>,
    /// Removed lines, sorted by anchor. Each anchor appears at most once.
    pub removed: Vec<RemovedBlock>,
}

impl InlineDiff {
    /// The inline diff for a document, if it has inline diff enabled and a diff base.
    pub fn for_document(doc: &Document) -> Option<Self> {
        if !doc.inline_diff {
            return None;
        }
        let handle = doc.diff_handle()?;
        let diff = handle.load();
        let hunks = (0..diff.len()).map(|index| diff.nth_hunk(index));
        Some(Self::from_hunks(
            diff.diff_base().clone(),
            hunks.map(|hunk| {
                (
                    hunk.before.start as usize..hunk.before.end as usize,
                    hunk.after.start as usize..hunk.after.end as usize,
                )
            }),
        ))
    }

    /// Build from `(before, after)` line ranges in ascending order.
    pub fn from_hunks(
        base: Rope,
        hunks: impl IntoIterator<Item = (Range<usize>, Range<usize>)>,
    ) -> Self {
        let mut added = Vec::new();
        let mut removed: Vec<RemovedBlock> = Vec::new();
        for (before, after) in hunks {
            if !after.is_empty() {
                added.push(after.clone());
            }
            if before.is_empty() {
                continue;
            }
            // Removed lines go above their replacement, after the preceding
            // line. Virtual lines cannot precede the first line, so changes
            // there are shown below the hunk instead.
            let anchor = if after.start > 0 {
                after.start - 1
            } else {
                after.end.saturating_sub(1)
            };
            match removed.last_mut() {
                Some(block) if block.anchor == anchor => block.ranges.push(before),
                _ => removed.push(RemovedBlock {
                    anchor,
                    ranges: vec![before],
                }),
            }
        }
        Self {
            base,
            added,
            removed,
        }
    }

    pub fn is_added(&self, line: usize) -> bool {
        let index = self.added.partition_point(|range| range.end <= line);
        self.added
            .get(index)
            .is_some_and(|range| range.contains(&line))
    }

    pub fn removed_after(&self, line: usize) -> Option<&RemovedBlock> {
        self.removed
            .binary_search_by_key(&line, |block| block.anchor)
            .ok()
            .map(|index| &self.removed[index])
    }
}

/// Reserves one virtual line per removed base line after its anchor line.
pub(crate) struct InlineDiffLines<'a> {
    text: RopeSlice<'a>,
    diff: InlineDiff,
}

impl<'a> InlineDiffLines<'a> {
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(text: RopeSlice<'a>, diff: InlineDiff) -> Box<dyn LineAnnotation + 'a> {
        Box::new(Self { text, diff })
    }
}

/// The char index after a line's line ending, where its virtual lines begin.
pub fn line_end(text: RopeSlice, line: usize) -> usize {
    text.line_to_char((line + 1).min(text.len_lines()))
}

impl LineAnnotation for InlineDiffLines<'_> {
    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        // Called at every visual line end; only the last one of a soft-wrapped
        // line ends at the line break.
        if line_end_char_idx != line_end(self.text, doc_line) {
            return Position::new(0, 0);
        }
        let rows = self
            .diff
            .removed_after(doc_line)
            .map_or(0, RemovedBlock::len);
        Position::new(rows, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn removals_anchor_above_their_replacement_and_merge_per_line() {
        let diff = InlineDiff::from_hunks(
            Rope::from("a\nb\nc\nd\n"),
            [
                (0..1, 0..2), // first line changed: shown below the hunk
                (2..3, 3..3), // pure removal after document line 2
                (3..4, 4..5), // change at line 4, anchored after line 3
            ],
        );
        assert_eq!(diff.added, vec![0..2, 4..5]);
        assert_eq!(
            diff.removed,
            vec![
                RemovedBlock {
                    anchor: 1,
                    ranges: vec![0..1]
                },
                RemovedBlock {
                    anchor: 2,
                    ranges: vec![2..3]
                },
                RemovedBlock {
                    anchor: 3,
                    ranges: vec![3..4]
                },
            ]
        );
        assert!(diff.is_added(1));
        assert!(!diff.is_added(2));
        assert!(diff.is_added(4));
        assert_eq!(diff.removed_after(2).unwrap().len(), 1);
        // Two removals that both precede the second line share one anchor.
        let shared = InlineDiff::from_hunks(Rope::from("a\nb\nc\n"), [(0..1, 0..1), (2..3, 1..1)]);
        assert_eq!(shared.removed.len(), 1);
        assert_eq!(shared.removed[0].lines().collect::<Vec<_>>(), [0, 2]);
    }

    #[test]
    fn virtual_lines_are_reserved_only_at_the_line_break() {
        let text = Rope::from("one\ntwo\n");
        let diff = InlineDiff::from_hunks(Rope::from("x\ny\n"), [(0..2, 1..1)]);
        let mut lines = InlineDiffLines {
            text: text.slice(..),
            diff,
        };
        // A soft wrap inside line 0 reserves nothing.
        assert_eq!(lines.insert_virtual_lines(2, Position::new(0, 0), 0).row, 0);
        assert_eq!(lines.insert_virtual_lines(4, Position::new(0, 0), 0).row, 2);
        assert_eq!(lines.insert_virtual_lines(8, Position::new(1, 0), 1).row, 0);
    }
}
