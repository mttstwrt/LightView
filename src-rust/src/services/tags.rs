//! Writing tags, ratings, colour labels and notes.
//!
//! **Every write goes through one locked read-modify-write of the companion**,
//! then mirrors into the index. There is no path that writes the database and
//! hopes the sidecar catches up, and no path that writes the sidecar without
//! taking the lock — see [`crate::companion::writer::modify_companion`].
//!
//! # The namespace is a parameter, not a parallel family
//!
//! Every tag-write operation — add, remove, the batch forms, rename, merge,
//! delete — takes a [`WritableNamespace`], which accepts `user` or `set` **and
//! nothing else**. A plugin bucket is replaced wholesale by its own run, so
//! writing into one through this path would be a second writer for something
//! with a single owner; the type has no variant for it, so such a request fails
//! to deserialize rather than reaching a check. [`order_set`] is the one
//! operation that takes no namespace: only a set has an order.
//!
//! # A set is a tag
//!
//! Set membership is one `set::` tag per member: a burst that is not forty
//! duplicates (`set::vacation-burst-3`), a work that exists as several images
//! (`set::kellys-comic`), a face cluster once someone has named it
//! (`set::alice`). The namespace parameter is nearly the entire surface sets
//! need: create is a batch add over a selection, rename is `rename`, merging
//! two clusters is `merge`, delete is `delete`, and the tag manager lists both
//! namespaces. No new file, no new table, no new filter syntax — and a set is
//! reconstructable from companions because it *is* companion content.
//!
//! **A set may carry an order.** A comic saved strip by strip does not arrive
//! as `page01.jpg`, `page02.jpg`: it is saved oldest-first one day and
//! newest-first the next, a strip published over months is saved over months,
//! and some of it lands out of place. No sort over the files recovers that, so
//! a member may carry its place in the set as a suffix on its entry,
//! `comic::3` — see [`crate::companion::schema`] for the format and why it is
//! one string rather than two fields. Two rules keep the suffix from leaking:
//!
//! - **Everything compares names.** Each operation takes its names through
//!   [`WritableNamespace::name_arg`] and matches entries by
//!   [`WritableNamespace::name_of`], so `remove`, `rename` and `delete` find
//!   `comic::3` when asked for `comic`, and `add` never enrols a member twice.
//! - **Only [`order_set`] writes a position.** `add` enrols without one.
//!   `rename` carries positions to a name no file carries yet; anything that
//!   joins a set that already exists — a merge, or a rename onto a taken name —
//!   arrives without one, because a place means nothing in an order it was not
//!   given in, and keeping it would interleave two works as ties.
//!
//! **Sets are cheap and fluid, deliberately.** Renaming one rewrites every
//! member's sidecar; trashing a member shrinks it silently and leaves a gap in
//! its order that nothing needs to close, since positions only have to sort. A
//! set is not a durable object with an identity, it is a name several files
//! agree on.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::autocomplete::engine::TagCount;
use crate::cache::{index, meta};
use crate::companion::schema::{set_entry, split_set_entry, CompanionFile, CoreMeta, MediaType};
use crate::companion::writer::{modify_companion, Outcome, WriteError};
use crate::path::{PathError, RelPath};
use crate::server::events::Event;
use crate::state::Gallery;

/// The two namespaces a person may write to.
///
/// A plugin namespace is not representable, so a request naming one fails to
/// deserialize rather than reaching a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WritableNamespace {
    User,
    Set,
}

