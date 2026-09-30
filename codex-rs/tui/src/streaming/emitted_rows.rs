//! Rows shared between an agent stream and the history cells it emitted.
//!
//! A terminal palette change re-renders the stream in the new syntax theme. Emitted cells read
//! their rows from these buffers, so the restyle reaches them wherever they are held or queued.

use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;

use crate::terminal_hyperlinks::HyperlinkLine;

#[derive(Default)]
pub(super) struct EmittedRows {
    cells: Vec<Arc<RwLock<Vec<HyperlinkLine>>>>,
}

impl EmittedRows {
    pub(super) fn push(&mut self, rows: Vec<HyperlinkLine>) -> Arc<RwLock<Vec<HyperlinkLine>>> {
        let rows = Arc::new(RwLock::new(rows));
        self.cells.push(rows.clone());
        rows
    }

    /// Stops at the first cell whose text differs, such as rows emitted before a mid-stream
    /// resize, which keep their styles until consolidation. Equal text keeps cell heights valid.
    pub(super) fn restyle(&self, mut rows: &[HyperlinkLine]) {
        let text = |line: &HyperlinkLine| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        for cell in &self.cells {
            let mut lines = cell.write().unwrap_or_else(PoisonError::into_inner);
            let Some((restyled, rest)) = rows.split_at_checked(lines.len()) else {
                return;
            };
            if !restyled.iter().map(text).eq(lines.iter().map(text)) {
                return;
            }
            lines.clone_from_slice(restyled);
            rows = rest;
        }
    }

    pub(super) fn clear(&mut self) {
        self.cells.clear();
    }
}

#[cfg(test)]
#[path = "emitted_rows_tests.rs"]
mod tests;
