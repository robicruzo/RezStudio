//! What the viewer knows about the data frame: its shape and columns from
//! Ark, and rows fetched in blocks as they scroll into view. Rows never all
//! leave R; only the blocks on screen (and recently on screen) are kept.

use collections::HashMap;
use runtime::comms::{ColumnDisplayType, ColumnSchema, ColumnSortKey, ColumnValue, TableShape};
use std::ops::Range;

/// Rows fetched per request.
pub const BLOCK_ROWS: usize = 100;
/// Columns shown at once; wider data frames page through them, like RStudio.
pub const PAGE_COLUMNS: usize = 50;
/// Blocks kept in memory before the least recently shown are dropped.
const MAX_BLOCKS: usize = 64;

#[derive(Default)]
pub struct ExplorerData {
    pub shape: Option<TableShape>,
    pub has_row_labels: bool,
    /// Schemas of the columns on the current page, in order.
    pub columns: Vec<ColumnSchema>,
    /// First column of the current page.
    pub first_column: usize,
    pub sort: Option<ColumnSortKey>,
    /// Missing values per column index, once Ark has counted them.
    pub null_counts: HashMap<i64, i64>,
    blocks: HashMap<usize, Block>,
    /// When each block was last asked for, counted in `claim_missing_blocks` calls.
    last_used: HashMap<usize, u64>,
    uses: u64,
}

pub enum Block {
    Loading,
    Loaded {
        /// One vector per column on the page.
        columns: Vec<Vec<ColumnValue>>,
        labels: Option<Vec<String>>,
    },
    Failed(String),
}

impl ExplorerData {
    pub fn row_count(&self) -> usize {
        self.shape
            .map_or(0, |shape| usize::try_from(shape.num_rows).unwrap_or(0))
    }

    pub fn column_count(&self) -> usize {
        self.shape
            .map_or(0, |shape| usize::try_from(shape.num_columns).unwrap_or(0))
    }

    /// The column indices on the current page.
    pub fn page_columns(&self) -> Range<usize> {
        let end = (self.first_column + PAGE_COLUMNS).min(self.column_count());
        self.first_column.min(end)..end
    }

    pub fn can_page(&self, forward: bool) -> bool {
        if forward {
            self.first_column + PAGE_COLUMNS < self.column_count()
        } else {
            self.first_column > 0
        }
    }

    /// Moves a page of columns; returns whether the page changed.
    pub fn page(&mut self, forward: bool) -> bool {
        if !self.can_page(forward) {
            return false;
        }
        self.first_column = if forward {
            self.first_column + PAGE_COLUMNS
        } else {
            self.first_column.saturating_sub(PAGE_COLUMNS)
        };
        self.columns.clear();
        self.invalidate_rows();
        true
    }

    /// Forgets fetched rows, as after a sort or a change in R.
    pub fn invalidate_rows(&mut self) {
        self.blocks.clear();
        self.last_used.clear();
    }

    /// Up to `limit` of the blocks covering `rows` that haven't been
    /// requested, now marked as loading.
    pub fn claim_missing_blocks(&mut self, rows: Range<usize>, limit: usize) -> Vec<usize> {
        if rows.is_empty() || self.columns.is_empty() {
            return Vec::new();
        }
        self.uses += 1;
        let first = rows.start / BLOCK_ROWS;
        let last = (rows.end - 1) / BLOCK_ROWS;
        let missing: Vec<usize> = (first..=last)
            .filter(|block| !self.blocks.contains_key(block))
            .take(limit)
            .collect();
        for block in &missing {
            self.blocks.insert(*block, Block::Loading);
        }
        for block in first..=last {
            if self.blocks.contains_key(&block) {
                self.last_used.insert(block, self.uses);
            }
        }
        self.evict_least_recent();
        missing
    }

    /// The rows a block covers, clipped to the data frame.
    pub fn block_rows(&self, block: usize) -> Range<usize> {
        let start = block * BLOCK_ROWS;
        start.min(self.row_count())..(start + BLOCK_ROWS).min(self.row_count())
    }

    pub fn set_block(&mut self, block: usize, value: Block) {
        // A block may have been dropped (scrolled far away or invalidated) while loading.
        if self.blocks.contains_key(&block) {
            self.blocks.insert(block, value);
        }
    }

    /// A cell's value, or `None` while its block loads.
    pub fn cell(&self, row: usize, page_column: usize) -> Option<&ColumnValue> {
        match self.blocks.get(&(row / BLOCK_ROWS))? {
            Block::Loaded { columns, .. } => columns.get(page_column)?.get(row % BLOCK_ROWS),
            _ => None,
        }
    }

    /// The label to show for a row: its name, or its 1-based position.
    pub fn row_label(&self, row: usize) -> Option<String> {
        if !self.has_row_labels {
            return Some((row + 1).to_string());
        }
        match self.blocks.get(&(row / BLOCK_ROWS))? {
            Block::Loaded {
                labels: Some(labels),
                ..
            } => labels.get(row % BLOCK_ROWS).cloned(),
            _ => None,
        }
    }

    pub fn block_error(&self, row: usize) -> Option<&str> {
        match self.blocks.get(&(row / BLOCK_ROWS))? {
            Block::Failed(message) => Some(message),
            _ => None,
        }
    }

    /// Clicking a header cycles ascending, descending, then R's original order.
    pub fn next_sort(&self, column_index: i64) -> Option<ColumnSortKey> {
        match self.sort {
            Some(key) if key.column_index == column_index && key.ascending => Some(ColumnSortKey {
                column_index,
                ascending: false,
            }),
            Some(key) if key.column_index == column_index => None,
            _ => Some(ColumnSortKey {
                column_index,
                ascending: true,
            }),
        }
    }

