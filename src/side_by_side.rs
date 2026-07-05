use similar::{DiffTag, TextDiff};

/// How a row in the side-by-side view relates the two file versions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Removed,
    Added,
    Modified,
}

/// One display row: the old and new side each carry a 1-based line
/// number and text, or None when the line only exists on the other side
#[derive(Debug, Clone)]
pub struct AlignedRow {
    pub old: Option<(usize, String)>,
    pub new: Option<(usize, String)>,
    pub kind: RowKind,
}

/// Align two file versions line-by-line for side-by-side display
pub fn align(old_text: &str, new_text: &str) -> Vec<AlignedRow> {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let diff = TextDiff::from_lines(old_text, new_text);

    let mut rows = Vec::new();
    for op in diff.ops() {
        let old_range = op.old_range();
        let new_range = op.new_range();

        match op.tag() {
            DiffTag::Equal => {
                for (old_index, new_index) in old_range.zip(new_range) {
                    rows.push(AlignedRow {
                        old: Some((old_index + 1, clean(old_lines[old_index]))),
                        new: Some((new_index + 1, clean(new_lines[new_index]))),
                        kind: RowKind::Context,
                    });
                }
            }
            DiffTag::Delete => {
                for old_index in old_range {
                    rows.push(AlignedRow {
                        old: Some((old_index + 1, clean(old_lines[old_index]))),
                        new: None,
                        kind: RowKind::Removed,
                    });
                }
            }
            DiffTag::Insert => {
                for new_index in new_range {
                    rows.push(AlignedRow {
                        old: None,
                        new: Some((new_index + 1, clean(new_lines[new_index]))),
                        kind: RowKind::Added,
                    });
                }
            }
            DiffTag::Replace => {
                let old_indices: Vec<usize> = old_range.collect();
                let new_indices: Vec<usize> = new_range.collect();
                let paired = old_indices.len().min(new_indices.len());

                for i in 0..paired {
                    rows.push(AlignedRow {
                        old: Some((old_indices[i] + 1, clean(old_lines[old_indices[i]]))),
                        new: Some((new_indices[i] + 1, clean(new_lines[new_indices[i]]))),
                        kind: RowKind::Modified,
                    });
                }
                for &old_index in &old_indices[paired..] {
                    rows.push(AlignedRow {
                        old: Some((old_index + 1, clean(old_lines[old_index]))),
                        new: None,
                        kind: RowKind::Removed,
                    });
                }
                for &new_index in &new_indices[paired..] {
                    rows.push(AlignedRow {
                        old: None,
                        new: Some((new_index + 1, clean(new_lines[new_index]))),
                        kind: RowKind::Added,
                    });
                }
            }
        }
    }

    rows
}

/// Strip trailing carriage returns so CRLF files render cleanly
fn clean(line: &str) -> String {
    line.trim_end_matches('\r').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_texts_are_all_context() {
        let rows = align("a\nb\n", "a\nb\n");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.kind == RowKind::Context));
        assert_eq!(rows[0].old, Some((1, "a".to_string())));
        assert_eq!(rows[0].new, Some((1, "a".to_string())));
    }

    #[test]
    fn test_pure_insertion() {
        let rows = align("a\nc\n", "a\nb\nc\n");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, RowKind::Added);
        assert_eq!(rows[1].old, None);
        assert_eq!(rows[1].new, Some((2, "b".to_string())));
        // Context lines keep both line numbers aligned
        assert_eq!(rows[2].old, Some((2, "c".to_string())));
        assert_eq!(rows[2].new, Some((3, "c".to_string())));
    }

    #[test]
    fn test_pure_deletion() {
        let rows = align("a\nb\nc\n", "a\nc\n");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, RowKind::Removed);
        assert_eq!(rows[1].old, Some((2, "b".to_string())));
        assert_eq!(rows[1].new, None);
    }

    #[test]
    fn test_replace_pairs_lines_and_spills_extras() {
        // one old line replaced by three new lines
        let rows = align("a\nOLD\nz\n", "a\nNEW1\nNEW2\nNEW3\nz\n");
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Context,
                RowKind::Modified,
                RowKind::Added,
                RowKind::Added,
                RowKind::Context,
            ]
        );
        assert_eq!(rows[1].old, Some((2, "OLD".to_string())));
        assert_eq!(rows[1].new, Some((2, "NEW1".to_string())));
    }

    #[test]
    fn test_empty_old_side_is_all_added() {
        let rows = align("", "a\nb\n");
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r.kind == RowKind::Added && r.old.is_none())
        );
    }

    #[test]
    fn test_crlf_lines_are_cleaned() {
        let rows = align("a\r\n", "b\r\n");
        assert_eq!(rows[0].old, Some((1, "a".to_string())));
        assert_eq!(rows[0].new, Some((1, "b".to_string())));
    }
}
