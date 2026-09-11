//! `media_meta`: one row per media file, and the mirror of the companion
//! fields the query language needs as columns.
//!
//! **A field is filterable only if it is indexed**, which is why `rating` and
//! `color_label` live here at all — they are the companion's, mirrored in. The
//! direction of that mirror matters and is stated once, here: at index time the
//! **companion wins whenever the field is present**, and the database value is
//! written back into the companion only when it is absent. Mirrored the other
//! way, the first machine to open a gallery with a cold cache would stamp its
//! own `now` onto every file's `date_added` in the durable tree — routinely the
//! desktop running `lightview tag` — which is precisely the loss the mirroring
//! exists to prevent.
//!
//! `date_added` and `last_viewed` are mirrored for a blunter reason: without
//! them in the companion, "delete everything else and reopen, and nothing is
//! lost but time" is false. Three operations the design blesses — a
//! `format_version` bump, `lightview cache --prune`, and the scan prune —
//! destroy both permanently and silently, after which "Date added" is a single
//! instant for the whole library and "Last viewed" is empty.

use rusqlite::Connection;

use crate::cache::db::CacheError;
use crate::companion::schema::{CompanionFile, Location};
use crate::path::RelPath;

/// What a scan knows about a file before anything has been decoded.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: RelPath,
    pub media_type: &'static str,
    pub file_size: i64,
    pub mtime: i64,
}

/// What the pipeline learns once it has looked inside.
#[derive(Debug, Clone, Default)]
pub struct ProbedMedia {
    pub date_taken: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<f64>,
    pub location: Option<Location>,
}

/// Insert the rows a scan found, preserving what is already known.
///
/// `date_added` is set to now **on first insert only**: it is a durable fact
/// about this gallery, and the `ON CONFLICT` clause must never touch it.
pub fn insert_scanned(conn: &Connection, files: &[ScannedFile]) -> Result<usize, CacheError> {
    let now = now();
    let tx = conn.unchecked_transaction()?;
    let mut inserted = 0;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO media_meta (path, media_type, file_size, mtime, date_added)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
               media_type = excluded.media_type,
               file_size  = excluded.file_size,
               mtime      = excluded.mtime",
        )?;
        for f in files {
            inserted += stmt.execute(rusqlite::params![
                f.path.as_str(),
                f.media_type,
                f.file_size,
                f.mtime,
                now,
            ])?;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

/// Record what a decode or probe learned.
///
/// **A placeholder must not write dimensions.** A `0×0` write fills the
/// `width IS NULL` gap that guards the column *and* hands the grid a degenerate
/// aspect ratio, so a clip that failed to thumbnail lays out as a zero-height
/// cell forever. `None` stays `None`.
///
/// A probe that found no location must not clear a stored one: phones spell the
/// container's location key three ways and a miss is a miss, not an absence.
pub fn set_probed(conn: &Connection, path: &RelPath, m: &ProbedMedia) -> Result<(), CacheError> {
    debug_assert!(
        m.width != Some(0) && m.height != Some(0),
        "a placeholder must leave dimensions NULL, not write 0"
    );
    conn.prepare_cached(
        "UPDATE media_meta SET
             date_taken = COALESCE(?2, date_taken),
             width      = COALESCE(?3, width),
             height     = COALESCE(?4, height),
             duration   = COALESCE(?5, duration),
             gps_lat    = COALESCE(?6, gps_lat),
             gps_lon    = COALESCE(?7, gps_lon)
         WHERE path = ?1",
    )?
    .execute(rusqlite::params![
        path.as_str(),
        m.date_taken,
        m.width,
        m.height,
        m.duration,
        m.location.map(|l| l.lat),
        m.location.map(|l| l.lon),
    ])?;
    Ok(())
}

/// Store the ThumbHash for a file.
pub fn set_thumbhash(conn: &Connection, path: &RelPath, hash: &[u8]) -> Result<(), CacheError> {
    conn.prepare_cached("UPDATE media_meta SET thumbhash = ?2 WHERE path = ?1")?
        .execute(rusqlite::params![path.as_str(), hash])?;
    Ok(())
}

/// Mirror a rating, stamping `last_rated`.
pub fn set_rating(conn: &Connection, path: &RelPath, rating: Option<u8>) -> Result<(), CacheError> {
    conn.prepare_cached("UPDATE media_meta SET rating = ?2, last_rated = ?3 WHERE path = ?1")?
        .execute(rusqlite::params![path.as_str(), rating, now()])?;
    Ok(())
}

/// Mirror a colour label.
///
/// Stored lowercase by every write path, so the filter's comparison is exact
/// rather than `COLLATE NOCASE` — which would not use the index.
pub fn set_color_label(
    conn: &Connection,
    path: &RelPath,
    label: Option<&str>,
) -> Result<(), CacheError> {
    let normalized = label
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty());
    conn.prepare_cached("UPDATE media_meta SET color_label = ?2 WHERE path = ?1")?
        .execute(rusqlite::params![path.as_str(), normalized])?;
    Ok(())
}

/// Record that a file was viewed.
pub fn record_view(conn: &Connection, path: &RelPath) -> Result<(), CacheError> {
    conn.prepare_cached("UPDATE media_meta SET last_viewed = ?2 WHERE path = ?1")?
        .execute(rusqlite::params![path.as_str(), now()])?;
    Ok(())
}

/// What a companion contributes to the index, and what the index owes it back.
pub struct MirrorResult {
    /// Fields the database had and the companion did not, for the caller to
    /// write back under the companion lock.
    pub missing_date_added: Option<i64>,
    pub missing_last_viewed: Option<i64>,
}

