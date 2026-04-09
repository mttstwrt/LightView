use crate::cache::db::CacheDb;

/// Compute a 64-bit difference hash (dHash) from raw RGBA pixel data.
///
/// Steps:
///   1. Downscale to 9×8 grayscale (72 pixels)
///   2. Compare each pixel to its right neighbour in each row
///   3. Produce 8×8 = 64 bits
///
/// The input is assumed to be `width × height` RGBA8 pixels (4 bytes each).
pub fn dhash_rgba(rgba: &[u8], width: u32, height: u32) -> u64 {
    // Downscale to 9×8 grayscale using nearest-neighbour sampling.
    let mut grey = [0u8; 9 * 8];
    for gy in 0..8u32 {
        let src_y = (gy * height / 8).min(height - 1);
        for gx in 0..9u32 {
            let src_x = (gx * width / 9).min(width - 1);
            let idx = ((src_y * width + src_x) * 4) as usize;
            // Luma ≈ 0.299R + 0.587G + 0.114B (integer approximation)
            let r = rgba[idx] as u32;
            let g = rgba[idx + 1] as u32;
            let b = rgba[idx + 2] as u32;
            grey[(gy * 9 + gx) as usize] = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
        }
    }

    // Build the 64-bit hash: for each row, compare adjacent columns.
    let mut hash: u64 = 0;
    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if grey[y * 9 + x] < grey[y * 9 + x + 1] {
                hash |= 1;
            }
        }
    }
    hash
}

/// Compute dHash from JPEG-compressed thumbnail bytes.
/// Decodes to RGB, then converts to the same grey pipeline.
pub fn dhash_jpeg(jpeg_data: &[u8]) -> Option<u64> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg_data));
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let width = info.width as u32;
    let height = info.height as u32;

    // Convert RGB to pseudo-RGBA for the shared dhash function
    let component_count = pixels.len() / (width * height) as usize;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for chunk in pixels.chunks_exact(component_count) {
        rgba.push(chunk[0]); // R
        rgba.push(if component_count > 1 { chunk[1] } else { chunk[0] }); // G
        rgba.push(if component_count > 2 { chunk[2] } else { chunk[0] }); // B
        rgba.push(255); // A
    }

    Some(dhash_rgba(&rgba, width, height))
}

/// Hamming distance between two 64-bit hashes.
#[inline]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Metadata for a single image within a duplicate group.
#[derive(Debug, serde::Serialize)]
pub struct DuplicateItem {
    pub path: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: u64,
    /// True if this is the recommended "best" version in the group.
    pub is_best: bool,
}

/// A group of duplicate images.
#[derive(Debug, serde::Serialize)]
pub struct DuplicateGroup {
    /// Items in this group with metadata.
    pub items: Vec<DuplicateItem>,
    /// The shared (or representative) perceptual hash.
    pub hash: u64,
}

impl CacheDb {
    /// Compute and store the perceptual hash for all thumbnails that don't have one yet.
    /// Returns the number of hashes computed.
    pub fn compute_phashes(&self) -> Result<usize, rusqlite::Error> {
        let mut read_stmt = self.conn().prepare(
            "SELECT path, thumbnail, format, width, height FROM thumbnails WHERE phash IS NULL",
        )?;

        let rows: Vec<(String, Vec<u8>, String, u32, u32)> = read_stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut count = 0;
        let tx = self.conn().unchecked_transaction()?;
        {
            let mut update_stmt =
                tx.prepare_cached("UPDATE thumbnails SET phash = ?1 WHERE path = ?2")?;

            for (path, data, format, width, height) in &rows {
                let hash = match format.as_str() {
                    "rgba" => Some(dhash_rgba(data, *width, *height)),
                    "jpeg" => dhash_jpeg(data),
                    _ => None,
                };
                if let Some(h) = hash {
                    update_stmt.execute(rusqlite::params![h as i64, path])?;
                    count += 1;
                }
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// Find groups of visually similar images based on perceptual hash.
    /// `threshold` is the maximum hamming distance to consider a match (0 = exact, 10 = loose).
    pub fn find_duplicates(&self, threshold: u32) -> Result<Vec<DuplicateGroup>, rusqlite::Error> {
        let mut stmt = self
            .conn()
            .prepare("SELECT path, phash FROM thumbnails WHERE phash IS NOT NULL")?;

        let entries: Vec<(String, u64)> = stmt
            .query_map([], |row| {
                let hash: i64 = row.get(1)?;
                Ok((row.get(0)?, hash as u64))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Union-Find for grouping
        let n = entries.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], i: usize) -> usize {
            if parent[i] != i {
                parent[i] = find(parent, parent[i]);
            }
            parent[i]
        }

        fn union(parent: &mut [usize], a: usize, b: usize) {
            let ra = find(parent, a);
            let rb = find(parent, b);
            if ra != rb {
                parent[rb] = ra;
            }
        }

        // Compare all pairs — O(n²) but fine for typical gallery sizes (< 100k).
        // For very large galleries a VP-tree would be better, but this is simple
        // and the hash comparison itself is a single XOR + popcount.
        for i in 0..n {
            for j in (i + 1)..n {
                if hamming_distance(entries[i].1, entries[j].1) <= threshold {
                    union(&mut parent, i, j);
                }
            }
        }

        // Collect groups
        let mut groups: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            groups.entry(root).or_default().push(i);
        }

        // Fetch resolution and file size from media_meta for all entries
        let mut meta_stmt = self.conn().prepare(
            "SELECT width, height, file_size FROM media_meta WHERE path = ?1",
        )?;

        // Only return groups with 2+ members, enriched with metadata
        let result = groups
            .into_values()
            .filter(|members| members.len() > 1)
            .map(|members| {
                let hash = entries[members[0]].1;
                let mut items: Vec<DuplicateItem> = members
                    .into_iter()
                    .map(|i| {
                        let path = entries[i].0.clone();
                        let (width, height, file_size) = meta_stmt
                            .query_row(rusqlite::params![&path], |row| {
                                Ok((
                                    row.get::<_, Option<u32>>(0)?,
                                    row.get::<_, Option<u32>>(1)?,
                                    row.get::<_, u64>(2)?,
                                ))
                            })
                            .unwrap_or((None, None, 0));
                        DuplicateItem {
                            path,
                            width,
                            height,
                            file_size,
                            is_best: false,
                        }
                    })
                    .collect();

                // Determine "best": highest resolution wins, ties broken by smallest file size
                if let Some(best_idx) = items.iter().enumerate().max_by(|(_, a), (_, b)| {
                    let res_a = a.width.unwrap_or(0) as u64 * a.height.unwrap_or(0) as u64;
                    let res_b = b.width.unwrap_or(0) as u64 * b.height.unwrap_or(0) as u64;
                    res_a
                        .cmp(&res_b)
                        .then_with(|| b.file_size.cmp(&a.file_size))
                }).map(|(i, _)| i) {
                    items[best_idx].is_best = true;
                }

                DuplicateGroup { items, hash }
            })
            .collect();

        Ok(result)
    }
}
