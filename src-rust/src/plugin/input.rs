//! What a plugin is fed, and how several answers become one.
//!
//! **The host decides input, and a plugin never sees a video.** A still becomes
//! one request; a clip becomes `video_frames` stills sampled across it, sent as
//! ordinary requests and merged afterwards. Every executor shares this
//! machinery, so a plugin cannot behave differently depending on where it ran —
//! and a plugin author never writes frame extraction, which is where the three
//! bundled taggers each had their own slightly different version.
//!
//! **Input is quantized up to a cached tier edge.** A plugin declares the
//! longest edge it wants and the host serves the smallest tier at least that
//! big — **rounding up, never down**: a model handed a smaller image than it
//! trained on has lost information it cannot recover. Above the largest tier
//! there is nothing to round up to, so the source is decoded directly.
//!
//! The payoff is conditional and the condition is worth stating where a plugin
//! author will read it: **a plugin declaring an edge above the warmed tier pays
//! one generation per image.** The idle worker warms `j` (512), which is why
//! the bundled manifests declare 512. If a tagger genuinely needs `jm`, the
//! answer is to warm `jm` for that gallery, not to absorb the decode silently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::cache::tiers::ThumbTier;
use crate::path::RelPath;
use crate::pipeline::serve::{Outcome, ThumbService};
use crate::pipeline::{thumbnailer, video};

/// Tags whose value is a single choice rather than a member of a set.
///
/// A plugin picks one per image from confidences the host never sees, so a
/// union across a clip's frames would hand back three contradictory ratings.
/// See [`merge`].
const CHOICE_PREFIX: &str = "rating:";

/// One image as a plugin will see it: a file on disk, and the name a result is
/// matched back on.
#[derive(Debug, Clone)]
pub struct Part {
    /// The temp file's **name**, not its path.
    ///
    /// A plugin that canonicalizes its input under a symlinked `TMPDIR` echoes
    /// back a different string for the same file, so matching on the full path
    /// silently drops every result. Keying on the name makes directory-level
    /// rewriting harmless.
    pub name: String,
    pub path: PathBuf,
}

/// What a plugin returned for one part.
#[derive(Debug, Clone, Default)]
pub struct PartResult {
    pub tags: Vec<String>,
    pub meta: Option<Value>,
    pub error: Option<String>,
}

/// One item's answer, after its parts are merged.
#[derive(Debug, Clone, Default)]
pub struct MergedItem {
    pub tags: Vec<String>,
    pub meta: Option<Value>,
    /// Set when every part failed. A partial failure is not an error: four
    /// frames out of five is a perfectly good answer for a clip.
    pub error: Option<String>,
}

/// Bytes for one request, at the edge the plugin asked for.
///
/// Returns `None` when the source cannot be read or decoded — a broken file
/// should cost itself its tags and nothing else.
pub async fn image_bytes(
    thumbs: &Arc<ThumbService>,
    path: &RelPath,
    max_edge: u32,
) -> Option<Vec<u8>> {
    match ThumbTier::smallest_at_least(max_edge) {
        // The ordinary path: the plugin reads the same cached bytes a browser
        // does, generating them only if they are not there yet.
        Some(tier) => match thumbs.get_or_generate(tier, path, false).await {
            Outcome::Hit(bytes) => Some(bytes),
            _ => None,
        },
        // Above the largest tier there is nothing to round up to.
        None => {
            let absolute = thumbs.root.resolve(path).ok()?.as_path().to_path_buf();
            tokio::task::spawn_blocking(move || {
                let filter = thumbnailer::filter_for_size(max_edge);
                thumbnailer::generate_for_path_fit(&absolute, filter, max_edge)
                    .ok()
                    .map(|t| t.data)
            })
            .await
            .ok()
            .flatten()
        }
    }
}

/// Sample a clip into `frames` stills at `max_edge`, as WebP.
///
/// Frames are the one irreducible exception to tier quantization: a frame at a
/// timestamp is not a tier, so these are always generated. A clip that cannot
/// be probed or decoded yields nothing and the item is skipped.
pub fn video_parts(absolute: &Path, max_edge: u32, frames: u32) -> Vec<Vec<u8>> {
    let Ok(info) = video::probe(absolute) else {
        return Vec::new();
    };
    let stamps = video::sample_timestamps(info.duration.unwrap_or(0.0), frames);
    // A clip with no usable duration still deserves one look at it.
    let stamps = if stamps.is_empty() { vec![0.0] } else { stamps };

    let mut out = Vec::new();
    for at in stamps {
        let Ok(frame) = video::extract_frame_at(absolute, max_edge, Some(at)) else {
            continue;
        };
        // `fit_rgba` encodes as WebP directly — the same encoder and the same
        // filter choice the cached tiers use, so a frame is not subtly a
        // different kind of image from a still.
        let filter = thumbnailer::filter_for_size(max_edge);
        if let Ok(bytes) =
            thumbnailer::fit_rgba(&frame.rgba, frame.width, frame.height, max_edge, filter)
        {
            out.push(bytes);
        }
    }
    out
}

