//! Which views a gallery offers, and the thumbnail tiers they need pre-warmed.
//!
//! Enablement and generation are one setting on purpose. The idle worker used
//! to pre-warm the square *and* the justified tier for every gallery, so a
//! library browsed only in the justified layout still paid the full square-tier
//! cost — gigabytes on a large collection, for cells nobody would ever render —
//! and the reverse held too. Reading the enabled list here means the tiers a
//! gallery generates are exactly the tiers its views ask for.
//!
//! Disabling a view **does not delete** its cached rows. Re-enabling must not
//! re-decode the whole library, and the zoomed-in tiers already have an LRU
//! byte budget (`cache::thumbnails::is_lru_capped`) that sheds the genuinely
//! stale ones. What stops is *generation*.
//!
//! The setting lives in `gallery_meta` rather than in `.lightview/settings.toml`
//! with the display preferences: the web client persists its copy of that file's
//! contents per browser in local storage, and which views exist is a property of
//! the gallery, not of the device looking at it. It is read by
//! [`crate::pipeline::idle`] and by the frontend (`get_enabled_views`); only the
//! desktop may write it.

use rusqlite::{params, Connection, OptionalExtension};

use crate::cache::thumbnails::ThumbTier;

/// `gallery_meta` key holding the comma-separated list of enabled views.
const META_ENABLED_VIEWS: &str = "views.enabled";

/// A browsable layout the gallery can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Uniform square cells.
    Grid,
    /// Aspect-preserving justified rows.
    Justified,
    /// Geographic map.
    Map,
}

impl View {
    pub const ALL: [View; 3] = [View::Grid, View::Justified, View::Map];

    pub fn as_str(self) -> &'static str {
        match self {
            View::Grid => "grid",
            View::Justified => "justified",
            View::Map => "map",
        }
    }

    pub fn parse(s: &str) -> Option<View> {
        View::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// The tier the idle worker pre-warms so this view's first screen is
    /// already there. `None` means the view needs no backfill: the map draws
    /// one micro thumbnail per cluster, a few dozen images that generate on
    /// demand faster than a backlog pass would reach them.
    fn prewarm_tier(self) -> Option<ThumbTier> {
        match self {
            View::Grid => Some(ThumbTier::Standard),
            View::Justified => Some(ThumbTier::Justified),
            View::Map => None,
        }
    }
}

/// The gallery's enabled views. A gallery that has never had the setting
/// written offers all of them, which is what every gallery did before the
/// setting existed.
///
/// Unknown names in the stored list are ignored rather than rejected, so a
/// gallery configured by a newer build still opens here. An empty result is
/// meaningful — the user turned everything off — and is only produced by an
/// explicitly empty stored value.
pub fn enabled(conn: &Connection) -> Vec<View> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM gallery_meta WHERE key = ?1",
            params![META_ENABLED_VIEWS],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);

    match stored {
        Some(list) => list.split(',').filter_map(|s| View::parse(s.trim())).collect(),
        None => View::ALL.to_vec(),
    }
}

pub fn set_enabled(conn: &Connection, views: &[View]) -> rusqlite::Result<()> {
    let list = views.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(",");
    conn.execute(
        "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
        params![META_ENABLED_VIEWS, list],
    )?;
    Ok(())
}

/// Tiers to pre-warm for this gallery, in [`View::ALL`] order and without
/// duplicates. Empty when no enabled view needs a backfill, which the idle
/// worker treats as "nothing to do" rather than as an error.
pub fn prewarm_tiers(conn: &Connection) -> Vec<ThumbTier> {
    let mut tiers: Vec<ThumbTier> = Vec::new();
    for tier in enabled(conn).into_iter().filter_map(View::prewarm_tier) {
        if !tiers.contains(&tier) {
            tiers.push(tier);
        }
    }
    tiers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::db::CacheDb;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, CacheDb) {
        let dir = TempDir::new().expect("tempdir");
        let db = CacheDb::open(dir.path()).expect("open cache db");
        (dir, db)
    }

    #[test]
    fn unset_gallery_offers_every_view() {
        let (_dir, db) = open_db();
        assert_eq!(enabled(db.conn()), View::ALL.to_vec());
        assert_eq!(
            prewarm_tiers(db.conn()),
            vec![ThumbTier::Standard, ThumbTier::Justified]
        );
    }

    #[test]
    fn disabled_grid_stops_warming_the_square_tier() {
        let (_dir, db) = open_db();
        set_enabled(db.conn(), &[View::Justified, View::Map]).unwrap();
        assert_eq!(enabled(db.conn()), vec![View::Justified, View::Map]);
        assert_eq!(prewarm_tiers(db.conn()), vec![ThumbTier::Justified]);
    }

    #[test]
    fn map_only_gallery_warms_nothing() {
        let (_dir, db) = open_db();
        set_enabled(db.conn(), &[View::Map]).unwrap();
        assert!(prewarm_tiers(db.conn()).is_empty());
    }

    #[test]
    fn everything_off_is_distinct_from_never_set() {
        let (_dir, db) = open_db();
        set_enabled(db.conn(), &[]).unwrap();
        assert!(enabled(db.conn()).is_empty());
    }

    #[test]
    fn a_name_this_build_does_not_know_is_skipped() {
        let (_dir, db) = open_db();
        db.set_gallery_meta(META_ENABLED_VIEWS, "grid,spiral").unwrap();
        assert_eq!(enabled(db.conn()), vec![View::Grid]);
    }
}
