use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::cache::counts::TagCount;

/// A tag suggestion returned by the autocomplete engine.
#[derive(Debug, Clone, Serialize)]
pub struct TagSuggestion {
    pub namespace: String,
    pub tag: String,
    pub count: u32,
    pub score: i64,
}

/// In-memory autocomplete engine that holds all unique tags for fast fuzzy matching.
/// At ~5,000 unique tags this uses ~300 KB — trivially small.
pub struct AutocompleteEngine {
    tags: Arc<RwLock<Vec<TagCount>>>,
}

impl AutocompleteEngine {
    pub fn new() -> Self {
        Self {
            tags: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Reload the tag cache from the database.
    pub async fn refresh(&self, tag_counts: Vec<TagCount>) {
        let mut tags = self.tags.write().await;
        *tags = tag_counts;
    }

    /// Query tags with fuzzy substring matching.
    /// Returns up to `limit` results sorted by match quality then frequency.
    ///
    /// Also returns namespace suggestions (user, auto, plugin.*) when the
    /// input matches a known namespace name.
    pub async fn query(
        &self,
        input: &str,
        namespace: Option<&str>,
        limit: usize,
    ) -> Vec<TagSuggestion> {
        let tags = self.tags.read().await;
        let input_lower = input.to_lowercase();

        let mut results: Vec<TagSuggestion> = tags
            .iter()
            .filter(|e| namespace.map_or(true, |ns| e.namespace == ns))
            .filter_map(|e| {
                let tag_lower = e.tag.to_lowercase();
                fuzzy_score(&tag_lower, &input_lower).map(|score| TagSuggestion {
                    namespace: e.namespace.clone(),
                    tag: e.tag.clone(),
                    count: e.count,
                    score,
                })
            })
            .collect();

        // Add namespace suggestions when input matches a namespace name.
        // These use a special namespace marker "_namespace" so the frontend
        // can distinguish them from regular tag suggestions.
        if namespace.is_none() {
            let ns_names = self.unique_namespaces(&tags);
            for ns in &ns_names {
                if let Some(score) = fuzzy_score(&ns.to_lowercase(), &input_lower) {
                    // Count how many tags exist in this namespace
                    let ns_count: u32 = tags
                        .iter()
                        .filter(|e| e.namespace == *ns)
                        .map(|e| e.count)
                        .sum();
                    results.push(TagSuggestion {
                        namespace: "_namespace".to_string(),
                        tag: ns.clone(),
                        count: ns_count,
                        score: score + 50, // Boost namespace suggestions slightly
                    });
                }
            }
        }

        // Sort by score descending, then count descending as tiebreaker
        results.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.count.cmp(&a.count))
        });
        results.truncate(limit);
        results
    }

    /// Extract unique namespace names from the tag cache.
    fn unique_namespaces(&self, tags: &[TagCount]) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut names = Vec::new();
        for tc in tags {
            if seen.insert(tc.namespace.clone()) {
                names.push(tc.namespace.clone());
            }
        }
        names
    }

    /// Get the total number of unique tags in the cache.
    pub async fn tag_count(&self) -> usize {
        self.tags.read().await.len()
    }
}

/// Simple fuzzy scoring: prefix match > contains > no match.
/// Returns None for no match, or a score where higher is better.
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() {
        // Empty query matches everything; score by nothing (all equal)
        return Some(0);
    }

    if haystack == needle {
        // Exact match: highest score
        return Some(1000);
    }

    if haystack.starts_with(needle) {
        // Prefix match: high score, bonus for shorter haystack (more specific match)
        let len_bonus = 100 - haystack.len().min(100) as i64;
        return Some(500 + len_bonus);
    }

    if haystack.contains(needle) {
        // Substring match: medium score
        let pos = haystack.find(needle).unwrap_or(0) as i64;
        return Some(200 - pos.min(200));
    }

    // Check if all characters of needle appear in haystack in order (subsequence)
    let mut hay_chars = haystack.chars();
    let mut matched = 0;
    for nc in needle.chars() {
        while let Some(hc) = hay_chars.next() {
            if hc == nc {
                matched += 1;
                break;
            }
        }
    }
    if matched == needle.len() {
        Some(50)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_exact() {
        assert_eq!(fuzzy_score("vacation", "vacation"), Some(1000));
    }

    #[test]
    fn test_fuzzy_prefix() {
        let score = fuzzy_score("vacation", "vac").unwrap();
        assert!(score > 400);
    }

    #[test]
    fn test_fuzzy_substring() {
        let score = fuzzy_score("paris", "ari").unwrap();
        assert!(score > 100 && score < 400);
    }

    #[test]
    fn test_fuzzy_no_match() {
        assert_eq!(fuzzy_score("vacation", "xyz"), None);
    }

    #[test]
    fn test_fuzzy_subsequence() {
        let score = fuzzy_score("vacation", "vcn").unwrap();
        assert!(score > 0 && score < 100);
    }
}
