//! SQLite CRUD for pre-rendered GIF frame atlases (`gif_atlas` table).
//!
//! An atlas is a sprite sheet (PNG) of every decoded, downscaled GIF frame
//! laid out in a grid, plus the metadata needed to play it back on a canvas:
//! frame count, per-frame pixel size, column count, and per-frame delays.
//! Keyed by `(path, tier)` and validated against the source file's mtime.

use crate::cache::db::{CacheDb, CacheError};

/// A cached GIF atlas row.
pub struct CachedGifAtlas {
    pub mtime: u64,
    pub frame_count: u32,
    pub frame_w: u32,
    pub frame_h: u32,
    pub cols: u32,
    /// Per-frame delays in milliseconds.
    pub delays: Vec<u16>,
    /// PNG sprite sheet bytes.
    pub atlas: Vec<u8>,
}

fn encode_delays(delays: &[u16]) -> String {
    delays
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_delays(s: &str) -> Vec<u16> {
    s.split(',')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<u16>().ok())
        .collect()
}

impl CacheDb {
    /// Read a cached atlas if present and still valid (mtime matches).
    pub fn get_gif_atlas(
        &self,
        path: &str,
        tier: &str,
        current_mtime: u64,
    ) -> Result<Option<CachedGifAtlas>, CacheError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT mtime, frame_count, frame_w, frame_h, cols, delays, atlas
             FROM gif_atlas WHERE path = ?1 AND tier = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![path, tier])?;
        match rows.next()? {
            Some(row) => {
                let mtime: u64 = row.get(0)?;
                if mtime != current_mtime {
                    return Ok(None);
                }
                Ok(Some(CachedGifAtlas {
                    mtime,
                    frame_count: row.get(1)?,
                    frame_w: row.get(2)?,
                    frame_h: row.get(3)?,
                    cols: row.get(4)?,
                    delays: decode_delays(&row.get::<_, String>(5)?),
                    atlas: row.get(6)?,
                }))
            }
            None => Ok(None),
        }
    }

    /// Insert or replace a cached atlas.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_gif_atlas(
        &self,
        path: &str,
        tier: &str,
        mtime: u64,
        frame_count: u32,
        frame_w: u32,
        frame_h: u32,
        cols: u32,
        delays: &[u16],
        atlas: &[u8],
    ) -> Result<(), CacheError> {
        self.conn().execute(
            "INSERT OR REPLACE INTO gif_atlas
                (path, tier, mtime, frame_count, frame_w, frame_h, cols, delays, atlas)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                path,
                tier,
                mtime,
                frame_count,
                frame_w,
                frame_h,
                cols,
                encode_delays(delays),
                atlas,
            ],
        )?;
        Ok(())
    }
}