/// Fold one item's part results into a single answer.
///
/// Tags are a **union**: two frames of the same clip seeing a dog and a beach
/// mean the clip has both. Choice-valued tags are the exception — see
/// [`CHOICE_PREFIX`] — and get a **redone argmax**: the value the most parts
/// agreed on, with the first-seen winning a tie, because the confidences the
/// plugin used for its own argmax never crossed the wire.
pub fn merge(parts: &[PartResult]) -> MergedItem {
    if parts.iter().all(|p| p.error.is_some()) {
        return MergedItem {
            error: Some(
                parts
                    .iter()
                    .find_map(|p| p.error.clone())
                    .unwrap_or_else(|| "no result".to_string()),
            ),
            ..Default::default()
        };
    }

    let mut tags: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Insertion order is kept so the winner of a tie is the first one seen.
    let mut choices: Vec<String> = Vec::new();
    let mut votes: HashMap<&str, usize> = HashMap::new();

    for part in parts {
        for tag in &part.tags {
            if tag.starts_with(CHOICE_PREFIX) {
                if !choices.iter().any(|c| c == tag) {
                    choices.push(tag.clone());
                }
            } else if seen.insert(tag.clone()) {
                tags.push(tag.clone());
            }
        }
    }
    for part in parts {
        for tag in &part.tags {
            if let Some(choice) = choices.iter().find(|c| *c == tag) {
                *votes.entry(choice.as_str()).or_default() += 1;
            }
        }
    }
    if let Some(winner) = choices.iter().max_by_key(|c| {
        // Negated position breaks a tie towards the first seen, since
        // `max_by_key` keeps the *last* maximum.
        let pos = choices.iter().position(|x| x == *c).unwrap_or(0);
        (votes.get(c.as_str()).copied().unwrap_or(0), usize::MAX - pos)
    }) {
        tags.push(winner.clone());
    }

    MergedItem {
        tags,
        meta: parts.iter().find_map(|p| p.meta.clone()),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(tags: &[&str]) -> PartResult {
        PartResult {
            tags: tags.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn err(message: &str) -> PartResult {
        PartResult {
            error: Some(message.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_single_part_passes_through() {
        let merged = merge(&[ok(&["dog", "beach"])]);
        assert_eq!(merged.tags, vec!["dog", "beach"]);
        assert!(merged.error.is_none());
    }

    #[test]
    fn frames_union_their_ordinary_tags() {
        let merged = merge(&[ok(&["dog", "beach"]), ok(&["beach", "sunset"])]);
        assert_eq!(merged.tags, vec!["dog", "beach", "sunset"]);
    }

    #[test]
    fn a_rating_is_one_choice_rather_than_a_set() {
        // The failure this exists to prevent: a five-frame clip coming back
        // tagged safe *and* questionable *and* explicit.
        let merged = merge(&[
            ok(&["rating:safe", "dog"]),
            ok(&["rating:questionable"]),
            ok(&["rating:safe"]),
        ]);
        assert_eq!(merged.tags, vec!["dog", "rating:safe"]);
    }

    #[test]
    fn a_tied_rating_keeps_the_first_frame_that_said_it() {
        let merged = merge(&[ok(&["rating:safe"]), ok(&["rating:explicit"])]);
        assert_eq!(merged.tags, vec!["rating:safe"]);
    }

    #[test]
    fn one_bad_frame_does_not_lose_a_clip_its_tags() {
        let merged = merge(&[err("decode failed"), ok(&["dog"])]);
        assert_eq!(merged.tags, vec!["dog"]);
        assert!(merged.error.is_none());
    }

    #[test]
    fn an_item_whose_every_part_failed_is_an_error() {
        let merged = merge(&[err("a"), err("b")]);
        assert_eq!(merged.error.as_deref(), Some("a"));
        assert!(merged.tags.is_empty());
    }
}
