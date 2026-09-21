//! Perceptual-hash duplicate detection.
//!
//! Hashes come from the cached `j` tier, never from the original: those bytes
//! are already decoded and already in the database, so hashing a whole gallery
//! costs no source decodes. The hash lives in a `phash` column on `thumbs_j`,
//! so it is discarded and recomputed along with the thumbnail it describes —
//! exactly the lifetime it should have.
//!
//! **`NULL` means "not hashed", never "hashed to zero".** The hasher this
//! replaces matched on a stored codec string and fell through to
//! `hash.unwrap_or(0)`. It read a JPEG tier; the `j` tier is WebP. Ported
//! unchanged, every row would have stored the sentinel `0`, every row would
//! have passed `WHERE phash IS NOT NULL`, `hamming(0, 0)` is `0`, and the
//! all-pairs loop would have unioned **the entire library into one duplicate
//! group** — with no error, no log line and no failing test, for the merge to
//! then trash. A genuinely flat image legitimately hashes to 0, which is why
//! the two cases cannot share a value.
//!
//! Grouping is an all-pairs Hamming comparison, quadratic in the number of
//! hashed files. That is acceptable at gallery scale and is why `threshold` is
//! a parameter for precision rather than for cost: a tighter threshold is not
//! cheaper.
//!
//! **"Not a duplicate" is not stored.** Two files sharing any `set::` tag are
//! never offered as a pair — derived from co-membership rather than from a
//! table of pairwise verdicts, so a forty-frame burst costs forty tag rows
//! instead of 780 pairwise ones and the user sees a name rather than a list of
//! negations. The accepted cost: two identical scans inside a 200-page comic
//! will not be found, because they share the comic's set.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::cache::db::CacheError;
use crate::cache::tiers::ThumbTier;
use crate::path::RelPath;

/// Compute a 64-bit difference hash (dHash) from raw RGBA pixels.
///
/// 1. Downscale to 9×8 greyscale (72 pixels)
/// 2. Compare each pixel to its right neighbour in each row
/// 3. Produce 8×8 = 64 bits
///
/// The input is `width × height` RGBA8 pixels. Downsampling to 9×8 regardless
/// is why the move from square-cropped to aspect-preserving source bytes does
/// not change what this measures.
pub fn dhash_rgba(rgba: &[u8], width: u32, height: u32) -> Option<u64> {
    if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
        return None;
    }
    let mut grey = [0u8; 9 * 8];
    for gy in 0..8u32 {
        let src_y = (gy * height / 8).min(height - 1);
        for gx in 0..9u32 {
            let src_x = (gx * width / 9).min(width - 1);
            let idx = ((src_y * width + src_x) * 4) as usize;
            // Luma ≈ 0.299R + 0.587G + 0.114B, integer approximation.
            let r = rgba[idx] as u32;
            let g = rgba[idx + 1] as u32;
            let b = rgba[idx + 2] as u32;
            grey[(gy * 9 + gx) as usize] = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
        }
    }

    let mut hash: u64 = 0;
    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if grey[y * 9 + x] < grey[y * 9 + x + 1] {
                hash |= 1;
            }
        }
    }
    Some(hash)
}

