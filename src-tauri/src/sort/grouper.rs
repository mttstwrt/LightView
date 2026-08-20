//! Group headers over a sorted item list.
//!
//! Pure and in-memory: takes the already-ordered items and returns
//! `{label, start_index, count}` spans wherever the group key changes. It never
//! sorts, so the grid can regroup without a round-trip to the database.

use serde::{Deserialize, Serialize};

use crate::sort::sorter::SortedItem;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupBy {
    TimePeriod { granularity: Granularity },
    MediaType,
    SizeRange,
    Tag { namespace: String, tag_prefix: String },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    Day,
    Month,
    Year,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupHeader {
    pub label: String,
    pub start_index: usize,
    pub count: usize,
}

/// Compute group headers for a sorted list of items.
pub fn compute_groups(items: &[SortedItem], group_by: &GroupBy) -> Vec<GroupHeader> {
    match group_by {
        GroupBy::None => Vec::new(),

        GroupBy::TimePeriod { granularity } => group_by_time(items, granularity),

        GroupBy::MediaType => {
            let mut groups = Vec::new();
            let mut last_type = String::new();
            let mut start = 0;
            let mut count = 0;

            for (i, item) in items.iter().enumerate() {
                if item.media_type != last_type {
                    if count > 0 {
                        groups.push(GroupHeader {
                            label: format_media_type(&last_type),
                            start_index: start,
                            count,
                        });
                    }
                    last_type = item.media_type.clone();
                    start = i;
                    count = 0;
                }
                count += 1;
            }
            if count > 0 {
                groups.push(GroupHeader {
                    label: format_media_type(&last_type),
                    start_index: start,
                    count,
                });
            }
            groups
        }

        GroupBy::SizeRange => {
            let buckets = [
                (0, 1_000_000, "< 1 MB"),
                (1_000_000, 10_000_000, "1 - 10 MB"),
                (10_000_000, 100_000_000, "10 - 100 MB"),
                (100_000_000, 1_000_000_000, "100 MB - 1 GB"),
                (1_000_000_000, i64::MAX, "> 1 GB"),
            ];

            // Only the first index and the tally are ever read, so count in
            // place rather than collecting every matching index into a vector
            // that is then thrown away — five allocations sized by the gallery,
            // to produce at most five small headers.
            let mut groups = Vec::new();
            for &(min, max, label) in &buckets {
                let mut first = None;
                let mut count = 0;
                for (i, item) in items.iter().enumerate() {
                    if item.file_size >= min && item.file_size < max {
                        first.get_or_insert(i);
                        count += 1;
                    }
                }
                if let Some(start_index) = first {
                    groups.push(GroupHeader {
                        label: label.to_string(),
                        start_index,
                        count,
                    });
                }
            }
            groups
        }

        GroupBy::Tag { .. } => {
            // Tag-based grouping requires querying the tag_index.
            // This is handled at a higher level in the commands layer.
            Vec::new()
        }
    }
}

/// The period an item belongs to, as a cheap comparable value. `None` covers
/// both a missing timestamp and one `chrono` cannot represent — the two the
/// label prints identically as "Unknown date", so they group together exactly
/// as comparing labels did.
type PeriodKey = Option<(i32, u32, u32)>;

fn period_key(ts: Option<i64>, granularity: &Granularity) -> PeriodKey {
    use chrono::Datelike;
    let d = chrono::DateTime::from_timestamp(ts?, 0)?;
    Some(match granularity {
        Granularity::Day => (d.year(), d.month(), d.day()),
        Granularity::Month => (d.year(), d.month(), 0),
        Granularity::Year => (d.year(), 0, 0),
    })
}

fn period_label(ts: Option<i64>, granularity: &Granularity) -> String {
    let Some(d) = ts.and_then(|ts| chrono::DateTime::from_timestamp(ts, 0)) else {
        return "Unknown date".to_string();
    };
    match granularity {
        Granularity::Day => d.format("%B %e, %Y").to_string(),
        Granularity::Month => d.format("%B %Y").to_string(),
        Granularity::Year => d.format("%Y").to_string(),
    }
}

/// Split the (already ordered) items wherever the period changes.
///
/// The split is decided on [`PeriodKey`], not on the rendered label, because
/// formatting is the expensive half — a `strftime` pass and a `String` per call
/// — and a hundred-thousand-item gallery holds at most a few hundred distinct
/// periods. Comparing labels meant formatting every item to discover that
/// almost all of them belonged to the group already open. The key is a triple
/// of integers, so a run costs one comparison per item and one format per
/// group.
fn group_by_time(items: &[SortedItem], granularity: &Granularity) -> Vec<GroupHeader> {
    let mut groups = Vec::new();
    let mut key: PeriodKey = None;
    let mut label = String::new();
    let mut start = 0;
    let mut count = 0;

    for (i, item) in items.iter().enumerate() {
        let item_key = period_key(item.date_taken, granularity);
        if count == 0 || item_key != key {
            if count > 0 {
                groups.push(GroupHeader {
                    label: std::mem::take(&mut label),
                    start_index: start,
                    count,
                });
            }
            key = item_key;
            label = period_label(item.date_taken, granularity);
            start = i;
            count = 0;
        }
        count += 1;
    }

    if count > 0 {
        groups.push(GroupHeader {
            label,
            start_index: start,
            count,
        });
    }

    groups
}

fn format_media_type(t: &str) -> String {
    match t {
        "image" => "Images".to_string(),
        "video" => "Videos".to_string(),
        "gif" => "GIFs".to_string(),
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-03-05T00:00:00Z, 2024-03-20, 2024-11-01, then an undated item.
    fn items() -> Vec<SortedItem> {
        [Some(1709596800), Some(1710892800), Some(1730419200), None]
            .into_iter()
            .enumerate()
            .map(|(i, date_taken)| SortedItem {
                path: format!("/g/{i}.jpg"),
                date_taken,
                file_size: 1,
                media_type: "image".to_string(),
                rating: None,
                color_label: None,
                last_viewed: None,
                date_added: None,
                last_rated: None,
                duration: None,
                width: None,
                height: None,
                thumbhash: None,
            })
            .collect()
    }

    fn spans(groups: &[GroupHeader]) -> Vec<(&str, usize, usize)> {
        groups
            .iter()
            .map(|g| (g.label.as_str(), g.start_index, g.count))
            .collect()
    }

    /// Grouping splits on the period, not on the rendered label — the label is
    /// formatted once per group rather than once per item. The two must agree,
    /// so these pin the spans and the exact strings.
    #[test]
    fn time_periods_span_their_runs() {
        let items = items();

        let by_month = compute_groups(
            &items,
            &GroupBy::TimePeriod { granularity: Granularity::Month },
        );
        assert_eq!(
            spans(&by_month),
            [("March 2024", 0, 2), ("November 2024", 2, 1), ("Unknown date", 3, 1)]
        );

        let by_year = compute_groups(
            &items,
            &GroupBy::TimePeriod { granularity: Granularity::Year },
        );
        assert_eq!(spans(&by_year), [("2024", 0, 3), ("Unknown date", 3, 1)]);

        let by_day = compute_groups(
            &items,
            &GroupBy::TimePeriod { granularity: Granularity::Day },
        );
        assert_eq!(by_day.len(), 4, "each item is its own day");
        assert_eq!(by_day[0].label, "March  5, 2024");
    }

    /// A period that recurs after an interruption opens a new group, exactly as
    /// comparing consecutive labels did.
    #[test]
    fn a_repeated_period_is_not_merged_across_a_gap() {
        let mut items = items();
        items.push(items[0].clone());

        let groups = compute_groups(
            &items,
            &GroupBy::TimePeriod { granularity: Granularity::Month },
        );
        assert_eq!(
            spans(&groups),
            [
                ("March 2024", 0, 2),
                ("November 2024", 2, 1),
                ("Unknown date", 3, 1),
                ("March 2024", 4, 1),
            ]
        );
    }

    /// Size buckets report the first matching index and the tally; unmatched
    /// buckets produce no header at all.
    #[test]
    fn size_buckets_report_first_index_and_count() {
        let mut items = items();
        items[0].file_size = 500_000;
        items[1].file_size = 5_000_000;
        items[2].file_size = 6_000_000;
        items[3].file_size = 900_000;

        let groups = compute_groups(&items, &GroupBy::SizeRange);
        assert_eq!(spans(&groups), [("< 1 MB", 0, 2), ("1 - 10 MB", 1, 2)]);
    }
}
