//! Step 2's acceptance: a fresh open indexes a gallery, and a version bump
//! deletes and rebuilds it.
//!
//! This is deliberately an *integration* test — it walks the same path a
//! gallery open walks, across every module that has to agree about what a path
//! is: the provider produces `RelPath`s relative to the canonical root, the
//! companion reader finds sidecars per directory, the index keys on the same
//! strings, and the sort statement selects them back. The failure it catches is
//! any two of those disagreeing, which does not show up in a unit test of
//! either one.

use lightview::cache::{db, index, meta};
use lightview::companion::schema::{CompanionFile, CoreMeta, MediaType, PluginTagEntry};
use lightview::companion::writer::{modify_companion, Outcome};
use lightview::path::{RelPath, Root};
use lightview::provider::local::LocalProvider;
use lightview::sort::sorter::{items_sql, map_row, SortField, SortOrder, SortSpec};

/// A gallery with a nested directory, a companion carrying user, set and
/// plugin tags, and a file with no companion at all.
fn gallery() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("2026/january")).unwrap();
    std::fs::write(d.path().join("2026/january/sunset.jpg"), b"jpeg-bytes").unwrap();
    std::fs::write(d.path().join("untagged.png"), b"png-bytes").unwrap();

    let media = d.path().join("2026/january/sunset.jpg");
    modify_companion(&media, MediaType::Image, |c| {
        c.tags.user = vec!["vacation".into()];
        c.tags.set = vec!["kellys-comic".into()];
        c.tags.plugins.insert(
            "location".into(),
            PluginTagEntry {
                version: "geonames-cities1000/2".into(),
                tags: vec!["Japan".into(), "Kyoto".into()],
                ..Default::default()
            },
        );
        c.meta.core = Some(CoreMeta {
            rating: Some(5),
            color_label: Some("Red".into()),
            date_added: Some("2020-05-05T00:00:00+00:00".into()),
            ..Default::default()
        });
        Outcome::Write(())
    })
    .unwrap();
    d
}

/// Everything `open_gallery` does at the cache layer: scan, insert, read each
/// companion, mirror it and index its tags.
fn index_gallery(conn: &rusqlite::Connection, root: &Root) -> Vec<RelPath> {
    let provider = LocalProvider::new(root.clone());
    let files = provider.list_dir_recursive().expect("scan");

    let scanned: Vec<meta::ScannedFile> = files
        .iter()
        .map(|f| meta::ScannedFile {
            path: f.path.clone(),
            media_type: "image",
            file_size: f.size as i64,
            mtime: f.mtime as i64,
        })
        .collect();
    meta::insert_scanned(conn, &scanned).unwrap();
    db::prune_missing(conn, &scanned.iter().map(|s| s.path.clone()).collect::<Vec<_>>()).unwrap();

    for f in &files {
        let abs = root.resolve(&f.path).unwrap();
        if let Some(companion) = lightview::companion::reader::read_companion(abs.as_path()).unwrap()
        {
            meta::mirror_companion(conn, &f.path, &companion).unwrap();
            index::reindex_file(conn, &f.path, &companion).unwrap();
            let state = index::IndexState::of(
                &std::fs::metadata(lightview::companion::reader::companion_path(
                    abs.as_path(),
                    lightview::companion::reader::CompanionLocation::LightviewFolder,
                ))
                .unwrap(),
            );
            index::set_state(conn, &f.path, state).unwrap();
        }
    }
    files.into_iter().map(|f| f.path).collect()
}

fn items(conn: &rusqlite::Connection, where_sql: Option<&str>, params: &[String]) -> Vec<String> {
    let spec = SortSpec {
        field: SortField::Name,
        order: SortOrder::Asc,
        sub_field: None,
        sub_order: None,
    };
    let sql = items_sql(&spec, where_sql);
    let mut stmt = conn.prepare(&sql).unwrap();
    let bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    stmt.query_map(bound.as_slice(), map_row)
        .unwrap()
        .map(|r| r.unwrap().path.as_str().to_string())
        .collect()
}

