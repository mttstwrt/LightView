use serde::Serialize;

use crate::cache::db::{CacheDb, CacheError};

/// A single entry in the timeline index, mapping a row position to a date.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineEntry {
    pub row_index: usize,
    pub date: i64,        // Unix timestamp
    pub label: String,    // Pre-formatted: "Mar 2026", "Dec 14", etc.
}

impl CacheDb {
    /// Compute the timeline index from media_meta, ordered by date descending.
    /// Returns entries at each month boundary for use in the scrollbar.
    pub fn compute_timeline(
        &self,
        items_per_row: usize,
    ) -> Result<Vec<TimelineEntry>, CacheError> {
        let mut stmt = self.conn().prepare(
            "SELECT date_taken FROM media_meta 
             WHERE date_taken IS NOT NULL 
             ORDER BY date_taken DESC",
        )?;

        let dates: Vec<i64> = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();

        if dates.is_empty() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        let mut last_month = String::new();

        for (i, &timestamp) in dates.iter().enumerate() {
            let dt = chrono::DateTime::from_timestamp(timestamp, 0);
            let month_label = dt
                .map(|d| d.format("%b %Y").to_string())
                .unwrap_or_else(|| "Unknown".to_string());

            if month_label != last_month {
                let row_index = i / items_per_row;
                entries.push(TimelineEntry {
                    row_index,
                    date: timestamp,
                    label: month_label.clone(),
                });
                last_month = month_label;
            }
        }

        Ok(entries)
    }
}