/// Mirror a companion's core fields into the row, companion-wins.
///
/// Returns what the *database* knows and the companion does not, so the caller
/// can complete the sidecar in the same pass. The caller does the writing,
/// because a write needs the companion lock and this function holds the
/// database writer.
pub fn mirror_companion(
    conn: &Connection,
    path: &RelPath,
    companion: &CompanionFile,
) -> Result<MirrorResult, CacheError> {
    let core = companion.meta.core.as_ref();
    let rating = core.and_then(|c| c.rating);
    let color = core.and_then(|c| c.color_label.clone());
    let date_added = core.and_then(|c| c.date_added.as_deref()).and_then(parse_rfc3339);
    let last_viewed = core
        .and_then(|c| c.last_viewed.as_deref())
        .and_then(parse_rfc3339);
    let last_rated = core.and_then(|c| c.date_rated.as_deref()).and_then(parse_rfc3339);
    let location = core.and_then(|c| c.location);

    let (db_added, db_viewed): (Option<i64>, Option<i64>) = conn
        .prepare_cached("SELECT date_added, last_viewed FROM media_meta WHERE path = ?1")?
        .query_row([path.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((None, None));

    conn.prepare_cached(
        "UPDATE media_meta SET
             rating      = ?2,
             color_label = ?3,
             last_rated  = COALESCE(?4, last_rated),
             date_added  = COALESCE(?5, date_added),
             last_viewed = COALESCE(?6, last_viewed),
             gps_lat     = COALESCE(?7, gps_lat),
             gps_lon     = COALESCE(?8, gps_lon)
         WHERE path = ?1",
    )?
    .execute(rusqlite::params![
        path.as_str(),
        rating,
        color.map(|c| c.trim().to_lowercase()),
        last_rated,
        date_added,
        last_viewed,
        location.map(|l| l.lat),
        location.map(|l| l.lon),
    ])?;

    Ok(MirrorResult {
        missing_date_added: if date_added.is_none() { db_added } else { None },
        missing_last_viewed: if last_viewed.is_none() { db_viewed } else { None },
    })
}

/// Every path in the index, for the sweep and for a tagging run's plan.
pub fn all_paths(conn: &Connection) -> Result<Vec<RelPath>, CacheError> {
    let mut stmt = conn.prepare("SELECT path FROM media_meta")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(RelPath::new(&row?)?);
    }
    Ok(out)
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// RFC 3339, for writing a database timestamp back into a companion.
pub fn to_rfc3339(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::db::CacheDb;
    use crate::companion::schema::{CoreMeta, MediaType};

    fn scanned(path: &str) -> ScannedFile {
        ScannedFile {
            path: RelPath::new(path).unwrap(),
            media_type: "image",
            file_size: 100,
            mtime: 42,
        }
    }

    #[test]
    fn a_rescan_does_not_move_date_added() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();

        insert_scanned(&conn, &[scanned("a.jpg")]).unwrap();
        conn.execute("UPDATE media_meta SET date_added = 1000", []).unwrap();
        insert_scanned(&conn, &[scanned("a.jpg")]).unwrap();

        let added: i64 = conn
            .query_row("SELECT date_added FROM media_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(added, 1000, "a rescan restamped date_added");
    }

    #[test]
    fn the_companion_wins_on_a_mirrored_field() {
        // The direction that matters: a cold cache must not stamp its own
        // `now` over what the durable file already says.
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        insert_scanned(&conn, &[scanned("a.jpg")]).unwrap();

        let mut c = CompanionFile::new("a.jpg", MediaType::Image);
        c.meta.core = Some(CoreMeta {
            rating: Some(4),
            date_added: Some("2020-01-01T00:00:00+00:00".into()),
            ..Default::default()
        });

        let path = RelPath::new("a.jpg").unwrap();
        let result = mirror_companion(&conn, &path, &c).unwrap();

        let (rating, added): (Option<u8>, i64) = conn
            .query_row("SELECT rating, date_added FROM media_meta", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(rating, Some(4));
        assert_eq!(added, 1577836800);
        assert!(result.missing_date_added.is_none());
    }

    #[test]
    fn the_database_completes_a_companion_that_lacks_a_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        insert_scanned(&conn, &[scanned("a.jpg")]).unwrap();
        conn.execute("UPDATE media_meta SET date_added = 1234, last_viewed = 5678", [])
            .unwrap();

        let c = CompanionFile::new("a.jpg", MediaType::Image);
        let path = RelPath::new("a.jpg").unwrap();
        let result = mirror_companion(&conn, &path, &c).unwrap();

        assert_eq!(result.missing_date_added, Some(1234));
        assert_eq!(result.missing_last_viewed, Some(5678));
    }

    #[test]
    fn a_probe_never_clears_what_it_did_not_find() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        insert_scanned(&conn, &[scanned("clip.mp4")]).unwrap();
        let path = RelPath::new("clip.mp4").unwrap();

        set_probed(
            &conn,
            &path,
            &ProbedMedia {
                width: Some(1920),
                height: Some(1080),
                location: Some(Location { lat: 1.0, lon: 2.0, alt: None }),
                ..Default::default()
            },
        )
        .unwrap();

        // A second probe with no location must leave the stored one alone.
        set_probed(&conn, &path, &ProbedMedia { duration: Some(3.5), ..Default::default() })
            .unwrap();

        let (w, lat, dur): (Option<u32>, Option<f64>, Option<f64>) = conn
            .query_row("SELECT width, gps_lat, duration FROM media_meta", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(w, Some(1920));
        assert_eq!(lat, Some(1.0));
        assert_eq!(dur, Some(3.5));
    }
}