#[test]
fn a_fresh_open_indexes_a_gallery() {
    let g = gallery();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = Root::open(g.path()).unwrap();
    let cache = db::CacheDb::open_at(cache_dir.path()).unwrap();
    let conn = cache.writer_blocking();

    let scanned = index_gallery(&conn, &root);

    // Nested paths are gallery-relative and the companion directory is not a
    // media file.
    let mut paths: Vec<_> = scanned.iter().map(|p| p.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["2026/january/sunset.jpg", "untagged.png"]);

    // The sort statement selects the same keys back.
    assert_eq!(
        items(&conn, None, &[]),
        vec!["2026/january/sunset.jpg", "untagged.png"]
    );

    // Tags reached the index under the namespaces the query language knows.
    let tagged = RelPath::new("2026/january/sunset.jpg").unwrap();
    let mut tags = index::tags_for_file(&conn, &tagged).unwrap();
    tags.sort();
    assert_eq!(
        tags,
        vec![
            ("plugin.location".to_string(), "Japan".to_string()),
            ("plugin.location".to_string(), "Kyoto".to_string()),
            ("set".to_string(), "kellys-comic".to_string()),
            ("user".to_string(), "vacation".to_string()),
        ]
    );

    // The mirrored columns are what the filter can actually compare against.
    let (rating, color, added): (Option<u8>, Option<String>, i64) = conn
        .query_row(
            "SELECT rating, color_label, date_added FROM media_meta WHERE path = ?1",
            [tagged.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(rating, Some(5));
    assert_eq!(color.as_deref(), Some("red"), "colour is normalized on the way in");
    assert_eq!(added, 1588636800, "the companion's date_added won");
}

#[test]
fn a_compiled_filter_runs_against_the_index_it_just_built() {
    let g = gallery();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = Root::open(g.path()).unwrap();
    let cache = db::CacheDb::open_at(cache_dir.path()).unwrap();
    let conn = cache.writer_blocking();
    index_gallery(&conn, &root);

    for (query, expected) in [
        ("set::kellys-comic", vec!["2026/january/sunset.jpg"]),
        ("user::vacation", vec!["2026/january/sunset.jpg"]),
        ("Kyoto", vec!["2026/january/sunset.jpg"]),
        ("rating>=4", vec!["2026/january/sunset.jpg"]),
        ("color:red", vec!["2026/january/sunset.jpg"]),
        ("NOT has::user", vec!["untagged.png"]),
        ("has::set", vec!["2026/january/sunset.jpg"]),
    ] {
        let expr = lightview::filter::parser::parse_filter(query).unwrap();
        let mut params = Vec::new();
        let where_sql = lightview::filter::evaluator::to_sql(&expr, &mut params);
        assert_eq!(items(&conn, Some(&where_sql), &params), expected, "query: {query}");
    }
}

#[test]
fn a_format_version_bump_rebuilds_from_the_companions() {
    // The claim requirement 11 rests on: delete the derived half and nothing is
    // lost but time. `date_added` is the field that used to make it false.
    let g = gallery();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = Root::open(g.path()).unwrap();

    {
        let cache = db::CacheDb::open_at(cache_dir.path()).unwrap();
        let conn = cache.writer_blocking();
        index_gallery(&conn, &root);
        db::meta_set(&conn, "format_version", "0").unwrap();
    }

    let cache = db::CacheDb::open_at(cache_dir.path()).unwrap();
    let conn = cache.writer_blocking();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM media_meta", [], |r| r.get(0))
            .unwrap(),
        0,
        "the stale cache survived the bump"
    );

    index_gallery(&conn, &root);
    let added: i64 = conn
        .query_row(
            "SELECT date_added FROM media_meta WHERE path = '2026/january/sunset.jpg'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(added, 1588636800, "date_added did not survive the rebuild");
    assert_eq!(
        index::tags_for_file(&conn, &RelPath::new("2026/january/sunset.jpg").unwrap())
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn a_companion_written_by_another_process_is_picked_up_on_the_next_pass() {
    // The `index_state` gate has to notice a rewrite, including one inside the
    // same second — which is why it keys on nanoseconds and size rather than
    // whole seconds.
    let g = gallery();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = Root::open(g.path()).unwrap();
    let cache = db::CacheDb::open_at(cache_dir.path()).unwrap();
    let conn = cache.writer_blocking();
    index_gallery(&conn, &root);

    let media = g.path().join("untagged.png");
    let before = index::load_state(&conn).unwrap().get("untagged.png").copied();
    assert!(before.is_none(), "a file with no companion has no index state");

    modify_companion(&media, MediaType::Image, |c: &mut CompanionFile| {
        c.tags.user = vec!["late-arrival".into()];
        Outcome::Write(())
    })
    .unwrap();

    index_gallery(&conn, &root);
    let tags = index::tags_for_file(&conn, &RelPath::new("untagged.png").unwrap()).unwrap();
    assert_eq!(tags, vec![("user".to_string(), "late-arrival".to_string())]);
    assert!(index::load_state(&conn).unwrap().contains_key("untagged.png"));
}
