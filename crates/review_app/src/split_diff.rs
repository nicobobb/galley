//! Side-by-side layout: pairs removed lines with the added lines that replace them.

use review_git::LineKind;

/// What a unified row is, for pairing purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTag {
    /// A full-width row (hunk header, notice).
    Banner,
    Line(LineKind),
}

/// A side-by-side row; cells are indices into the unified rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRow {
    Banner(usize),
    Pair {
        left: Option<usize>,
        right: Option<usize>,
    },
}

/// Lays out unified rows side by side. Context lines appear on both sides; a run
/// of removed lines followed by added lines is zipped so changes line up.
pub fn split_rows(tags: &[RowTag]) -> Vec<SplitRow> {
    let mut split = Vec::with_capacity(tags.len());
    let mut removed: Vec<usize> = Vec::new();
    let mut added: Vec<usize> = Vec::new();
    let flush = |removed: &mut Vec<usize>, added: &mut Vec<usize>, split: &mut Vec<SplitRow>| {
        for i in 0..removed.len().max(added.len()) {
            split.push(SplitRow::Pair {
                left: removed.get(i).copied(),
                right: added.get(i).copied(),
            });
        }
        removed.clear();
        added.clear();
    };

    for (index, tag) in tags.iter().enumerate() {
        match tag {
            RowTag::Line(LineKind::Removed) => {
                // Removals after additions start a new change block.
                if !added.is_empty() {
                    flush(&mut removed, &mut added, &mut split);
                }
                removed.push(index);
            }
            RowTag::Line(LineKind::Added) => added.push(index),
            RowTag::Line(LineKind::Context) => {
                flush(&mut removed, &mut added, &mut split);
                split.push(SplitRow::Pair {
                    left: Some(index),
                    right: Some(index),
                });
            }
            RowTag::Banner => {
                flush(&mut removed, &mut added, &mut split);
                split.push(SplitRow::Banner(index));
            }
        }
    }
    flush(&mut removed, &mut added, &mut split);
    split
}

/// For each unified row, the split row that shows it.
pub fn unified_to_split(split: &[SplitRow], unified_len: usize) -> Vec<usize> {
    let mut map = vec![0; unified_len];
    for (split_index, row) in split.iter().enumerate() {
        match *row {
            SplitRow::Banner(index) => map[index] = split_index,
            SplitRow::Pair { left, right } => {
                for index in left.into_iter().chain(right) {
                    map[index] = split_index;
                }
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use LineKind::*;
    use RowTag::*;

    #[test]
    fn pairs_replacements_and_pads_uneven_blocks() {
        let tags = [
            Banner,
            Line(Context),
            Line(Removed),
            Line(Removed),
            Line(Added),
            Line(Context),
            Line(Added),
            Line(Added),
        ];
        let split = split_rows(&tags);
        let pair = |left, right| SplitRow::Pair { left, right };
        assert_eq!(
            split,
            [
                SplitRow::Banner(0),
                pair(Some(1), Some(1)),
                pair(Some(2), Some(4)),
                pair(Some(3), None),
                pair(Some(5), Some(5)),
                pair(None, Some(6)),
                pair(None, Some(7)),
            ]
        );
        assert_eq!(
            unified_to_split(&split, tags.len()),
            [0, 1, 2, 3, 2, 4, 5, 6]
        );
    }

    #[test]
    fn removal_after_addition_starts_new_block() {
        let tags = [Line(Added), Line(Removed)];
        let pair = |left, right| SplitRow::Pair { left, right };
        assert_eq!(
            split_rows(&tags),
            [pair(None, Some(0)), pair(Some(1), None)]
        );
    }
}
