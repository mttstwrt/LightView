//! Writing tags, ratings, colour labels and notes.
//!
//! **Every write goes through one locked read-modify-write of the companion**,
//! then mirrors into the index. There is no path that writes the database and
//! hopes the sidecar catches up, and no path that writes the sidecar without
//! taking the lock — see [`crate::companion::writer::modify_companion`].
//!
//! # The namespace is a parameter, not a parallel family
//!
//! Every tag-write operation was user-hardcoded: add, remove, the batch forms,
//! rename, merge, delete. "The tag-write commands apply unchanged" for sets was
//! therefore wrong. Each takes a [`WritableNamespace`] instead, which accepts
//! `user` or `set` **and nothing else** — a plugin bucket is replaced wholesale
//! by its own run, so writing into one through this path would be a second
//! writer for something with a single owner.
//!
//! That one parameter is the entire surface sets need: create is a batch add
//! over a selection, rename is `rename`, merging two clusters is `merge`,
//! delete is `delete`, and the tag manager lists both namespaces instead of
//! one. No new file, no new table, no new wire format, no new filter syntax.
//!
//! **Sets are cheap and fluid, deliberately.** Renaming one rewrites every
//! member's sidecar; trashing a member shrinks it silently. A set is not a
//! durable object with an identity, it is a name several files agree on.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::autocomplete::engine::TagCount;
use crate::cache::{index, meta};
use crate::companion::schema::{CompanionFile, CoreMeta, MediaType, Order};
use crate::sort::order_key;
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
    pub fn as_str(self) -> &'static str {
        match self {
            WritableNamespace::User => "user",
            WritableNamespace::Set => "set",
        }
    }

    fn field(self, companion: &mut CompanionFile) -> &mut Vec<String> {
        match self {
            WritableNamespace::User => &mut companion.tags.user,
            WritableNamespace::Set => &mut companion.tags.set,
        }
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
    let tags = tags.to_vec();
    edit(gallery, paths, namespace, move |list| {
        let mut changed = false;
        for tag in &tags {
            if !list.iter().any(|t| t == tag) {
                list.push(tag.clone());
                changed = true;
            }
        }
        changed
    })
    .await
}

/// Remove tags from a selection.
///
/// Taking a file out of a set also takes it out of that set's block: its
/// order goes with the tag, and it returns to where its date puts it. Keeping
/// the order would leave it sorted at the block's key, clumped beside the
/// block without belonging to it — and a set deleted outright would leave
/// every former member clumped at one shared key.
pub async fn remove(
    gallery: &Gallery,
    paths: &[RelPath],
    tags: &[String],
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let tags = tags.to_vec();
    let edited = edit_companion(gallery, paths, move |_, companion| {
        let list = namespace.field(companion);
        let before = list.len();
        list.retain(|t| !tags.contains(t));
        let mut changed = list.len() != before;
        if namespace == WritableNamespace::Set && order_set_is(companion, |s| tags.contains(s)) {
            companion.meta.order = None;
            changed = true;
        }
        changed
    })
    .await?;
    Ok(announce(gallery, edited).await)
}

/// Whether `companion`'s order names a set that `pred` picks.
fn order_set_is(companion: &CompanionFile, pred: impl Fn(&String) -> bool) -> bool {
    companion
        .meta
        .order
        .as_ref()
        .and_then(|o| o.set.as_ref())
        .is_some_and(pred)
}

/// Rename a tag everywhere it appears.
///
/// For a set this is the rename operation in full: the set *is* the name, so
/// rewriting every member's sidecar is not a side effect, it is the change.
pub async fn rename(
    gallery: &Gallery,
    from: &str,
    to: &str,
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let members = members_of(gallery, from, namespace).await?;
    let (from, to) = (from.to_string(), to.to_string());
    let edited = edit_companion(gallery, &members, move |_, companion| {
        let list = namespace.field(companion);
        let mut changed = false;
        for tag in list.iter_mut() {
            if *tag == from {
                tag.clone_from(&to);
                changed = true;
            }
        }
        // A file already carrying the destination would now carry it twice.
        list.sort();
        list.dedup();
        // The block goes with its name.
        if namespace == WritableNamespace::Set && order_set_is(companion, |s| *s == from) {
            if let Some(order) = companion.meta.order.as_mut() {
                order.set = Some(to.clone());
            }
            changed = true;
        }
        changed
    })
    .await?;
    Ok(announce(gallery, edited).await)
}