impl WritableNamespace {
    /// The namespace as the companion and the filter language spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            WritableNamespace::User => "user",
            WritableNamespace::Set => "set",
        }
    }

    /// The companion's tag list for this namespace.
    fn field(self, companion: &mut CompanionFile) -> &mut Vec<String> {
        match self {
            WritableNamespace::User => &mut companion.tags.user,
            WritableNamespace::Set => &mut companion.tags.set,
        }
    }

    /// The name an entry in this namespace is known by: a set entry without
    /// its position, a user tag exactly as written.
    fn name_of(self, entry: &str) -> &str {
        match self {
            WritableNamespace::User => entry,
            WritableNamespace::Set => split_set_entry(entry).0,
        }
    }

    /// A name a caller sent, checked before anything is written or matched
    /// with it.
    ///
    /// A set name may not end in `::` and digits: it would be written as
    /// asked and read back as a shorter name at a position, so the file would
    /// say something other than what was written. Refusing it here is what
    /// lets [`split_set_entry`] read every entry without ambiguity. A user tag
    /// has no suffix, and any string is its own name.
    fn name_arg(self, name: &str) -> Result<&str, TagError> {
        match self {
            WritableNamespace::Set if split_set_entry(name).1.is_some() => {
                Err(TagError::InvalidSetName(name.to_string()))
            }
            _ => Ok(name),
        }
    }

    /// Every name in `names`, each checked by [`Self::name_arg`].
    fn name_args(self, names: &[String]) -> Result<Vec<String>, TagError> {
        names
            .iter()
            .map(|n| self.name_arg(n).map(str::to_string))
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Cache(#[from] crate::cache::db::CacheError),
    #[error("a set name may not end in `::` and digits: {0:?}")]
    InvalidSetName(String),
}

/// Add tags to a selection.
pub async fn add(
    gallery: &Gallery,
    paths: &[RelPath],
    tags: &[String],
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    // Owned, because the edit crosses a `spawn_blocking` boundary: the
    // companion lock blocks, and over the share it can block on another
    // machine, so it must never run on an async worker thread.
    let tags = namespace.name_args(tags)?;
    edit(gallery, paths, namespace, move |_, list| {
        let mut changed = false;
        for tag in &tags {
            if !list.iter().any(|t| namespace.name_of(t) == tag) {
                list.push(tag.clone());
                changed = true;
            }
        }
        changed
    })
    .await
}

/// Remove tags from a selection.
pub async fn remove(
    gallery: &Gallery,
    paths: &[RelPath],
    tags: &[String],
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let tags = namespace.name_args(tags)?;
    edit(gallery, paths, namespace, move |_, list| {
        let before = list.len();
        list.retain(|t| !tags.iter().any(|tag| tag == namespace.name_of(t)));
        list.len() != before
    })
    .await
}

/// Rename a tag everywhere it appears.
///
/// For a set this is the rename operation in full: the set *is* the name, so
/// rewriting every member's sidecar is not a side effect, it is the change —
/// and the order goes with it. Renaming onto a set that already has members is
/// a merge, and is treated as one: see [`fold_into`].
pub async fn rename(
    gallery: &Gallery,
    from: &str,
    to: &str,
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let from = namespace.name_arg(from)?.to_string();
    let to = namespace.name_arg(to)?.to_string();
    let members = members_of(gallery, &from, namespace).await?;
    let keep_positions = members_of(gallery, &to, namespace).await?.is_empty();
    let sources = vec![from];
    edit(gallery, &members, namespace, move |_, list| {
        fold_into(namespace, list, &sources, &to, keep_positions)
    })
    .await
}

/// Fold several tags into one. Merging two clusters is this.
///
/// Members arriving in the target arrive without a position; the target's own
/// members keep theirs. See [`fold_into`].
pub async fn merge(
    gallery: &Gallery,
    sources: &[String],
    target: &str,
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let sources = namespace.name_args(sources)?;
    let target = namespace.name_arg(target)?.to_string();
    let mut members = Vec::new();
    for source in &sources {
        members.extend(members_of(gallery, source, namespace).await?);
    }
    members.sort();
    members.dedup();

    edit(gallery, &members, namespace, move |_, list| {
        fold_into(namespace, list, &sources, &target, false)
    })
    .await
}

/// Rewrite every entry named in `sources` as `target`, in place, and say
/// whether anything changed.
///
/// A file already carrying `target` keeps that entry and loses the others,
/// rather than carrying the name twice. A set entry that moves keeps its
/// position only when `keep_positions` — true for a rename to a name nobody
/// has yet, which moves one whole order intact. Joining a set that already
/// exists is different: a position means nothing in an order it was not given
/// in, and two sets' positions kept side by side interleave both works as
/// ties. Such a member arrives unordered and follows the ordered ones.
fn fold_into(
    namespace: WritableNamespace,
    list: &mut Vec<String>,
    sources: &[String],
    target: &str,
    keep_positions: bool,
) -> bool {
    let mut has_target = list.iter().any(|t| namespace.name_of(t) == target);
    let mut changed = false;
    let mut folded = Vec::with_capacity(list.len());
    for entry in list.drain(..) {
        let name = namespace.name_of(&entry);
        if name == target || !sources.iter().any(|s| s == name) {
            folded.push(entry);
            continue;
        }
        changed = true;
        if has_target {
            continue;
        }
        has_target = true;
        folded.push(match namespace {
            WritableNamespace::User => target.to_string(),
            WritableNamespace::Set => {
                let position = split_set_entry(&entry).1.filter(|_| keep_positions);
                set_entry(target, position)
            }
        });
    }
    *list = folded;
    changed
}

/// Remove a tag from every file that carries it.
pub async fn delete(
    gallery: &Gallery,
    tag: &str,
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let tag = namespace.name_arg(tag)?.to_string();
    let members = members_of(gallery, &tag, namespace).await?;
    remove(gallery, &members, std::slice::from_ref(&tag), namespace).await
}

/// Put a set's members in the order given: the listed members take positions
/// 1..n in list order, and every other member loses its position.
///
/// This is the one writer of positions, and every gesture that orders a set is
/// this call with a different list — a drag sends the order it produced,
/// Reverse the shown order backwards, Lock the shown order as it stands, and
/// Clear an empty list. A file listed twice keeps its first place, and a file
/// that is not a member is skipped: a stale list can never enrol anything. A
/// member missing from the list — one another device added after the list was
/// drawn — is left unordered, after the ordered ones, rather than refused.
///
/// Positions are dense, so a move rewrites only the members between its two
/// ends: a file whose entry comes out unchanged is not written.
pub async fn order_set(
    gallery: &Gallery,
    set: &str,
    order: &[RelPath],
) -> Result<usize, TagError> {
    let set = WritableNamespace::Set.name_arg(set)?.to_string();
    let members = members_of(gallery, &set, WritableNamespace::Set).await?;

    let enrolled: HashSet<&RelPath> = members.iter().collect();
    let mut positions: HashMap<RelPath, u32> = HashMap::new();
    for path in order.iter().filter(|p| enrolled.contains(p)) {
        let next = positions.len() as u32 + 1;
        positions.entry(path.clone()).or_insert(next);
    }
    // Shared rather than copied: `edit` clones its closure once per file.
    let positions = Arc::new(positions);

    edit(gallery, &members, WritableNamespace::Set, move |path, list| {
        let placed = set_entry(&set, positions.get(path).copied());
        match list.iter_mut().find(|t| split_set_entry(t).0 == set) {
            Some(entry) if *entry != placed => {
                *entry = placed;
                true
            }
            _ => false,
        }
    })
    .await
}

/// Set or clear a rating over a selection, mirroring it into the indexed column.
pub async fn set_rating(
    gallery: &Gallery,
    paths: &[RelPath],
    rating: Option<u8>,
) -> Result<(), TagError> {
    for path in paths {
        write_core(gallery, path, move |core| {
            core.rating = rating;
            core.date_rated = Some(chrono::Utc::now().to_rfc3339());
        })
        .await?;

        let conn = gallery.db.writer().await;
        meta::set_rating(&conn, path, rating)?;
    }
    gallery.events.send(Event::ItemsChanged { paths: paths.to_vec() });
    Ok(())
}

/// Set or clear a colour label over a selection.
pub async fn set_color_label(
    gallery: &Gallery,
    paths: &[RelPath],
    label: Option<String>,
) -> Result<(), TagError> {
    let normalized = label
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty());
    for path in paths {
        let stored = normalized.clone();
        write_core(gallery, path, move |core| {
            core.color_label = stored.clone();
        })
        .await?;

        let conn = gallery.db.writer().await;
        meta::set_color_label(&conn, path, normalized.as_deref())?;
    }
    gallery.events.send(Event::ItemsChanged { paths: paths.to_vec() });
    Ok(())
}