    /// Drops the blocks shown longest ago. Not the farthest from view: the
    /// list asks for row 0 every frame to measure row height, and that must
    /// not push out the rows actually on screen.
    fn evict_least_recent(&mut self) {
        if self.blocks.len() <= MAX_BLOCKS {
            return;
        }
        let mut blocks: Vec<usize> = self.blocks.keys().copied().collect();
        blocks.sort_by_key(|block| self.last_used.get(block).copied().unwrap_or(0));
        for block in blocks.into_iter().take(self.blocks.len() - MAX_BLOCKS) {
            self.blocks.remove(&block);
            self.last_used.remove(&block);
        }
    }
}

/// Whether a column's values read best right-aligned, as numbers do.
pub fn is_numeric(display: ColumnDisplayType) -> bool {
    matches!(
        display,
        ColumnDisplayType::Integer | ColumnDisplayType::Floating | ColumnDisplayType::Decimal
    )
}

/// Thousands separators for counts in the toolbar, like `10,000,000`.
pub fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(rows: i64, columns: i64) -> ExplorerData {
        let mut data = ExplorerData {
            shape: Some(TableShape {
                num_rows: rows,
                num_columns: columns,
            }),
            ..ExplorerData::default()
        };
        data.columns = (0..columns.min(PAGE_COLUMNS as i64))
            .map(|index| ColumnSchema {
                column_name: format!("c{index}"),
                column_label: None,
                column_index: index,
                type_name: "dbl".into(),
                type_display: ColumnDisplayType::Floating,
            })
            .collect();
        data
    }

    #[test]
    fn test_blocks_are_claimed_once_and_filled() {
        let mut data = data(1_000, 2);
        assert_eq!(data.claim_missing_blocks(150..260, usize::MAX), [1, 2]);
        assert!(data.claim_missing_blocks(150..260, usize::MAX).is_empty());
        assert_eq!(
            data.claim_missing_blocks(250..560, 2),
            [3, 4],
            "at most `limit` at once"
        );
        assert_eq!(data.claim_missing_blocks(250..560, 2), [5]);
        assert_eq!(data.block_rows(9), 900..1_000);
        assert_eq!(data.block_rows(10), 1_000..1_000);

        assert_eq!(data.cell(150, 0), None);
        data.set_block(
            1,
            Block::Loaded {
                columns: vec![
                    (100..200)
                        .map(|row| ColumnValue::FormattedValue(row.to_string()))
                        .collect(),
                    vec![ColumnValue::SpecialValueCode(1); 100],
                ],
                labels: None,
            },
        );
        assert_eq!(
            data.cell(150, 0),
            Some(&ColumnValue::FormattedValue("150".into()))
        );
        assert_eq!(
            data.cell(150, 1).map(ColumnValue::display),
            Some(("NA", true))
        );
        assert_eq!(data.row_label(150).as_deref(), Some("151"));

        // A block dropped while loading isn't resurrected by its late reply.
        data.invalidate_rows();
        data.set_block(2, Block::Failed("too late".into()));
        assert_eq!(data.block_error(250), None);
    }

    #[test]
    fn test_least_recently_shown_blocks_are_dropped() {
        let mut data = data(10_000_000, 1);
        for block in 0..MAX_BLOCKS {
            data.claim_missing_blocks(block * BLOCK_ROWS..(block + 1) * BLOCK_ROWS, usize::MAX);
        }
        let far = 90_000 * BLOCK_ROWS;
        assert_eq!(
            data.claim_missing_blocks(far..far + 30, usize::MAX),
            [90_000]
        );
        assert_eq!(data.blocks.len(), MAX_BLOCKS);
        assert!(!data.blocks.contains_key(&0), "the oldest block goes first");

        // Each frame the list measures row 0, then renders the rows in view.
        // Neither may push out the other, or both are fetched again forever.
        for _ in 0..3 {
            data.claim_missing_blocks(0..1, usize::MAX);
            data.claim_missing_blocks(far..far + 30, usize::MAX);
        }
        assert!(data.claim_missing_blocks(0..1, usize::MAX).is_empty());
        assert!(
            data.claim_missing_blocks(far..far + 30, usize::MAX)
                .is_empty()
        );
    }

    #[test]
    fn test_column_pages() {
        let mut data = data(10, 120);
        assert_eq!(data.page_columns(), 0..50);
        assert!(!data.can_page(false));
        assert!(data.page(true));
        assert_eq!(data.page_columns(), 50..100);
        assert!(data.page(true));
        assert_eq!(data.page_columns(), 100..120);
        assert!(!data.page(true));
        assert!(data.page(false));
        assert_eq!(data.page_columns(), 50..100);
    }

    #[test]
    fn test_sort_cycles() {
        let mut data = data(10, 3);
        let ascending = data.next_sort(1);
        assert_eq!(
            ascending,
            Some(ColumnSortKey {
                column_index: 1,
                ascending: true
            })
        );
        data.sort = ascending;
        assert_eq!(
            data.next_sort(1),
            Some(ColumnSortKey {
                column_index: 1,
                ascending: false
            })
        );
        data.sort = data.next_sort(1);
        assert_eq!(data.next_sort(1), None);
        assert_eq!(
            data.next_sort(2),
            Some(ColumnSortKey {
                column_index: 2,
                ascending: true
            })
        );
    }

    #[test]
    fn test_format_count() {
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(10_000_000), "10,000,000");
    }
}
