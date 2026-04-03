use criterion::{black_box, criterion_group, criterion_main, Criterion};

use lightview_lib::cache::atlas::ThumbAtlas;

/// Create a temp dir with a `.lightview/` subdirectory and open an atlas in it.
fn create_test_atlas() -> (tempfile::TempDir, ThumbAtlas) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let lv_dir = dir.path().join(".lightview");
    std::fs::create_dir_all(&lv_dir).expect("create .lightview dir");
    let atlas = ThumbAtlas::open(&lv_dir).expect("open atlas");
    (dir, atlas)
}

fn bench_atlas_open(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let lv_dir = dir.path().join(".lightview");
    std::fs::create_dir_all(&lv_dir).expect("create dir");
    let _ = ThumbAtlas::open(&lv_dir).expect("initial open");

    c.bench_function("atlas/open", |b| {
        b.iter(|| ThumbAtlas::open(black_box(&lv_dir)).expect("open"))
    });
}

fn bench_atlas_upsert(c: &mut Criterion) {
    let (_dir, mut atlas) = create_test_atlas();

    // Create fake RGBA data for a 400x400 thumbnail
    let rgba_data = vec![0x80u8; 400 * 400 * 4];
    let mut counter = 0u64;

    c.bench_function("atlas/upsert", |b| {
        b.iter(|| {
            counter += 1;
            let path = format!("/fake/image_{}.jpg", counter);
            atlas
                .upsert(
                    black_box(&path),
                    black_box(&rgba_data),
                    black_box(400),
                    black_box(400),
                    black_box(1234567890),
                )
                .expect("upsert to atlas")
        })
    });
}

fn bench_atlas_upsert_bc7_raw(c: &mut Criterion) {
    let (_dir, mut atlas) = create_test_atlas();

    // Pre-encoded BC7 data (16 bytes per 4x4 block, 100x100 blocks for 400x400)
    let bc7_data = vec![0xABu8; 100 * 100 * 16];
    let mut counter = 0u64;

    c.bench_function("atlas/upsert_bc7_raw", |b| {
        b.iter(|| {
            counter += 1;
            let path = format!("/fake/image_{}.jpg", counter);
            atlas
                .upsert_bc7_raw(
                    black_box(&path),
                    black_box(&bc7_data),
                    black_box(400),
                    black_box(400),
                    black_box(1234567890),
                )
                .expect("upsert raw bc7")
        })
    });
}

fn bench_atlas_read(c: &mut Criterion) {
    let (_dir, mut atlas) = create_test_atlas();
    let rgba_data = vec![0x80u8; 400 * 400 * 4];

    // Pre-populate
    for i in 0..500 {
        let path = format!("/fake/image_{}.jpg", i);
        atlas.upsert(&path, &rgba_data, 400, 400, 1234567890).expect("upsert");
    }
    atlas.flush_index().expect("flush");
    atlas.remap().expect("remap");

    c.bench_function("atlas/read_bc7_hit", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let path = format!("/fake/image_{}.jpg", i % 500);
            i += 1;
            atlas.read_bc7(black_box(&path))
        })
    });

    c.bench_function("atlas/read_bc7_miss", |b| {
        b.iter(|| atlas.read_bc7(black_box("/nonexistent/path.jpg")))
    });

    c.bench_function("atlas/is_valid_hit", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let path = format!("/fake/image_{}.jpg", i % 500);
            i += 1;
            atlas.is_valid(black_box(&path), black_box(1234567890))
        })
    });
}

criterion_group!(
    benches,
    bench_atlas_open,
    bench_atlas_upsert,
    bench_atlas_upsert_bc7_raw,
    bench_atlas_read,
);
criterion_main!(benches);