/// Set or clear free-text notes. Not indexed — there is no `notes:` term,
/// because a field is filterable only if it is indexed.
pub async fn set_notes(
    gallery: &Gallery,
    path: &RelPath,
    notes: Option<String>,
) -> Result<(), TagError> {
    let notes = notes.filter(|n| !n.trim().is_empty());
    write_core(gallery, path, move |core| {
        core.notes = notes.clone();
    })
    .await?;
    gallery
        .events
        .send(Event::ItemsChanged { paths: vec![path.clone()] });
    Ok(())
}

/// Record that a file was viewed, in both places.
///
/// `last_viewed` is mirrored into the companion for the same reason
/// `date_added` is: otherwise a `format_version` bump silently empties it and
/// "nothing is lost but time" is false.
pub async fn record_view(gallery: &Gallery, path: &RelPath) -> Result<(), TagError> {
    let stamp = chrono::Utc::now().to_rfc3339();
    write_core(gallery, path, move |core| {
        core.last_viewed = Some(stamp.clone());
    })
    .await?;
    let conn = gallery.db.writer().await;
    meta::record_view(&conn, path)?;
    Ok(())
}

/// Apply one edit to a list of files, companion first, index second.
///
/// The closure is given each file's path as well as its tag list, for an edit
/// that differs per file — [`order_set`] gives each member its own place. It
/// reports whether it changed anything, so a no-op write never touches the
/// durable tree — which matters over a mount, where a rewrite is an mtime
/// change every other machine's index sweep then has to look at.
async fn edit(
    gallery: &Gallery,
    paths: &[RelPath],
    namespace: WritableNamespace,
    edit: impl Fn(&RelPath, &mut Vec<String>) -> bool + Send + Sync + 'static + Clone,
) -> Result<usize, TagError> {
    let mut touched = Vec::new();
    for path in paths {
        let absolute = gallery.root.resolve(path)?;
        let edit = edit.clone();
        let media_type = media_type_for(path);
        let edited = path.clone();

        // The lock blocks, and over the share it can block on another machine.
        let updated = tokio::task::spawn_blocking(move || {
            modify_companion(absolute.as_path(), media_type, |companion| {
                if edit(&edited, namespace.field(companion)) {
                    Outcome::Write(true)
                } else {
                    Outcome::Leave(false)
                }
            })
        })
        .await
        .map_err(|e| WriteError::Io(std::io::Error::other(e.to_string())))??;

        if !updated {
            continue;
        }
        touched.push(path.clone());
        reindex(gallery, path).await?;
    }

    if !touched.is_empty() {
        let _ = gallery.refresh_autocomplete().await;
        // Unconditional: this is the one path that *knows* tags moved. Moving a
        // tag from one file to another leaves the vocabulary byte for byte the
        // same while changing what a tag filter selects, so the vocabulary is
        // the wrong thing to ask here.
        gallery.events.send(Event::TagsIndexed);
        let changed = touched.len();
        gallery.events.send(Event::ItemsChanged { paths: touched });
        return Ok(changed);
    }
    Ok(0)
}

