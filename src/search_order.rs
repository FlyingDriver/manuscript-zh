//! Pure ordering logic for in-document search (no GTK types).
//!
//! View mode searches the main rendered buffer *and* the text inside rich
//! table cells (separate TextViews mounted at child anchors). Both kinds of
//! match are merged into a single list in document order so the `X / Y`
//! counter and next/previous step through them seamlessly.

/// Where a merged match lives, as indices into the caller's own storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitRef {
    /// `main[i]`
    Main(usize),
    /// `tables[table].cells[cell][i]`
    Cell { table: usize, cell: usize, idx: usize },
}

/// One table: the anchor offset in the main buffer plus, per cell
/// (row-major), the match offsets inside that cell (ascending).
pub struct TableMatches {
    pub anchor_offset: i32,
    pub cells: Vec<Vec<i32>>,
}

/// Merge main-buffer match offsets (ascending) and table cell matches into
/// document order. A cell match sorts at its table's anchor offset; within a
/// table: row-major cell order, then offset inside the cell. A main match at
/// the same offset as an anchor sorts first (it cannot really coincide, the
/// anchor occupies one character, but the order is defined anyway).
pub fn merge_matches(main: &[i32], tables: &[TableMatches]) -> Vec<HitRef> {
    // (main offset or anchor offset, kind 0=main/1=cell, cell, offset-in-cell, ref)
    let mut keyed: Vec<((i32, u8, usize, i32), HitRef)> = Vec::new();
    for (i, off) in main.iter().enumerate() {
        keyed.push(((*off, 0, 0, 0), HitRef::Main(i)));
    }
    for (t, table) in tables.iter().enumerate() {
        for (c, offs) in table.cells.iter().enumerate() {
            for (i, off) in offs.iter().enumerate() {
                keyed.push((
                    (table.anchor_offset, 1, c, *off),
                    HitRef::Cell { table: t, cell: c, idx: i },
                ));
            }
        }
    }
    keyed.sort_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tables_keeps_main_order() {
        let r = merge_matches(&[1, 5, 9], &[]);
        assert_eq!(r, vec![HitRef::Main(0), HitRef::Main(1), HitRef::Main(2)]);
    }

    #[test]
    fn cells_sort_at_anchor_between_main_matches() {
        let tables = [TableMatches {
            anchor_offset: 10,
            cells: vec![vec![3, 7], vec![], vec![0]],
        }];
        let r = merge_matches(&[2, 20], &tables);
        assert_eq!(
            r,
            vec![
                HitRef::Main(0),
                HitRef::Cell { table: 0, cell: 0, idx: 0 },
                HitRef::Cell { table: 0, cell: 0, idx: 1 },
                HitRef::Cell { table: 0, cell: 2, idx: 0 },
                HitRef::Main(1),
            ]
        );
    }

    #[test]
    fn multiple_tables_in_anchor_order() {
        let tables = [
            TableMatches { anchor_offset: 4, cells: vec![vec![1]] },
            TableMatches { anchor_offset: 30, cells: vec![vec![2]] },
        ];
        let r = merge_matches(&[10], &tables);
        assert_eq!(
            r,
            vec![
                HitRef::Cell { table: 0, cell: 0, idx: 0 },
                HitRef::Main(0),
                HitRef::Cell { table: 1, cell: 0, idx: 0 },
            ]
        );
    }
}
