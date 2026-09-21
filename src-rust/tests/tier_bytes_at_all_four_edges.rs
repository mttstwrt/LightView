//! Step 3's acceptance: tier bytes appear for a test gallery at all four
//! edges, through one render path, and the coalescer lets exactly one
//! generator through per key.

use std::sync::Arc;

use lightview::cache::db::CacheDb;
use lightview::cache::meta::{self, ScannedFile};
use lightview::cache::tiers::{self, ThumbTier};
use lightview::path::{RelPath, Root};
use lightview::pipeline::serve::{Outcome, ThumbService};

/// A 900x600 gallery image — wide enough that every tier has to downscale, so
/// the aspect-ratio assertion means something at all four rungs.
fn gallery() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("2026")).unwrap();
    let mut img = image::RgbImage::new(900, 600);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
    }
    img.save(d.path().join("2026/wide.png")).unwrap();
    d
}

async fn service(gallery: &std::path::Path, cache_dir: &std::path::Path) -> Arc<ThumbService> {
    let root = Root::open(gallery).unwrap();
    let db = Arc::new(CacheDb::open_at(cache_dir).unwrap());
    {
        let conn = db.writer().await;
        meta::insert_scanned(
            &conn,
            &[ScannedFile {
                path: RelPath::new("2026/wide.png").unwrap(),
                media_type: "image",
                file_size: 1,
                mtime: 1,
            }],
        )
        .unwrap();
    }
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap(),
    );
    Arc::new(ThumbService::new(db, root, pool, 64 * 1024 * 1024))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tier_generates_stores_and_reads_back() {
    let g = gallery();
    let c = tempfile::tempdir().unwrap();
    let svc = service(g.path(), c.path()).await;
    let path = RelPath::new("2026/wide.png").unwrap();

    for tier in ThumbTier::ALL {
        let bytes = match svc.get_or_generate(tier, &path, true).await {
            Outcome::Hit(b) => b,
            Outcome::Miss => panic!("{tier:?} produced no bytes"),
        };
        assert!(!bytes.is_empty(), "{tier:?} produced an empty blob");

        // One family, one encoder: every tier is WebP. The RIFF/WEBP magic is
        // what a browser sniffs, and it is what the duplicate hasher's decoder
        // has to cope with — the failure that check exists for is a hasher
        // that assumed JPEG.
        assert_eq!(&bytes[0..4], b"RIFF", "{tier:?} is not a RIFF container");
        assert_eq!(&bytes[8..12], b"WEBP", "{tier:?} is not WebP");

        // Aspect preserved, never upscaled past the source.
        let decoded = image::load_from_memory(&bytes).expect("decodes");
        let (w, h) = (decoded.width(), decoded.height());
        let expected_w = tier.edge().min(900);
        assert_eq!(w, expected_w, "{tier:?} width");
        assert_eq!(h, (expected_w as f64 * 600.0 / 900.0).round() as u32, "{tier:?} height");

        // And it is in the tier's own table, not some other tier's.
        let conn = svc.db.writer().await;
        assert!(
            tiers::get(&conn, tier, &path).unwrap().is_some(),
            "{tier:?} row missing"
        );
        assert!(tiers::total_bytes(&conn, tier).unwrap() > 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_j_tier_writes_the_thumbhash_and_the_source_dimensions() {
    // The ThumbHash is what lets a client paint every cell blurry before any
    // thumbnail request goes out. Deriving it later from the stored bytes would
    // mean a second decode and a first paint with no placeholders at all.
    let g = gallery();
    let c = tempfile::tempdir().unwrap();
    let svc = service(g.path(), c.path()).await;
    let path = RelPath::new("2026/wide.png").unwrap();

    svc.get_or_generate(ThumbTier::J, &path, true).await;

    let conn = svc.db.writer().await;
    let (hash, w, h): (Option<Vec<u8>>, Option<u32>, Option<u32>) = conn
        .query_row(
            "SELECT thumbhash, width, height FROM media_meta WHERE path = ?1",
            [path.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(hash.is_some_and(|h| !h.is_empty()), "no ThumbHash stored");
    // Source dimensions, not the tier's — this is what the grid lays out from.
    assert_eq!((w, h), (Some(900), Some(600)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_for_one_key_produce_one_row() {
    let g = gallery();
    let c = tempfile::tempdir().unwrap();
    let svc = service(g.path(), c.path()).await;
    let path = RelPath::new("2026/wide.png").unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let svc = svc.clone();
        let path = path.clone();
        tasks.push(tokio::spawn(async move {
            matches!(
                svc.get_or_generate(ThumbTier::Jm, &path, true).await,
                Outcome::Hit(_)
            )
        }));
    }
    for t in tasks {
        assert!(t.await.unwrap(), "a coalesced waiter came back empty-handed");
    }

    let conn = svc.db.writer().await;
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM thumbs_jm", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_source_is_a_miss_rather_than_a_hang() {
    // The bounded-retry half of the coalescer: a source that cannot be
    // generated must degrade to a miss instead of spinning or wedging the key.
    let g = gallery();
    let c = tempfile::tempdir().unwrap();
    let svc = service(g.path(), c.path()).await;
    let ghost = RelPath::new("2026/does-not-exist.png").unwrap();

    assert!(matches!(
        svc.get_or_generate(ThumbTier::J, &ghost, true).await,
        Outcome::Miss
    ));
    // And the key is free afterwards, so a later real request is not blocked.
    assert!(matches!(
        svc.get_or_generate(ThumbTier::J, &ghost, true).await,
        Outcome::Miss
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_backfill_does_not_mark_the_gallery_busy() {
    // If the idle worker marked activity, it would decide on its next tick
    // that somebody was looking, and never run again.
    let g = gallery();
    let c = tempfile::tempdir().unwrap();
    let svc = service(g.path(), c.path()).await;
    let path = RelPath::new("2026/wide.png").unwrap();

    // The clock has whole-second granularity, so the assertions use the real
    // 60-second window rather than a one-second one a slow generation could
    // cross on its own.
    svc.get_or_generate(ThumbTier::J, &path, false).await;
    assert!(svc.activity.idle_for(60), "a backfill request marked activity");

    svc.get_or_generate(ThumbTier::Js, &path, true).await;
    assert!(!svc.activity.idle_for(60), "a user request did not mark activity");
}