/// Mutate `meta.core`, creating it if absent.
async fn write_core(
    gallery: &Gallery,
    path: &RelPath,
    edit: impl Fn(&mut CoreMeta) + Send + 'static,
) -> Result<(), TagError> {
    let absolute = gallery.root.resolve(path)?;
    let media_type = media_type_for(path);
    tokio::task::spawn_blocking(move || {
        modify_companion(absolute.as_path(), media_type, |companion| {
            let mut core = companion.meta.core.take().unwrap_or_default();
            edit(&mut core);
            companion.meta.core = Some(core);
            Outcome::Write(())
        })
    })
    .await
    .map_err(|e| WriteError::Io(std::io::Error::other(e.to_string())))??;
    Ok(())
}

/// Re-read one companion and replace its index rows.
async fn reindex(gallery: &Gallery, path: &RelPath) -> Result<(), TagError> {
    let absolute = gallery.root.resolve(path)?;
    let read = tokio::task::spawn_blocking(move || {
        let companion = crate::companion::reader::read_companion(absolute.as_path());
        let state = std::fs::metadata(crate::companion::reader::companion_path(
            absolute.as_path(),
            crate::companion::reader::CompanionLocation::LightviewFolder,
        ))
        .ok()
        .map(|m| index::IndexState::of(&m));
        (companion, state)
    })
    .await
    .map_err(|e| WriteError::Io(std::io::Error::other(e.to_string())))?;

    let (companion, state) = read;
    let Ok(Some(companion)) = companion else {
        return Ok(());
    };

    let conn = gallery.db.writer().await;
    index::reindex_file(&conn, path, &companion)?;
    if let Some(state) = state {
        index::set_state(&conn, path, state)?;
    }
    drop(conn);
    Ok(())
}