/// Hamming distance between two hashes.
#[inline]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// `j`-tier rows with no hash yet, with their bytes, up to `limit`.
///
/// Returns the bytes rather than hashing here, so the decode happens on the
/// caller's thread and **outside the writer lock**. The loop this replaces held
/// the writer across 64 image decodes per acquisition, which blocked every
/// thumbnail the grid was waiting on.
pub fn unhashed(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<(RelPath, Vec<u8>)>, CacheError> {
    let sql = format!(
        "SELECT path, bytes FROM {} WHERE phash IS NULL LIMIT ?1",
        ThumbTier::J.table()
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (path, bytes) = row?;
        out.push((RelPath::new(&path)?, bytes));
    }
    Ok(out)
}

/// Store a batch of computed hashes. `None` records a decode that failed, so
/// the row is not re-selected forever, and keeps `IS NOT NULL` meaningful.
///
/// A failed decode stores `NULL`; the row is skipped on the next pass by the
/// `LIMIT` moving past it rather than by a sentinel, so a permanently
/// undecodable file is retried on a later backfill. That is the right trade at
/// this cost: the alternative is a third state to model.
pub fn set_phashes(
    conn: &Connection,
    hashes: &[(RelPath, Option<u64>)],
) -> Result<(), CacheError> {
    let sql = format!(
        "UPDATE {} SET phash = ?1 WHERE path = ?2",
        ThumbTier::J.table()
    );
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(&sql)?;
        for (path, hash) in hashes {
            stmt.execute(rusqlite::params![hash.map(|h| h as i64), path.as_str()])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// One image within a duplicate group.
#[derive(Debug, serde::Serialize)]
pub struct DuplicateItem {
    pub path: RelPath,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: u64,
    pub date_taken: Option<i64>,
    /// The recommended keeper: highest resolution, ties broken by smallest
    /// file.
    pub is_best: bool,
}

/// A group of visually similar images.
#[derive(Debug, serde::Serialize)]
pub struct DuplicateGroup {
    pub items: Vec<DuplicateItem>,
    /// The shared (representative) hash.
    pub hash: u64,
}

/// What the panel shows for one file, alongside its group.
type DisplayMeta = (Option<u32>, Option<u32>, u64, Option<i64>);

/// Everything the finder needs out of the database, read in three queries.
///
/// Separated from the loop so the caller can release the connection before
/// running the quadratic part on a blocking thread.
pub struct DuplicateInput {
    entries: Vec<(RelPath, u64)>,
    /// Interned set ids per entry, in the same positions as `entries`.
    sets: Vec<Vec<u32>>,
    meta: HashMap<String, DisplayMeta>,
}

/// Load hashes, set membership and the metadata the panel displays.
pub fn load(conn: &Connection) -> Result<DuplicateInput, CacheError> {
    let sql = format!(
        "SELECT path, phash FROM {} WHERE phash IS NOT NULL",
        ThumbTier::J.table()
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut entries: Vec<(RelPath, u64)> = Vec::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (path, hash) = row?;
        entries.push((RelPath::new(&path)?, hash as u64));
    }

    // Positions once, so the inner loop compares integers. Probing a set keyed
    // by paths meant building an owned `(String, String)` per near-match — two
    // allocations and a string comparison per candidate. That was tolerable
    // when the check almost never fired; set co-membership is the common case,
    // so it is now the inner loop of the one quadratic algorithm in the tree.
    let position: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.as_str(), i))
        .collect();

    let mut set_ids: HashMap<String, u32> = HashMap::new();
    let mut sets: Vec<Vec<u32>> = vec![Vec::new(); entries.len()];
    for (path, tag) in crate::cache::index::set_membership(conn)? {
        let Some(&i) = position.get(path.as_str()) else {
            continue;
        };
        let next = set_ids.len() as u32;
        let id = *set_ids.entry(tag).or_insert(next);
        sets[i].push(id);
    }
    for list in &mut sets {
        list.sort_unstable();
    }

    let mut meta = HashMap::new();
    let mut meta_stmt =
        conn.prepare("SELECT path, width, height, file_size, date_taken FROM media_meta")?;
    let rows = meta_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (
                r.get::<_, Option<u32>>(1)?,
                r.get::<_, Option<u32>>(2)?,
                r.get::<_, u64>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ),
        ))
    })?;
    for row in rows {
        let (path, m) = row?;
        meta.insert(path, m);
    }

    Ok(DuplicateInput {
        entries,
        sets,
        meta,
    })
}

/// Group near-identical images. Pure and CPU-bound — run it on a blocking
/// thread, with no database connection held.
pub fn group(input: &DuplicateInput, threshold: u32) -> Vec<DuplicateGroup> {
    let n = input.entries.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] != i {
            parent[i] = find(parent, parent[i]);
        }
        parent[i]
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[rb] = ra;
        }
    }

    for i in 0..n {
        for j in (i + 1)..n {
            if hamming_distance(input.entries[i].1, input.entries[j].1) > threshold {
                continue;
            }
            if shares_a_set(&input.sets[i], &input.sets[j]) {
                continue;
            }
            union(&mut parent, i, j);
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }

    groups
        .into_values()
        .filter(|members| members.len() > 1)
        .map(|members| {
            let hash = input.entries[members[0]].1;
            let mut items: Vec<DuplicateItem> = members
                .into_iter()
                .map(|i| {
                    let path = input.entries[i].0.clone();
                    let (width, height, file_size, date_taken) = input
                        .meta
                        .get(path.as_str())
                        .copied()
                        .unwrap_or((None, None, 0, None));
                    DuplicateItem {
                        path,
                        width,
                        height,
                        file_size,
                        date_taken,
                        is_best: false,
                    }
                })
                .collect();

            if let Some(best) = items
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| {
                    let res_a = a.width.unwrap_or(0) as u64 * a.height.unwrap_or(0) as u64;
                    let res_b = b.width.unwrap_or(0) as u64 * b.height.unwrap_or(0) as u64;
                    res_a.cmp(&res_b).then_with(|| b.file_size.cmp(&a.file_size))
                })
                .map(|(i, _)| i)
            {
                items[best].is_best = true;
            }

            DuplicateGroup { items, hash }
        })
        .collect()
}

