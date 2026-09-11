//! Finding near-identical files, and folding a group onto one survivor.
//!
//! Detection is [`crate::cache::duplicates`]; this is the service around it —
//! the part that decides *when* to run the quadratic loop and what a merge
//! does.
//!
//! **"Not a duplicate" is not stored.** Two files sharing any `set::` tag are
//! never offered as a pair. Forty burst frames cost forty tag rows instead of
//! 780 pairwise verdicts, and the user sees a name rather than a list of
//! negations. The accepted cost is stated where it bites: two identical scans
//! inside a 200-page comic will not be found, because they share the comic's
//! set.
//!
//! **A merge unions `set::` tags onto the keeper**, like user tags. Without it,
//! merging a set member silently drops that member from its set. A keeper
//! ending up in two sets is fine — suppression is pairwise co-membership, so
//! two sets do not become one through it.
//!
//! **`auto` is not unioned.** The merge used to union `tags.auto` across every
//! copy onto the survivor, which made it the one writer of a namespace nothing
//! else created. The namespace is gone from the index, so the union goes with
//! it — and so does the test that asserted it.
//!
//! **Image bytes are never rewritten.** There is no EXIF write path in this
//! system. GPS is the one field that can be promoted into the keeper, precisely
//! because doing so touches the companion rather than the photo.

use serde::{Deserialize, Serialize};

use crate::cache::duplicates as finder;
use crate::companion::schema::{CompanionFile, Location, MediaType, PluginTagEntry};
use crate::companion::writer::{modify_companion, Outcome};
use crate::path::RelPath;
use crate::state::Gallery;