/// Fold several tags into one. Merging two clusters is this.
///
/// For sets, merging two ordered sets **concatenates** their blocks: the
/// target's members in their order, then each source's in the order given,
/// all under the lowest of their keys. Merge already rewrites every member's
/// sidecar, so keeping each block's order whole costs no write that
/// interleaving their positions would not.
pub async fn merge(
    gallery: &Gallery,
    sources: &[String],
    target: &str,
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let mut members = Vec::new();
    for source in sources {
        members.extend(members_of(gallery, source, namespace).await?);
    }
    let placement = if namespace == WritableNamespace::Set {
        concatenate_blocks(gallery, sources, target).await?
    } else {
        HashMap::new()
    };
    // The target's own block members are renumbered too.
    members.extend(placement.keys().cloned());
    members.sort();
    members.dedup();

    let (sources, target) = (sources.to_vec(), target.to_string());
    let placement = Arc::new(placement);
    let edited = edit_companion(gallery, &members, move |path, companion| {
        let list = namespace.field(companion);
        let before = list.clone();
        list.retain(|t| !sources.contains(t));
        if !list.contains(&target) {
            list.push(target.clone());
        }
        list.sort();
        list.dedup();
        let mut changed = *list != before;
        if let Some(order) = placement.get(path)
            && companion.meta.order.as_ref() != Some(order)
        {
            companion.meta.order = Some(order.clone());
            changed = true;
        }
        changed
    })
    .await?;
    Ok(announce(gallery, edited).await)
}

/// Where each member of the merged block goes: `target`'s block, then each
/// source's, renumbered as one run under the lowest key among them. Empty when
/// no source is a block, so merging plain sets writes nothing extra.
async fn concatenate_blocks(
    gallery: &Gallery,
    sources: &[String],
    target: &str,
) -> Result<HashMap<RelPath, Order>, TagError> {
    let conn = gallery.db.read().await;
    let mut run = index::block_members(&conn, target)?;
    let from_target = run.len();
    for source in sources.iter().filter(|s| s.as_str() != target) {
        run.extend(index::block_members(&conn, source)?);
    }
    drop(conn);
    if run.len() == from_target {
        return Ok(HashMap::new());
    }
    let key = run.iter().filter_map(|(_, row)| row.key.clone()).min();
    let positions = order_key::spread(None, run.len());
    Ok(run
        .into_iter()
        .zip(positions)
        .map(|((path, _), pos)| {
            let order = Order { key: key.clone(), set: Some(target.to_string()), pos: Some(pos) };
            (path, order)
        })
        .collect())
}