/// Every tag in a writable namespace, with how many files carry it.
pub async fn list(gallery: &Gallery, namespace: WritableNamespace) -> Vec<TagCount> {
    gallery.autocomplete.list(namespace.as_str()).await
}

/// A sample of the files a tag selection covers, for the manager to show
/// before a gallery-wide rewrite.
///
/// Capped rather than complete: a tag covering the whole library would
/// otherwise pull tens of thousands of paths back to fill a preview strip.
pub async fn paths_with_tags(
    gallery: &Gallery,
    tags: &[String],
    namespace: WritableNamespace,
    limit: usize,
) -> Result<Vec<RelPath>, TagError> {
    let tags = namespace.name_args(tags)?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tag in &tags {
        for path in members_of(gallery, tag, namespace).await? {
            if out.len() >= limit {
                return Ok(out);
            }
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Every file carrying `tag` in `namespace`, from the index.
async fn members_of(
    gallery: &Gallery,
    tag: &str,
    namespace: WritableNamespace,
) -> Result<Vec<RelPath>, TagError> {
    let conn = gallery.db.read().await;
    Ok(index::paths_with_tag(&conn, namespace.as_str(), tag)?)
}

/// The media type a path's extension implies; an unknown extension is an image.
fn media_type_for(path: &RelPath) -> MediaType {
    std::path::Path::new(path.as_str())
        .extension()
        .and_then(|e| e.to_str())
        .and_then(MediaType::from_extension)
        .unwrap_or(MediaType::Image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_namespace_is_not_representable_on_the_wire() {
        // Not a check that could be forgotten: the type has no variant for it,
        // so a request naming one never reaches a handler.
        assert!(serde_json::from_str::<WritableNamespace>("\"user\"").is_ok());
        assert!(serde_json::from_str::<WritableNamespace>("\"set\"").is_ok());
        assert!(serde_json::from_str::<WritableNamespace>("\"plugin.wd\"").is_err());
        assert!(serde_json::from_str::<WritableNamespace>("\"auto\"").is_err());
    }

    #[test]
    fn a_set_entry_is_known_by_its_name_and_a_user_tag_by_its_string() {
        assert_eq!(WritableNamespace::Set.name_of("comic::3"), "comic");
        assert_eq!(WritableNamespace::Set.name_of("comic"), "comic");
        // A user tag has no position; `::` in one is just text.
        assert_eq!(WritableNamespace::User.name_of("comic::3"), "comic::3");
    }

    #[test]
    fn a_set_name_that_would_read_back_as_a_position_is_refused() {
        assert!(matches!(
            WritableNamespace::Set.name_arg("ch::2"),
            Err(TagError::InvalidSetName(_))
        ));
        assert!(WritableNamespace::Set.name_arg("a::b").is_ok());
        assert!(WritableNamespace::User.name_arg("ch::2").is_ok());
    }

    fn folded(list: &[&str], sources: &[&str], target: &str, keep: bool) -> (Vec<String>, bool) {
        let mut list: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
        let changed = fold_into(WritableNamespace::Set, &mut list, &sources, target, keep);
        (list, changed)
    }

    #[test]
    fn a_rename_to_a_new_name_carries_the_order() {
        assert_eq!(
            folded(&["burst", "comic::3"], &["comic"], "strip", true),
            (vec!["burst".to_string(), "strip::3".to_string()], true)
        );
    }

    #[test]
    fn joining_an_existing_set_arrives_unordered() {
        assert_eq!(
            folded(&["a::1"], &["a", "b"], "c", false),
            (vec!["c".to_string()], true)
        );
        // The target's own members keep their places.
        assert_eq!(folded(&["c::4"], &["a", "b"], "c", false), (vec!["c::4".to_string()], false));
    }

    #[test]
    fn a_file_already_in_the_target_keeps_that_entry() {
        assert_eq!(
            folded(&["a::2", "b::7"], &["a"], "b", false),
            (vec!["b::7".to_string()], true)
        );
    }

    #[test]
    fn the_two_namespaces_are_siblings_in_the_companion() {
        let mut c = CompanionFile::new("a.jpg", MediaType::Image);
        WritableNamespace::User.field(&mut c).push("vacation".into());
        WritableNamespace::Set.field(&mut c).push("burst-3".into());
        assert_eq!(c.tags.user, vec!["vacation"]);
        assert_eq!(c.tags.set, vec!["burst-3"]);
    }
}