/// Intersection of two small sorted lists of interned set ids.
///
/// `Vec<Vec<u32>>` rather than a small-vector crate: an empty `Vec` does not
/// allocate, and the overwhelmingly common case is a file in no set at all.
fn shares_a_set(a: &[u32], b: &[u32]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => return true,
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::db::CacheDb;

    fn rgba(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let l = f(x, y);
                v.extend_from_slice(&[l, l, l, 255]);
            }
        }
        v
    }

    #[test]
    fn a_flat_image_hashes_to_zero_and_that_is_a_real_hash() {
        // Which is the whole reason a failure must be NULL rather than 0.
        let flat = rgba(16, 16, |_, _| 128);
        assert_eq!(dhash_rgba(&flat, 16, 16), Some(0));
    }

    #[test]
    fn a_gradient_and_its_inverse_are_far_apart() {
        let up = rgba(16, 16, |x, _| (x * 16) as u8);
        let down = rgba(16, 16, |x, _| (255 - x * 16) as u8);
        let a = dhash_rgba(&up, 16, 16).unwrap();
        let b = dhash_rgba(&down, 16, 16).unwrap();
        assert!(hamming_distance(a, b) > 32, "distance {}", hamming_distance(a, b));
    }

    #[test]
    fn malformed_input_is_none_not_a_panic() {
        assert_eq!(dhash_rgba(&[], 0, 0), None);
        assert_eq!(dhash_rgba(&[0, 0, 0, 0], 16, 16), None);
    }

    fn seed(conn: &rusqlite::Connection, path: &str, phash: Option<i64>) {
        conn.execute(
            "INSERT INTO media_meta (path, media_type, file_size, mtime, width, height)
             VALUES (?1, 'image', 100, 1, 10, 10)",
            [path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thumbs_j (path, bytes, byte_len, accessed_at, phash)
             VALUES (?1, x'00', 1, 1, ?2)",
            rusqlite::params![path, phash],
        )
        .unwrap();
    }

    #[test]
    fn unrelated_photos_do_not_group() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        seed(&conn, "a.jpg", Some(0x0F0F_0F0F_0F0F_0F0Fu64 as i64));
        seed(&conn, "b.jpg", Some(0xF0F0_F0F0_F0F0_F0F0u64 as i64));

        let input = load(&conn).unwrap();
        assert!(group(&input, 8).is_empty());
    }

    #[test]
    fn identical_hashes_group() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        seed(&conn, "a.jpg", Some(42));
        seed(&conn, "b.jpg", Some(42));

        let input = load(&conn).unwrap();
        let groups = group(&input, 0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 2);
        assert_eq!(groups[0].items.iter().filter(|i| i.is_best).count(), 1);
    }

    #[test]
    fn an_unhashed_row_is_excluded_rather_than_matching_everything() {
        // The failure this pins: a sentinel 0 passes `IS NOT NULL`, and
        // hamming(0,0) = 0, so every unhashable row joins every other one.
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        seed(&conn, "a.jpg", None);
        seed(&conn, "b.jpg", None);
        seed(&conn, "c.jpg", Some(1));

        let input = load(&conn).unwrap();
        assert_eq!(input.entries.len(), 1);
        assert!(group(&input, 8).is_empty());
    }

    #[test]
    fn two_files_sharing_a_set_are_never_offered_as_a_pair() {
        // The one piece of genuinely new logic sitting where a user's
        // judgement is stored — cheap to test, expensive to get wrong quietly.
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        seed(&conn, "burst1.jpg", Some(7));
        seed(&conn, "burst2.jpg", Some(7));
        seed(&conn, "other.jpg", Some(7));

        for p in ["burst1.jpg", "burst2.jpg"] {
            conn.execute(
                "INSERT INTO tag_index (path, namespace, tag) VALUES (?1, 'set', 'burst-3')",
                [p],
            )
            .unwrap();
        }

        let input = load(&conn).unwrap();
        let groups = group(&input, 0);

        // All three are identical, but the two burst frames must not be
        // offered *to each other*; the third still bridges them, which is the
        // documented behaviour of a union over pairwise suppression.
        assert_eq!(groups.len(), 1);
        let paths: Vec<&str> = groups[0].items.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"other.jpg"));

        // With the bridging file gone, the pair is fully suppressed.
        conn.execute("DELETE FROM thumbs_j WHERE path = 'other.jpg'", []).unwrap();
        let input = load(&conn).unwrap();
        assert!(group(&input, 0).is_empty());
    }

    #[test]
    fn set_membership_uses_interned_ids_not_paths() {
        assert!(shares_a_set(&[1, 4, 9], &[4]));
        assert!(!shares_a_set(&[1, 4, 9], &[2, 5]));
        assert!(!shares_a_set(&[], &[1]));
    }
}
