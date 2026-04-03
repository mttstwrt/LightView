use criterion::{black_box, criterion_group, criterion_main, Criterion};

use lightview_lib::cache::db::CacheDb;

fn create_test_db() -> (tempfile::TempDir, CacheDb) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = CacheDb::open(dir.path()).expect("open cache db");
    (dir, db)
}

fn bench_open_db(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let _ = CacheDb::open(dir.path()).expect("initial open");

    c.bench_function("cache_db/open", |b| {
        b.iter(|| CacheDb::open(black_box(dir.path())).expect("open"))
    });
}

fn bench_upsert_thumbnail(c: &mut Criterion) {
    let (_dir, db) = create_test_db();
    let thumb_data = vec![0xFFu8; 30_000];
    let mut counter = 0u64;

    c.bench_function("cache_db/upsert_thumbnail", |b| {
        b.iter(|| {
            counter += 1;
            let path = format!("/fake/image_{}.jpg", counter);
            db.upsert_thumbnail(
                black_box(&path),
                black_box("image"),
                black_box(1234567890),
                black_box(400),
                black_box(400),
                black_box(&thumb_data),
                black_box("jpeg"),
                black_box("nearest"),
            )
            .expect("upsert thumbnail");
        })
    });
}

fn bench_get_thumbnail(c: &mut Criterion) {
    let (_dir, db) = create_test_db();
    let thumb_data = vec![0xFFu8; 30_000];

    // Pre-populate
    for i in 0..1000 {
        let path = format!("/fake/image_{}.jpg", i);
        db.upsert_thumbnail(&path, "image", 1234567890, 400, 400, &thumb_data, "jpeg", "nearest")
            .expect("insert");
    }

    c.bench_function("cache_db/get_thumbnail_hit", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let path = format!("/fake/image_{}.jpg", i % 1000);
            i += 1;
            db.get_thumbnail(black_box(&path)).expect("get thumbnail")
        })
    });

    c.bench_function("cache_db/get_thumbnail_miss", |b| {
        b.iter(|| {
            db.get_thumbnail(black_box("/nonexistent/path.jpg"))
                .expect("get thumbnail")
        })
    });
}

fn bench_query_tag(c: &mut Criterion) {
    let (_dir, db) = create_test_db();

    // Pre-populate tags using raw SQL since reindex_tags_for_file needs CompanionFile
    for i in 0..500 {
        let path = format!("/fake/image_{}.jpg", i);
        db.conn()
            .execute(
                "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, "general", "landscape"],
            )
            .expect("insert tag");
        db.conn()
            .execute(
                "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, "general", "outdoor"],
            )
            .expect("insert tag");
        db.conn()
            .execute(
                "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, "rating", "safe"],
            )
            .expect("insert tag");
    }

    let mut group = c.benchmark_group("cache_db/tags");

    group.bench_function("query_tag", |b| {
        b.iter(|| {
            db.query_tag(black_box("general"), black_box("landscape"))
                .expect("query tag")
        })
    });

    group.bench_function("get_tags_for_file", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let path = format!("/fake/image_{}.jpg", i % 500);
            i += 1;
            db.get_tags_for_file(black_box(&path)).expect("get tags")
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_open_db,
    bench_upsert_thumbnail,
    bench_get_thumbnail,
    bench_query_tag,
);
criterion_main!(benches);
