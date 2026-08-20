//! Per-device authentication for the LAN web interface.
//!
//! A paired device holds a cookie of the form `<device_id>.<secret>`. The
//! server stores only a SHA-256 hash of `<secret>`, keyed by `<device_id>`,
//! so a leaked database alone never grants access. SHA-256 (not argon2) is
//! the right primitive here: the secret is 32 random bytes, so brute-force
//! resistance from a slow hash buys nothing, and verification runs on every
//! thumbnail/media request — fast verify is essential.
//!
//! Enrollment goes through a short-lived `remote_pairing` row — either a
//! 6-digit PIN (typed by hand) or a 32-byte hex token (embedded in a QR
//! code). Both share the same redemption endpoint and are consumed on first
//! use.
//!
//! An optional gallery-wide password layers on top: when set, devices must
//! re-prove they hold it after `inactivity_secs` of silence. The password
//! hash and inactivity threshold live in `gallery_meta` so they travel with
//! the gallery, matching the per-gallery scope of device pairings.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lifetime of a freshly issued pairing code. Long enough for the user to
/// switch to their phone and scan/type; short enough that an unattended code
/// doesn't sit around indefinitely.
pub const PAIRING_TTL_SECS: i64 = 600; // 10 minutes

/// Default inactivity window before a device is challenged for the password.
pub const DEFAULT_INACTIVITY_SECS: i64 = 6 * 60 * 60; // 6 hours

const META_PASSWORD_HASH: &str = "remote.password_hash";
const META_INACTIVITY_SECS: &str = "remote.inactivity_secs";
const META_COOKIE_SUFFIX: &str = "remote.cookie_suffix";

/// Base name of the per-device pairing cookie, and the name galleries indexed
/// before per-gallery scoping still hold. See [`cookie_name`].
pub const DEVICE_COOKIE_BASE: &str = "lv_device";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("hashing error: {0}")]
    Hash(String),
    #[error("invalid pairing code")]
    InvalidPairing,
    #[error("pairing code already used")]
    PairingConsumed,
    #[error("pairing code expired")]
    PairingExpired,
}