/// Remove a tag from every file that carries it.
pub async fn delete(
    gallery: &Gallery,
    tag: &str,
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let members = members_of(gallery, tag, namespace).await?;
    remove(gallery, &members, std::slice::from_ref(&tag.to_string()), namespace).await
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

/// Apply one edit to one namespace's tag list over a selection, and announce
/// it.
async fn edit(
    gallery: &Gallery,
    paths: &[RelPath],
    namespace: WritableNamespace,
    edit: impl Fn(&mut Vec<String>) -> bool + Send + Sync + 'static + Clone,
) -> Result<usize, TagError> {
    let edited = edit_companion(gallery, paths, move |_, c| edit(namespace.field(c))).await?;
    Ok(announce(gallery, edited).await)
}

/// What one edit over a selection changed.
#[derive(Debug, Default)]
pub(crate) struct Edited {
    /// The files whose sidecar was rewritten.
    pub touched: Vec<RelPath>,
    /// Whether any of them moved in the Custom order.
    pub order_changed: bool,
}

/// Apply one edit to a list of files, companion first, index second.
///
/// The closure sees the file's path and its whole sidecar, and reports whether
/// it changed anything, so a no-op write never touches the durable tree — which
/// matters over a mount, where a rewrite is an mtime change every other
/// machine's index sweep then has to look at. Announcing is the caller's: only
/// it knows what kind of change it made.
pub(crate) async fn edit_companion(
    gallery: &Gallery,
    paths: &[RelPath],
    edit: impl Fn(&RelPath, &mut CompanionFile) -> bool + Send + Sync + 'static + Clone,
) -> Result<Edited, TagError> {
    let mut edited = Edited::default();
    for path in paths {
        let absolute = gallery.root.resolve(path)?;
        let edit = edit.clone();
        let media_type = media_type_for(path);
        let rel = path.clone();

        // The lock blocks, and over the share it can block on another machine.
        let updated = tokio::task::spawn_blocking(move || {
            modify_companion(absolute.as_path(), media_type, |companion| {
                if edit(&rel, companion) {
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
        edited.touched.push(path.clone());
        edited.order_changed |= reindex(gallery, path).await?;
    }
    Ok(edited)
}

/// Tell every client what a tag edit changed, and how many files it touched.
async fn announce(gallery: &Gallery, edited: Edited) -> usize {
    if edited.order_changed {
        gallery.events.send(Event::OrderChanged);
    }
    if edited.touched.is_empty() {
        return 0;
    }
    let _ = gallery.refresh_autocomplete().await;
    // Unconditional: this is the one path that *knows* tags moved. Moving a
    // tag from one file to another leaves the vocabulary byte for byte the
    // same while changing what a tag filter selects, so the vocabulary is the
    // wrong thing to ask here.
    gallery.events.send(Event::TagsIndexed);
    let changed = edited.touched.len();
    gallery.events.send(Event::ItemsChanged { paths: edited.touched });
    changed
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

/// Re-read one companion and replace its index rows. Reports whether the
/// file moved in the Custom order.
async fn reindex(gallery: &Gallery, path: &RelPath) -> Result<bool, TagError> {
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
        return Ok(false);
    };

    let conn = gallery.db.writer().await;
    let order_changed = index::reindex_file(&conn, path, &companion)?;
    if let Some(state) = state {
        index::set_state(&conn, path, state)?;
    }
    drop(conn);
    Ok(order_changed)
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
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tag in tags {
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
    fn the_two_namespaces_are_siblings_in_the_companion() {
        let mut c = CompanionFile::new("a.jpg", MediaType::Image);
        WritableNamespace::User.field(&mut c).push("vacation".into());
        WritableNamespace::Set.field(&mut c).push("burst-3".into());
        assert_eq!(c.tags.user, vec!["vacation"]);
        assert_eq!(c.tags.set, vec!["burst-3"]);
    }

    /// A gallery on disk whose files carry the given set and order, indexed.
    async fn arranged(
        dir: &std::path::Path,
        files: &[(&str, &[&str], Option<Order>)],
    ) -> Gallery {
        let gallery = crate::services::test_gallery(dir);
        for (name, sets, order) in files {
            std::fs::write(dir.join(name), b"x").unwrap();
            let path = RelPath::new(name).unwrap();
            {
                let conn = gallery.db.writer().await;
                conn.execute(
                    "INSERT INTO media_meta (path, media_type, file_size, mtime) VALUES (?1, 'image', 1, 1)",
                    [name],
                )
                .unwrap();
            }
            let (sets, order) = (sets.iter().map(|s| s.to_string()).collect::<Vec<_>>(), order.clone());
            let absolute = gallery.root.resolve(&path).unwrap();
            modify_companion(absolute.as_path(), MediaType::Image, move |c| {
                c.tags.set = sets;
                c.meta.order = order;
                Outcome::Write(())
            })
            .unwrap();
            reindex(&gallery, &path).await.unwrap();
        }
        gallery
    }

    fn in_block(set: &str, key: &str, pos: &str) -> Option<Order> {
        Some(Order { key: Some(key.into()), set: Some(set.into()), pos: Some(pos.into()) })
    }

    fn sidecar_order(gallery: &Gallery, name: &str) -> Option<Order> {
        let absolute = gallery.root.resolve(&RelPath::new(name).unwrap()).unwrap();
        crate::companion::reader::read_companion(absolute.as_path())
            .unwrap()
            .unwrap()
            .meta
            .order
    }

    async fn block(gallery: &Gallery, set: &str) -> Vec<String> {
        let conn = gallery.db.read().await;
        index::block_members(&conn, set)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p.as_str().to_string())
            .collect()
    }

    #[tokio::test]
    async fn taking_a_file_out_of_a_set_takes_it_out_of_the_block() {
        let d = tempfile::tempdir().unwrap();
        let gallery = arranged(
            d.path(),
            &[
                ("a.jpg", &["comic"], in_block("comic", "K", "1")),
                ("b.jpg", &["comic"], in_block("comic", "K", "2")),
            ],
        )
        .await;
        let mut events = gallery.events.subscribe();

        let a = RelPath::new("a.jpg").unwrap();
        remove(&gallery, &[a], &["comic".into()], WritableNamespace::Set).await.unwrap();

        assert_eq!(sidecar_order(&gallery, "a.jpg"), None, "back to its date position");
        assert_eq!(block(&gallery, "comic").await, ["b.jpg"]);
        assert!(matches!(events.try_recv(), Ok(Event::OrderChanged)));
    }

    #[tokio::test]
    async fn deleting_a_set_dissolves_its_block() {
        let d = tempfile::tempdir().unwrap();
        let gallery = arranged(
            d.path(),
            &[
                ("a.jpg", &["comic"], in_block("comic", "K", "1")),
                ("b.jpg", &["comic"], in_block("comic", "K", "2")),
            ],
        )
        .await;
        delete(&gallery, "comic", WritableNamespace::Set).await.unwrap();
        assert_eq!(sidecar_order(&gallery, "a.jpg"), None);
        assert_eq!(sidecar_order(&gallery, "b.jpg"), None);
        assert!(block(&gallery, "comic").await.is_empty());
    }

    #[tokio::test]
    async fn renaming_a_set_carries_its_block() {
        let d = tempfile::tempdir().unwrap();
        let gallery = arranged(
            d.path(),
            &[
                ("a.jpg", &["comic"], in_block("comic", "K", "1")),
                ("b.jpg", &["comic"], in_block("comic", "K", "2")),
            ],
        )
        .await;
        rename(&gallery, "comic", "manga", WritableNamespace::Set).await.unwrap();
        assert_eq!(block(&gallery, "manga").await, ["a.jpg", "b.jpg"]);
        assert!(block(&gallery, "comic").await.is_empty());
    }

    #[tokio::test]
    async fn merging_two_blocks_concatenates_them_under_the_lower_key() {
        let d = tempfile::tempdir().unwrap();
        let gallery = arranged(
            d.path(),
            &[
                ("a.jpg", &["comic"], in_block("comic", "K2", "1")),
                ("b.jpg", &["comic"], in_block("comic", "K2", "2")),
                // Sorted by position it would interleave with the target's.
                ("c.jpg", &["zine"], in_block("zine", "K1", "0")),
                ("d.jpg", &["zine"], in_block("zine", "K1", "3")),
            ],
        )
        .await;
        merge(&gallery, &["zine".into()], "comic", WritableNamespace::Set).await.unwrap();
        assert_eq!(block(&gallery, "comic").await, ["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        for name in ["a.jpg", "b.jpg", "c.jpg", "d.jpg"] {
            assert_eq!(sidecar_order(&gallery, name).unwrap().key.as_deref(), Some("K1"));
        }
    }

    #[tokio::test]
    async fn a_user_tag_edit_leaves_the_order_alone() {
        let d = tempfile::tempdir().unwrap();
        let gallery = arranged(d.path(), &[("a.jpg", &["comic"], in_block("comic", "K", "1"))]).await;
        let mut events = gallery.events.subscribe();
        let a = RelPath::new("a.jpg").unwrap();
        add(&gallery, std::slice::from_ref(&a), &["comic".into()], WritableNamespace::User)
            .await
            .unwrap();
        remove(&gallery, &[a], &["comic".into()], WritableNamespace::User).await.unwrap();
        assert!(sidecar_order(&gallery, "a.jpg").is_some());
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, Event::OrderChanged), "a user tag is not an arrangement");
        }
    }
}