/// Hamming distance at which two hashes count as a match. A parameter for
/// precision, not for cost: a tighter threshold is not cheaper.
pub const DEFAULT_THRESHOLD: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum DuplicateError {
    #[error(transparent)]
    Cache(#[from] crate::cache::db::CacheError),
    #[error(transparent)]
    Path(#[from] crate::path::PathError),
    #[error(transparent)]
    Trash(#[from] crate::services::trash::TrashError),
    #[error(transparent)]
    Write(#[from] crate::companion::writer::WriteError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the finder.
///
/// Loads under the read pool, releases it, and runs the quadratic loop on a
/// blocking thread with **no connection held**. The version this replaces held
/// the writer across the entire all-pairs comparison, on an async worker with
/// no `spawn_blocking` at all.
pub async fn find(
    gallery: &Gallery,
    threshold: u32,
) -> Result<Vec<finder::DuplicateGroup>, DuplicateError> {
    let input = {
        let conn = gallery.db.read().await;
        finder::load(&conn)?
    };
    let groups = tokio::task::spawn_blocking(move || finder::group(&input, threshold))
        .await
        .map_err(|e| DuplicateError::Io(std::io::Error::other(e.to_string())))?;
    Ok(groups)
}

/// A fully-resolved merge. The dialog resolves the conflicts; the backend
/// applies the answer and does not second-guess it.
#[derive(Debug, Clone, Deserialize)]
pub struct MergePlan {
    pub keeper: RelPath,
    /// Everything else in the group. These are trashed, not deleted.
    pub others: Vec<RelPath>,
    #[serde(default)]
    pub rating: Option<u8>,
    #[serde(default)]
    pub color_label: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Promoted into the keeper's companion when a copy had coordinates it
    /// lacked. The photo's own bytes are never touched.
    #[serde(default)]
    pub location: Option<Location>,
    /// Stamped onto the keeper's file with `filetime`, so the survivor carries
    /// the capture time the group agreed on rather than the copy date.
    #[serde(default)]
    pub mtime: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MergeResult {
    pub keeper: RelPath,
    pub trashed: usize,
    /// The trash entry the non-keepers went into, so one merge is one undo.
    pub trash_entry: String,
}

/// Fold a group onto its keeper and trash the rest.
pub async fn merge(gallery: &Gallery, plan: MergePlan) -> Result<MergeResult, DuplicateError> {
    // Gather what the others contribute, before anything moves.
    let mut contributed = Contributions::default();
    for other in &plan.others {
        let absolute = gallery.root.resolve(other)?;
        let companion = tokio::task::spawn_blocking(move || {
            crate::companion::reader::read_companion(absolute.as_path())
        })
        .await
        .map_err(|e| DuplicateError::Io(std::io::Error::other(e.to_string())))?;
        if let Ok(Some(companion)) = companion {
            contributed.absorb(&companion);
        }
    }

    // Apply everything to the keeper in one locked read-modify-write.
    let keeper_path = gallery.root.resolve(&plan.keeper)?;
    let plan_for_write = plan.clone();
    tokio::task::spawn_blocking(move || {
        modify_companion(
            keeper_path.as_path(),
            MediaType::Image,
            |companion: &mut CompanionFile| {
                contributed.apply(companion);
                let mut core = companion.meta.core.take().unwrap_or_default();
                core.rating = plan_for_write.rating;
                core.color_label = plan_for_write
                    .color_label
                    .as_ref()
                    .map(|c| c.trim().to_lowercase());
                core.notes = plan_for_write.notes.clone();
                if plan_for_write.location.is_some() {
                    core.location = plan_for_write.location;
                }
                companion.meta.core = Some(core);
                Outcome::Write(())
            },
        )
    })
    .await
    .map_err(|e| DuplicateError::Io(std::io::Error::other(e.to_string())))??;

    // The survivor's mtime is the group's agreed capture time. Restoring a file
    // with a rewritten mtime is silent data loss, which is why this is the one
    // place that writes one and why it is an explicit field of the plan.
    if let Some(mtime) = plan.mtime {
        let keeper = gallery.root.resolve(&plan.keeper)?;
        let stamp = filetime::FileTime::from_unix_time(mtime, 0);
        filetime::set_file_mtime(keeper.as_path(), stamp)?;
    }

    let trash_entry = {
        let root = gallery.root.clone();
        let others = plan.others.clone();
        tokio::task::spawn_blocking(move || crate::services::trash::move_to_trash(&root, &others))
            .await
            .map_err(|e| DuplicateError::Io(std::io::Error::other(e.to_string())))??
    };

    {
        let conn = gallery.db.writer().await;
        for other in &plan.others {
            crate::cache::db::forget_path(&conn, other)?;
        }
    }
    crate::services::gallery::index_one(gallery, &plan.keeper)
        .await
        .map_err(|e| DuplicateError::Io(std::io::Error::other(e.to_string())))?;
    gallery.refresh_autocomplete().await;
    gallery.events.send(crate::server::events::Event::FsChanged {
        added: Vec::new(),
        removed: plan.others.clone(),
    });

    Ok(MergeResult {
        keeper: plan.keeper,
        trashed: plan.others.len(),
        trash_entry,
    })
}

/// What the non-keepers contribute to the survivor.
#[derive(Debug, Default)]
struct Contributions {
    user: Vec<String>,
    set: Vec<String>,
    plugins: std::collections::HashMap<String, PluginTagEntry>,
}

impl Contributions {
    fn absorb(&mut self, companion: &CompanionFile) {
        self.user.extend(companion.tags.user.iter().cloned());
        // Without this, merging a set member silently drops that member's set.
        self.set.extend(companion.tags.set.iter().cloned());
        for (name, entry) in &companion.tags.plugins {
            // Keep the newest bucket per plugin; a bucket is replaced
            // wholesale by its own run, so mixing two is not meaningful.
            self.plugins
                .entry(name.clone())
                .and_modify(|existing| {
                    if entry.version > existing.version {
                        *existing = entry.clone();
                    }
                })
                .or_insert_with(|| entry.clone());
        }
        // `tags.auto` is deliberately not absorbed. It was the one writer of a
        // namespace nothing else created, and the namespace is gone.
    }

    fn apply(self, companion: &mut CompanionFile) {
        merge_into(&mut companion.tags.user, self.user);
        merge_into(&mut companion.tags.set, self.set);
        for (name, entry) in self.plugins {
            companion
                .tags
                .plugins
                .entry(name)
                .or_insert(entry);
        }
    }
}

fn merge_into(target: &mut Vec<String>, extra: Vec<String>) {
    target.extend(extra);
    target.sort();
    target.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tags(user: &[&str], set: &[&str]) -> CompanionFile {
        let mut c = CompanionFile::new("x.jpg", MediaType::Image);
        c.tags.user = user.iter().map(|s| s.to_string()).collect();
        c.tags.set = set.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn user_and_set_tags_both_union_onto_the_keeper() {
        let mut keeper = with_tags(&["beach"], &["holiday"]);
        let mut contributions = Contributions::default();
        contributions.absorb(&with_tags(&["beach", "sunset"], &["burst-3"]));
        contributions.absorb(&with_tags(&["family"], &[]));
        contributions.apply(&mut keeper);

        assert_eq!(keeper.tags.user, vec!["beach", "family", "sunset"]);
        // A keeper in two sets is fine: suppression is pairwise co-membership,
        // so two sets do not become one through it.
        assert_eq!(keeper.tags.set, vec!["burst-3", "holiday"]);
    }

    #[test]
    fn auto_tags_are_not_unioned_and_are_not_destroyed() {
        // The merge was the only writer of `tags.auto`. The union goes with the
        // namespace — but the field still round-trips through the flattened
        // extras, so a merge does not delete what an old sidecar carries.
        let raw = r#"{"schema_version":1,"file":"a.jpg","file_hash":"",
            "media_type":"image","created":"","modified":"",
            "tags":{"user":["u"],"auto":["indoor"],"plugins":{}},
            "meta":{"core":null,"plugins":{}}}"#;
        let other: CompanionFile = serde_json::from_str(raw).unwrap();
        let mut keeper: CompanionFile = serde_json::from_str(raw).unwrap();

        let mut contributions = Contributions::default();
        contributions.absorb(&other);
        contributions.apply(&mut keeper);

        let written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&keeper).unwrap()).unwrap();
        assert_eq!(written["tags"]["auto"], serde_json::json!(["indoor"]));
        assert_eq!(written["tags"]["user"], serde_json::json!(["u"]));
    }

    #[test]
    fn a_plugin_bucket_is_taken_whole_at_its_newest_version() {
        // A bucket is replaced wholesale by its own run, so mixing two
        // versions' tags would produce output no run ever emitted.
        let mut older = CompanionFile::new("a.jpg", MediaType::Image);
        older.tags.plugins.insert(
            "wd".into(),
            PluginTagEntry {
                version: "1.0.0".into(),
                tags: vec!["old".into()],
                ..Default::default()
            },
        );
        let mut newer = CompanionFile::new("b.jpg", MediaType::Image);
        newer.tags.plugins.insert(
            "wd".into(),
            PluginTagEntry {
                version: "1.2.0".into(),
                tags: vec!["new".into()],
                ..Default::default()
            },
        );

        let mut contributions = Contributions::default();
        contributions.absorb(&older);
        contributions.absorb(&newer);

        let mut keeper = CompanionFile::new("k.jpg", MediaType::Image);
        contributions.apply(&mut keeper);
        let bucket = &keeper.tags.plugins["wd"];
        assert_eq!(bucket.version, "1.2.0");
        assert_eq!(bucket.tags, vec!["new"]);
    }

    #[test]
    fn the_keepers_own_plugin_bucket_wins_over_a_contributed_one() {
        let mut keeper = CompanionFile::new("k.jpg", MediaType::Image);
        keeper.tags.plugins.insert(
            "wd".into(),
            PluginTagEntry {
                version: "2.0.0".into(),
                tags: vec!["keeper".into()],
                ..Default::default()
            },
        );
        let mut other = CompanionFile::new("o.jpg", MediaType::Image);
        other.tags.plugins.insert(
            "wd".into(),
            PluginTagEntry {
                version: "1.0.0".into(),
                tags: vec!["other".into()],
                ..Default::default()
            },
        );

        let mut contributions = Contributions::default();
        contributions.absorb(&other);
        contributions.apply(&mut keeper);
        assert_eq!(keeper.tags.plugins["wd"].tags, vec!["keeper"]);
    }
}