impl From<argon2::password_hash::Error> for AuthError {
    fn from(e: argon2::password_hash::Error) -> Self {
        AuthError::Hash(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub last_auth_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingKind {
    Pin,
    Qr,
}

impl PairingKind {
    fn as_str(&self) -> &'static str {
        match self {
            PairingKind::Pin => "pin",
            PairingKind::Qr => "qr",
        }
    }
}

/// Result of redeeming a pairing code: a fresh device row and the raw cookie
/// value to send back to the client. The cookie is the only place the secret
/// exists in plaintext — the DB stores only its hash.
pub struct RedeemedDevice {
    pub device: DeviceRow,
    pub cookie_value: String,
}

/// Successful cookie verification.
pub struct AuthenticatedDevice {
    pub id: String,
    pub last_auth_at: i64,
    /// Carried out of the verification query so the auth path can decide
    /// whether [`touch_device`] is worth a write. See [`TOUCH_INTERVAL_SECS`].
    pub last_seen: i64,
}

/// How stale `last_seen` may get before the auth path refreshes it.
///
/// It used to be written on *every* authenticated request, and a remote
/// client's request mix is overwhelmingly `/thumb`: one scroll burst is
/// hundreds of them, each taking the single writer connection for an `UPDATE`
/// nothing reads at finer than human granularity — it surfaces as "last seen"
/// in the device list. Those writes also serialize against the read-only
/// connection pool the thumbnail route exists to use. A minute of slack turns a
/// burst into at most one write and leaves the displayed value indistinguishable.
pub const TOUCH_INTERVAL_SECS: i64 = 60;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn argon2_hash(secret: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    Ok(argon2
        .hash_password(secret.as_bytes(), &salt)?
        .to_string())
}

fn argon2_verify(secret: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .and_then(|parsed| Argon2::default().verify_password(secret.as_bytes(), &parsed))
        .is_ok()
}

fn sha256_hex(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Length-checked, constant-time string compare so an attacker can't time
/// the response to learn the prefix of a stored hash.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        acc |= x ^ y;
    }
    acc == 0
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill(&mut buf[..]);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn random_pin() -> String {
    // 6 digits — typeable, ~1M combinations, only valid for PAIRING_TTL_SECS.
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{:06}", n)
}

// ─────────────────────────────────────────────────────────────
// Pairing codes
// ─────────────────────────────────────────────────────────────

/// Create a fresh pairing code of the given kind and return the code value.
/// QR codes use a 32-byte hex token (unguessable); PINs use 6 digits and
/// rely on the short TTL + single-use redemption for safety.
pub fn create_pairing(conn: &Connection, kind: PairingKind) -> Result<String, AuthError> {
    purge_expired_pairings(conn)?;
    let code = match kind {
        PairingKind::Pin => {
            // Avoid a collision with an unredeemed PIN by retrying on conflict.
            loop {
                let candidate = random_pin();
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM remote_pairing WHERE code = ?1 AND consumed_at IS NULL",
                        params![candidate],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if !exists {
                    break candidate;
                }
            }
        }
        PairingKind::Qr => random_hex(32),
    };
    let expires_at = now_secs() + PAIRING_TTL_SECS;
    conn.execute(
        "INSERT INTO remote_pairing (code, kind, expires_at, consumed_at)
         VALUES (?1, ?2, ?3, NULL)",
        params![code, kind.as_str(), expires_at],
    )?;
    Ok(code)
}

/// Remove expired and consumed pairing rows. Cheap janitor — called whenever
/// a new code is generated so the table never grows unbounded.
pub fn purge_expired_pairings(conn: &Connection) -> Result<(), AuthError> {
    conn.execute(
        "DELETE FROM remote_pairing
         WHERE expires_at < ?1 OR consumed_at IS NOT NULL",
        params![now_secs()],
    )?;
    Ok(())
}

/// Consume a pairing code and create a new device. Returns the device row plus
/// the raw cookie value (only place the secret exists in plaintext).
pub fn redeem_pairing(
    conn: &Connection,
    code: &str,
    device_name: &str,
) -> Result<RedeemedDevice, AuthError> {
    let row: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT expires_at, consumed_at FROM remote_pairing WHERE code = ?1",
            params![code],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (expires_at, consumed_at) = row.ok_or(AuthError::InvalidPairing)?;
    if consumed_at.is_some() {
        return Err(AuthError::PairingConsumed);
    }
    if expires_at < now_secs() {
        return Err(AuthError::PairingExpired);
    }

    let now = now_secs();
    let device_id = random_hex(8); // 16 hex chars — short enough for logs, plenty of entropy
    let secret = random_hex(32);
    let token_hash = sha256_hex(&secret);
    let trimmed_name = device_name.trim();
    let name = if trimmed_name.is_empty() {
        "Unnamed device".to_string()
    } else {
        // Cap to a sane length so a hostile client can't fill the DB.
        trimmed_name.chars().take(64).collect()
    };

    conn.execute(
        "INSERT INTO remote_devices
            (id, name, token_hash, created_at, last_seen, last_auth_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, ?4, ?4, NULL)",
        params![device_id, name, token_hash, now],
    )?;
    conn.execute(
        "UPDATE remote_pairing SET consumed_at = ?1 WHERE code = ?2",
        params![now, code],
    )?;

    Ok(RedeemedDevice {
        device: DeviceRow {
            id: device_id.clone(),
            name,
            created_at: now,
            last_seen: now,
            last_auth_at: now,
            revoked_at: None,
        },
        cookie_value: format!("{}.{}", device_id, secret),
    })
}

// ─────────────────────────────────────────────────────────────
// Device CRUD + auth
// ─────────────────────────────────────────────────────────────

pub fn list_devices(conn: &Connection) -> Result<Vec<DeviceRow>, AuthError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, created_at, last_seen, last_auth_at, revoked_at
         FROM remote_devices
         ORDER BY revoked_at IS NOT NULL, last_seen DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DeviceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                last_seen: row.get(3)?,
                last_auth_at: row.get(4)?,
                revoked_at: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn revoke_device(conn: &Connection, device_id: &str) -> Result<(), AuthError> {
    conn.execute(
        "UPDATE remote_devices SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
        params![now_secs(), device_id],
    )?;
    Ok(())
}

pub fn delete_device(conn: &Connection, device_id: &str) -> Result<(), AuthError> {
    conn.execute("DELETE FROM remote_devices WHERE id = ?1", params![device_id])?;
    Ok(())
}

/// Verify a cookie value of the form `<device_id>.<secret>`. Returns the
/// authenticated device (id + last successful password check timestamp) on
/// success. The caller is responsible for any inactivity check.
pub fn verify_cookie(conn: &Connection, cookie: &str) -> Option<AuthenticatedDevice> {
    let (id, secret) = cookie.split_once('.')?;
    let row: Option<(String, Option<i64>, i64, i64)> = conn
        .query_row(
            "SELECT token_hash, revoked_at, last_auth_at, last_seen
             FROM remote_devices WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (token_hash, revoked_at, last_auth_at, last_seen) = row?;
    if revoked_at.is_some() {
        return None;
    }
    if !constant_time_eq(&sha256_hex(secret), &token_hash) {
        return None;
    }
    Some(AuthenticatedDevice {
        id: id.to_string(),
        last_auth_at,
        last_seen,
    })
}

/// Bump `last_seen` to now. Called from the auth path, rate-limited by
/// [`TOUCH_INTERVAL_SECS`].
pub fn touch_device(conn: &Connection, device_id: &str) -> Result<(), AuthError> {
    conn.execute(
        "UPDATE remote_devices SET last_seen = ?1 WHERE id = ?2",
        params![now_secs(), device_id],
    )?;
    Ok(())
}

/// Record a successful password re-authentication for a device.
pub fn mark_authenticated(conn: &Connection, device_id: &str) -> Result<(), AuthError> {
    let now = now_secs();
    conn.execute(
        "UPDATE remote_devices SET last_auth_at = ?1, last_seen = ?1 WHERE id = ?2",
        params![now, device_id],
    )?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Cookie naming
// ─────────────────────────────────────────────────────────────

/// The device-cookie name this gallery issues: `lv_device_<suffix>`.
///
/// Cookies are scoped by host and **not** by port, so two galleries served
/// from one machine on different ports share a jar entry — pairing a browser
/// with the second silently un-paired it from the first. The suffix is a
/// per-gallery random id, minted on first use and stored in `gallery_meta` so
/// it travels with the gallery rather than with the server that happens to be
/// serving it.
///
/// On a DB error this falls back to the bare legacy name. That is the right
/// failure: the same error will make `verify_cookie` fail a moment later, and
/// answering under a name no client holds would turn a transient DB problem
/// into a forced re-pair.
pub fn cookie_name(conn: &Connection) -> String {
    match cookie_suffix(conn) {
        Ok(suffix) => format!("{DEVICE_COOKIE_BASE}_{suffix}"),
        Err(e) => {
            log::warn!("could not read this gallery's cookie suffix: {e}");
            DEVICE_COOKIE_BASE.to_string()
        }
    }
}

fn cookie_suffix(conn: &Connection) -> Result<String, AuthError> {
    if let Some(existing) = meta_get(conn, META_COOKIE_SUFFIX)? {
        return Ok(existing);
    }
    let fresh = random_hex(4);
    conn.execute(
        "INSERT OR IGNORE INTO gallery_meta (key, value) VALUES (?1, ?2)",
        params![META_COOKIE_SUFFIX, fresh],
    )?;
    // Re-read instead of returning `fresh`: the desktop app and a headless
    // server can hold the same gallery open, so another process may have won
    // the insert, and both must agree on the name or each will keep issuing a
    // cookie the other ignores.
    Ok(meta_get(conn, META_COOKIE_SUFFIX)?.unwrap_or(fresh))
}

// ─────────────────────────────────────────────────────────────
// Gallery password + inactivity settings
// ─────────────────────────────────────────────────────────────

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, AuthError> {
    Ok(conn
        .query_row(
            "SELECT value FROM gallery_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn get_password_hash(conn: &Connection) -> Result<Option<String>, AuthError> {
    meta_get(conn, META_PASSWORD_HASH)
}

pub fn set_password(conn: &Connection, password: &str) -> Result<(), AuthError> {
    let hash = argon2_hash(password)?;
    conn.execute(
        "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
        params![META_PASSWORD_HASH, hash],
    )?;
    Ok(())
}

pub fn clear_password(conn: &Connection) -> Result<(), AuthError> {
    conn.execute(
        "DELETE FROM gallery_meta WHERE key = ?1",
        params![META_PASSWORD_HASH],
    )?;
    Ok(())
}

pub fn verify_password(conn: &Connection, password: &str) -> Result<bool, AuthError> {
    match get_password_hash(conn)? {
        Some(hash) => Ok(argon2_verify(password, &hash)),
        None => Ok(false),
    }
}

pub fn get_inactivity_secs(conn: &Connection) -> Result<i64, AuthError> {
    Ok(meta_get(conn, META_INACTIVITY_SECS)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_INACTIVITY_SECS))
}

pub fn set_inactivity_secs(conn: &Connection, secs: i64) -> Result<(), AuthError> {
    conn.execute(
        "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
        params![META_INACTIVITY_SECS, secs.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, crate::cache::db::CacheDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::cache::db::CacheDb::open(dir.path()).expect("open cache db");
        (dir, db)
    }

    #[test]
    fn pair_and_redeem_yields_working_cookie() {
        let (_dir, db) = open_db();
        let conn = db.conn();

        let code = create_pairing(conn, PairingKind::Qr).unwrap();
        let redeemed = redeem_pairing(conn, &code, "Test Phone").unwrap();

        let device = verify_cookie(conn, &redeemed.cookie_value).expect("cookie verifies");
        assert_eq!(device.id, redeemed.device.id);
    }

    #[test]
    fn pairing_code_is_single_use() {
        let (_dir, db) = open_db();
        let conn = db.conn();
        let code = create_pairing(conn, PairingKind::Pin).unwrap();
        let _ = redeem_pairing(conn, &code, "A").unwrap();
        match redeem_pairing(conn, &code, "B") {
            Err(AuthError::PairingConsumed) => {}
            other => panic!("expected PairingConsumed, got {:?}", other.map(|_| "ok")),
        }
    }

    #[test]
    fn tampered_cookie_is_rejected() {
        let (_dir, db) = open_db();
        let conn = db.conn();
        let code = create_pairing(conn, PairingKind::Qr).unwrap();
        let redeemed = redeem_pairing(conn, &code, "x").unwrap();
        // Flip one char in the secret.
        let mut bad = redeemed.cookie_value.clone();
        let last = bad.pop().unwrap();
        let flipped = if last == '0' { '1' } else { '0' };
        bad.push(flipped);
        assert!(verify_cookie(conn, &bad).is_none());
    }

    #[test]
    fn revoked_device_does_not_verify() {
        let (_dir, db) = open_db();
        let conn = db.conn();
        let code = create_pairing(conn, PairingKind::Qr).unwrap();
        let redeemed = redeem_pairing(conn, &code, "x").unwrap();
        revoke_device(conn, &redeemed.device.id).unwrap();
        assert!(verify_cookie(conn, &redeemed.cookie_value).is_none());
    }

    #[test]
    fn password_round_trip() {
        let (_dir, db) = open_db();
        let conn = db.conn();
        assert!(get_password_hash(conn).unwrap().is_none());
        set_password(conn, "hunter2").unwrap();
        assert!(verify_password(conn, "hunter2").unwrap());
        assert!(!verify_password(conn, "wrong").unwrap());
        clear_password(conn).unwrap();
        assert!(get_password_hash(conn).unwrap().is_none());
    }

    #[test]
    fn inactivity_defaults_then_overrides() {
        let (_dir, db) = open_db();
        let conn = db.conn();
        assert_eq!(get_inactivity_secs(conn).unwrap(), DEFAULT_INACTIVITY_SECS);
        set_inactivity_secs(conn, 3600).unwrap();
        assert_eq!(get_inactivity_secs(conn).unwrap(), 3600);
    }
}

