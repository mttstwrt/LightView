//! A set's order, end to end through the services: written by `order_set` into
//! the sidecars, read back by the items query, and carried or dropped by the
//! other tag operations exactly as `services::tags` says.
//!
//! Every query here sorts by name, so the order a set view falls back to — for
//! members without a position, and for any view that is not a set — is known
//! without arranging file times.

use std::sync::Arc;

use lightview::autocomplete::engine::AutocompleteEngine;
use lightview::cache::db::CacheDb;
use lightview::cache::index;
use lightview::companion::reader::read_companion;
use lightview::path::{RelPath, Root};
use lightview::pipeline::serve::ThumbService;
use lightview::server::events::Events;
use lightview::services::media::{self, ItemsRequest};
use lightview::services::settings::GallerySettings;
use lightview::services::tags::{self, TagError, WritableNamespace};
use lightview::sort::grouper::{GroupBy, Granularity};
use lightview::sort::sorter::{SortField, SortOrder};
use lightview::state::Gallery;
use lightview::util::paths::Dirs;

const SET: WritableNamespace = WritableNamespace::Set;

struct Fixture {
    gallery: Arc<Gallery>,
    dir: tempfile::TempDir,
    _state: tempfile::TempDir,
}

/// A gallery of four images, `a.png` to `d.png`, scanned and indexed.
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c", "d"] {
        image::RgbImage::new(8, 6)
            .save(dir.path().join(format!("{name}.png")))
            .unwrap();
    }

    let dirs = Dirs::under(state.path());
    dirs.ensure().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let db = Arc::new(CacheDb::open_at(&dirs.gallery_cache(root.as_path())).unwrap());
    let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
    let thumbs = Arc::new(ThumbService::new(db.clone(), root.clone(), pool, 1 << 20));
    let gallery = Arc::new(Gallery {
        root,
        db,
        thumbs,
        events: Arc::new(Events::new()),
        autocomplete: Arc::new(AutocompleteEngine::new()),
        settings: std::sync::RwLock::new(GallerySettings::default()),
        cache_dir: dirs.cache().to_path_buf(),
    });
    lightview::services::gallery::scan_and_index(&gallery).await.unwrap();
    Fixture {
        gallery,
        dir,
        _state: state,
    }
}

fn paths(names: &[&str]) -> Vec<RelPath> {
    names
        .iter()
        .map(|n| RelPath::new(&format!("{n}.png")).unwrap())
        .collect()
}

impl Fixture {
    /// The grid for `filter`, sorted by name and asking for monthly groups:
    /// `(order, how many groups, the set the view is)`.
    async fn grid(&self, filter: &str) -> (Vec<String>, usize, Option<String>) {
        let request = ItemsRequest {
            sort: SortField::Name,
            order: SortOrder::Asc,
            filter: filter.to_string(),
            group_by: GroupBy::TimePeriod {
                granularity: Granularity::Month,
            },
            ..Default::default()
        };
        let items = media::get_items(&self.gallery, &request).await.unwrap();
        let order = items
            .items
            .iter()
            .map(|i| i.path.as_str().trim_end_matches(".png").to_string())
            .collect();
        (order, items.groups.len(), items.set)
    }

    /// What `name.png`'s sidecar holds under `tags.set`.
    fn sidecar(&self, name: &str) -> Vec<String> {
        read_companion(&self.dir.path().join(format!("{name}.png")))
            .unwrap()
            .map(|c| c.tags.set)
            .unwrap_or_default()
    }

