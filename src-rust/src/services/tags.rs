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

use serde::{Deserialize, Serialize};

use crate::cache::{index, meta};
use crate::companion::schema::{CompanionFile, CoreMeta, MediaType};
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
pub async fn remove(
    gallery: &Gallery,
    paths: &[RelPath],
    tags: &[String],
    namespace: WritableNamespace,
) -> Result<usize, TagError> {
    let tags = tags.to_vec();
    edit(gallery, paths, namespace, move |list| {
        let before = list.len();
        list.retain(|t| !tags.contains(t));
        list.len() != before
    })
    .await
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
    edit(gallery, &members, namespace, move |list| {
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
        changed
    })
    .await
}

/// Fold several tags into one. Merging two clusters is this.
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
    members.sort();
    members.dedup();

    let (sources, target) = (sources.to_vec(), target.to_string());
    edit(gallery, &members, namespace, move |list| {
        let before = list.clone();
        list.retain(|t| !sources.contains(t));
        if !list.contains(&target) {
            list.push(target.clone());
        }
        list.sort();
        list.dedup();
        *list != before
    })
    .await
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

/// Set or clear a rating, mirroring it into the indexed column.
pub async fn set_rating(
    gallery: &Gallery,
    path: &RelPath,
    rating: Option<u8>,
) -> Result<(), TagError> {
    write_core(gallery, path, move |core| {
        core.rating = rating;
        core.date_rated = Some(chrono::Utc::now().to_rfc3339());
    })
    .await?;

    let conn = gallery.db.writer().await;
    meta::set_rating(&conn, path, rating)?;
    drop(conn);
    gallery.events.send(Event::ItemChanged { path: path.clone() });
    Ok(())
}

/// Set or clear a colour label.
pub async fn set_color_label(
    gallery: &Gallery,
    path: &RelPath,
    label: Option<String>,
) -> Result<(), TagError> {
    let normalized = label
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty());
    let stored = normalized.clone();
    write_core(gallery, path, move |core| {
        core.color_label = stored.clone();
    })
    .await?;

    let conn = gallery.db.writer().await;
    meta::set_color_label(&conn, path, normalized.as_deref())?;
    drop(conn);
    gallery.events.send(Event::ItemChanged { path: path.clone() });
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
    gallery.events.send(Event::ItemChanged { path: path.clone() });
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
/// The closure reports whether it changed anything, so a no-op write never
/// touches the durable tree — which matters over a mount, where a rewrite is
/// an mtime change every other machine's index sweep then has to look at.
async fn edit(
    gallery: &Gallery,
    paths: &[RelPath],
    namespace: WritableNamespace,
    edit: impl Fn(&mut Vec<String>) -> bool + Send + Sync + 'static + Clone,
) -> Result<usize, TagError> {
    let mut changed = 0;
    for path in paths {
        let absolute = gallery.root.resolve(path)?;
        let edit = edit.clone();
        let media_type = media_type_for(path);

        // The lock blocks, and over the share it can block on another machine.
        let updated = tokio::task::spawn_blocking(move || {
            modify_companion(absolute.as_path(), media_type, |companion| {
                if edit(namespace.field(companion)) {
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
        changed += 1;
        reindex(gallery, path).await?;
    }

    if changed > 0 {
        gallery.refresh_autocomplete().await;
    }
    Ok(changed)
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
    gallery.events.send(Event::ItemChanged { path: path.clone() });
    Ok(())
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
}
