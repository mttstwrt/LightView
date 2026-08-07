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

            let mut groups = Vec::new();
            for &(min, max, label) in &buckets {
                let matching: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.file_size >= min && item.file_size < max)
                    .map(|(i, _)| i)
                    .collect();

                if !matching.is_empty() {
                    groups.push(GroupHeader {
                        label: label.to_string(),
                        start_index: matching[0],
                        count: matching.len(),
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

fn group_by_time(items: &[SortedItem], granularity: &Granularity) -> Vec<GroupHeader> {
    let mut groups = Vec::new();
    let mut last_label = String::new();
    let mut start = 0;
    let mut count = 0;

    for (i, item) in items.iter().enumerate() {
        let label = match item.date_taken {
            Some(ts) => {
                let dt = chrono::DateTime::from_timestamp(ts, 0);
                dt.map(|d| match granularity {
                    Granularity::Day => d.format("%B %e, %Y").to_string(),
                    Granularity::Month => d.format("%B %Y").to_string(),
                    Granularity::Year => d.format("%Y").to_string(),
                })
                .unwrap_or_else(|| "Unknown date".to_string())
            }
            None => "Unknown date".to_string(),
        };

        if label != last_label {
            if count > 0 {
                groups.push(GroupHeader {
                    label: last_label.clone(),
                    start_index: start,
                    count,
                });
            }
            last_label = label;
            start = i;
            count = 0;
        }
        count += 1;
    }

    if count > 0 {
        groups.push(GroupHeader {
            label: last_label,
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