    async fn enrol(&self, names: &[&str], set: &str) {
        tags::add(&self.gallery, &paths(names), &[set.to_string()], SET)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_set_view_shows_the_order_it_was_given() {
    let f = fixture().await;
    f.enrol(&["a", "b", "c"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["c", "a", "b"]))
        .await
        .unwrap();

    assert_eq!(f.sidecar("c"), vec!["strip::1"]);
    assert_eq!(f.sidecar("a"), vec!["strip::2"]);
    assert_eq!(f.sidecar("b"), vec!["strip::3"]);

    let (order, groups, set) = f.grid("set::strip").await;
    assert_eq!(order, vec!["c", "a", "b"]);
    // A group break is a row break; a set view has none to cut a strip with.
    assert_eq!(groups, 0);
    assert_eq!(set.as_deref(), Some("strip"));
}

#[tokio::test]
async fn a_member_without_a_position_follows_the_ordered_ones() {
    let f = fixture().await;
    f.enrol(&["b", "c"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["c", "b"]))
        .await
        .unwrap();
    f.enrol(&["a"], "strip").await;

    assert_eq!(f.grid("set::strip").await.0, vec!["c", "b", "a"]);
}

#[tokio::test]
async fn a_view_that_is_more_than_the_set_keeps_the_requested_sort() {
    let f = fixture().await;
    f.enrol(&["a", "b", "c"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["c", "b", "a"]))
        .await
        .unwrap();

    let (order, _, set) = f.grid("set::strip AND type:image").await;
    assert_eq!(order, vec!["a", "b", "c"]);
    assert_eq!(set, None);
}

#[tokio::test]
async fn the_index_knows_a_member_by_the_set_name_alone() {
    let f = fixture().await;
    f.enrol(&["a", "b"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["b", "a"]))
        .await
        .unwrap();

    // The duplicate finder's suppression reads these pairs: two members of one
    // set must still share one string after they are ordered.
    let conn = f.gallery.db.read().await;
    let mut membership = index::set_membership(&conn).unwrap();
    membership.sort();
    assert_eq!(
        membership,
        vec![
            ("a.png".to_string(), "strip".to_string()),
            ("b.png".to_string(), "strip".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_rename_to_a_new_name_carries_the_order() {
    let f = fixture().await;
    f.enrol(&["a", "b", "c"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["c", "a", "b"]))
        .await
        .unwrap();
    tags::rename(&f.gallery, "strip", "comic", SET).await.unwrap();

    assert_eq!(f.sidecar("c"), vec!["comic::1"]);
    assert_eq!(f.grid("set::comic").await.0, vec!["c", "a", "b"]);
    assert!(f.grid("set::strip").await.0.is_empty());
}

#[tokio::test]
async fn joining_a_set_that_exists_arrives_unordered() {
    let f = fixture().await;
    f.enrol(&["a", "b", "c"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["c", "b", "a"]))
        .await
        .unwrap();
    f.enrol(&["d", "c"], "comic").await;
    tags::order_set(&f.gallery, "comic", &paths(&["d", "c"]))
        .await
        .unwrap();

    // Renaming onto a taken name is a merge: the joiners lose their places, the
    // target's members keep theirs, and `c` — in both — keeps its `comic` one.
    tags::rename(&f.gallery, "strip", "comic", SET).await.unwrap();
    assert_eq!(f.sidecar("c"), vec!["comic::2"]);
    assert_eq!(f.sidecar("a"), vec!["comic"]);
    assert_eq!(f.grid("set::comic").await.0, vec!["d", "c", "a", "b"]);
}

#[tokio::test]
async fn removing_a_set_by_name_removes_a_positioned_entry() {
    let f = fixture().await;
    f.enrol(&["a", "b"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["a", "b"]))
        .await
        .unwrap();

    tags::remove(&f.gallery, &paths(&["a"]), &["strip".to_string()], SET)
        .await
        .unwrap();
    assert!(f.sidecar("a").is_empty());

    tags::delete(&f.gallery, "strip", SET).await.unwrap();
    assert!(f.sidecar("b").is_empty());
}

#[tokio::test]
async fn enrolling_a_member_twice_leaves_its_place_alone() {
    let f = fixture().await;
    f.enrol(&["a", "b"], "strip").await;
    tags::order_set(&f.gallery, "strip", &paths(&["b", "a"]))
        .await
        .unwrap();
    f.enrol(&["a"], "strip").await;
    assert_eq!(f.sidecar("a"), vec!["strip::2"]);
}

#[tokio::test]
async fn an_empty_order_clears_the_set_and_a_stranger_is_never_enrolled() {
    let f = fixture().await;
    f.enrol(&["a", "b"], "strip").await;

    // `d` is not a member: listing it gives it nothing and costs `a` nothing.
    tags::order_set(&f.gallery, "strip", &paths(&["d", "b", "a", "b"]))
        .await
        .unwrap();
    assert!(f.sidecar("d").is_empty());
    assert_eq!(f.sidecar("b"), vec!["strip::1"]);
    assert_eq!(f.sidecar("a"), vec!["strip::2"]);

    tags::order_set(&f.gallery, "strip", &[]).await.unwrap();
    assert_eq!(f.sidecar("a"), vec!["strip"]);
    assert_eq!(f.sidecar("b"), vec!["strip"]);
    assert_eq!(f.grid("set::strip").await.0, vec!["a", "b"]);
}

#[tokio::test]
async fn a_set_name_that_would_read_back_as_a_position_is_refused() {
    let f = fixture().await;
    let refused = tags::add(&f.gallery, &paths(&["a"]), &["ch::2".to_string()], SET).await;
    assert!(matches!(refused, Err(TagError::InvalidSetName(_))));
    assert!(f.sidecar("a").is_empty());

    // A user tag has no position, so the same string is just a tag.
    tags::add(
        &f.gallery,
        &paths(&["a"]),
        &["ch::2".to_string()],
        WritableNamespace::User,
    )
    .await
    .unwrap();
}
