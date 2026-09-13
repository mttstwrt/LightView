//! `devices.db` — every pairing this **account** holds.
//!
//! In the state directory, not the gallery, because a pairing is a property of
//! the account serving rather than of any one folder. The consequence, stated
//! because it is a real widening: a phone paired to this machine is paired to
//! every gallery this machine serves, now or later. That is consistent with all
//! paired devices being equally trusted, and it is what removes the
//! per-gallery cookie-name mint that existed only because cookies are scoped by
//! host and not by port.
//!
//! **The secret is hashed with SHA-256, deliberately not argon2.** It is 32
//! random bytes, so a slow hash buys nothing against that search space — and
//! verification runs on *every thumbnail request*. The gallery password is the
//! opposite case and uses argon2id; see [`super::auth`].
//!
//! **The PIN fails closed.** "Six digits are fine because of the 10-minute TTL
//! and single-use redemption" is wrong: single use bounds a *successful* guess,
//! not the number of attempts. A million codes over a 600-second window is
//! about 1,700 guesses a second to exhaust, trivially parallel on a LAN,
//! against a server whose ordinary job is hundreds of requests per scroll — and
//! the prize is now a permanent credential for every gallery this machine
//! serves. So there is an `attempts` column, and **every outstanding pairing is
//! deleted at ten failures**. The human runs `lightview pair` again.

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::cache::pool::{apply_read_pragmas, ReadPool};

/// The cookie a paired device holds: `lv_device=<id>.<secret>`.
///
/// A fixed name. Two `--serve` processes on one account share pairings by
/// design, and two *accounts* on one host is not a deployment that exists.
pub const DEVICE_COOKIE: &str = "lv_device";

/// How long a pairing code is redeemable.
const PAIRING_TTL_SECS: i64 = 600;

/// Failed redemptions before every outstanding code is destroyed.
const MAX_PAIRING_ATTEMPTS: i64 = 10;

/// Don't rewrite `last_seen` more often than this. Auth is on the hot path and
/// must not take the writer on every thumbnail.
const LAST_SEEN_INTERVAL_SECS: i64 = 300;

/// Two readers, because authentication runs on every thumbnail request and must
/// never queue behind a pairing write.
const READ_POOL_MAX: usize = 2;
const CACHE_KB: i64 = 2_000;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no such pairing code")]
    NoSuchCode,
    #[error("this pairing code has expired")]
    Expired,
}

/// What a redemption produces.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub device_id: String,
    /// The cookie value: `<id>.<secret>`. Shown once and never stored.
    pub cookie_value: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: Option<i64>,
}

/// A device that presented a valid cookie.
#[derive(Debug, Clone)]
pub struct AuthenticatedDevice {
    pub id: String,
    /// When it last cleared the password challenge, if one is configured.
    pub last_auth_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingKind {
    /// Six digits, typed by hand.
    Pin,
    /// 32 bytes of hex, in a QR code.
    Token,
}

/// The pairing store: one writer, a pool of two readers.
pub struct Devices {
    writer: tokio::sync::Mutex<Connection>,
    readers: ReadPool,
}

impl Devices {
    pub fn open(path: &std::path::Path) -> Result<Self, AuthError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        apply_read_pragmas(&conn, CACHE_KB)?;
        conn.execute_batch(SCHEMA)?;

        let readers = ReadPool::open(path, READ_POOL_MAX, CACHE_KB)?;
        Ok(Self {
            writer: tokio::sync::Mutex::new(conn),
            readers,
        })
    }

    pub async fn writer(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.writer.lock().await
    }

    pub fn writer_blocking(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.writer.blocking_lock()
    }

    pub async fn read(&self) -> crate::cache::pool::PooledConn<'_> {
        self.readers.get().await
    }
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS devices (
        id           TEXT PRIMARY KEY,
        secret_hash  TEXT NOT NULL,
        name         TEXT NOT NULL,
        created_at   INTEGER NOT NULL,
        last_seen    INTEGER,
        last_auth_at INTEGER
    );

    CREATE TABLE IF NOT EXISTS pairings (
        code       TEXT PRIMARY KEY,
        kind       TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        -- Incremented on every failed redemption while this code is
        -- outstanding. One column and one UPDATE is the whole rate limit: no
        -- configuration, no timing state, no per-client bookkeeping.
        attempts   INTEGER NOT NULL DEFAULT 0
    );
";

/// Mint a pairing code. Returns the code to show the human.
pub fn create_pairing(conn: &Connection, kind: PairingKind) -> Result<String, AuthError> {
    purge_expired(conn)?;
    let code = match kind {
        PairingKind::Pin => format!("{:06}", random_u32() % 1_000_000),
        PairingKind::Token => hex(&random_bytes::<32>()),
    };
    let now = now();
    conn.execute(
        "INSERT OR REPLACE INTO pairings (code, kind, created_at, expires_at, attempts)
         VALUES (?1, ?2, ?3, ?4, 0)",
        rusqlite::params![
            code,
            match kind {
                PairingKind::Pin => "pin",
                PairingKind::Token => "token",
            },
            now,
            now + PAIRING_TTL_SECS,
        ],
    )?;
    Ok(code)
}

/// Redeem a code for a device cookie.
///
/// On failure, increments the attempt counter on **every** outstanding pairing
/// and destroys them all once any of them reaches the limit. Counting per row
/// would let an attacker burn attempts on a code that does not exist; counting
/// across the outstanding set is what makes the limit mean "ten guesses at this
/// pairing session".
pub fn redeem(
    conn: &Connection,
    code: &str,
    device_name: &str,
) -> Result<Pairing, AuthError> {
    purge_expired(conn)?;

    let row: Option<i64> = conn
        .query_row(
            "SELECT expires_at FROM pairings WHERE code = ?1",
            [code],
            |r| r.get(0),
        )
        .ok();

    let Some(expires_at) = row else {
        register_failure(conn)?;
        return Err(AuthError::NoSuchCode);
    };
    if expires_at < now() {
        conn.execute("DELETE FROM pairings WHERE code = ?1", [code])?;
        return Err(AuthError::Expired);
    }

    // Single use: the code is consumed whether or not what follows succeeds.
    conn.execute("DELETE FROM pairings WHERE code = ?1", [code])?;

    let device_id = hex(&random_bytes::<16>());
    let secret = hex(&random_bytes::<32>());
    conn.execute(
        "INSERT INTO devices (id, secret_hash, name, created_at, last_seen, last_auth_at)
         VALUES (?1, ?2, ?3, ?4, ?4, ?4)",
        rusqlite::params![device_id, sha256_hex(&secret), device_name, now()],
    )?;

    Ok(Pairing {
        cookie_value: format!("{device_id}.{secret}"),
        device_id,
    })
}

/// Count one failed redemption, and destroy the pairing session at the limit.
fn register_failure(conn: &Connection) -> Result<(), AuthError> {
    conn.execute("UPDATE pairings SET attempts = attempts + 1", [])?;
    let worst: i64 = conn
        .query_row("SELECT COALESCE(MAX(attempts), 0) FROM pairings", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if worst >= MAX_PAIRING_ATTEMPTS {
        log::warn!(
            "pairing failed {worst} times; destroying every outstanding code. \
             Run `lightview pair` again."
        );
        conn.execute("DELETE FROM pairings", [])?;
    }
    Ok(())
}

/// Verify a `lv_device` cookie.
///
/// Returns `None` for every failure shape — an unknown device, a malformed
/// cookie, a wrong secret — so the caller cannot accidentally distinguish them
/// in a response.
pub fn verify_cookie(conn: &Connection, cookie: &str) -> Option<AuthenticatedDevice> {
    let (id, secret) = cookie.split_once('.')?;
    let (stored, last_auth_at): (String, Option<i64>) = conn
        .query_row(
            "SELECT secret_hash, last_auth_at FROM devices WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    if !constant_time_eq(stored.as_bytes(), sha256_hex(secret).as_bytes()) {
        return None;
    }
    Some(AuthenticatedDevice {
        id: id.to_string(),
        last_auth_at,
    })
}

/// Whether `last_seen` is stale enough to be worth a write.
///
/// Auth runs on every thumbnail request, so an unconditional touch would take
/// the writer hundreds of times a scroll.
pub fn needs_last_seen_touch(conn: &Connection, device_id: &str) -> bool {
    let last: Option<i64> = conn
        .query_row("SELECT last_seen FROM devices WHERE id = ?1", [device_id], |r| {
            r.get(0)
        })
        .ok()
        .flatten();
    match last {
        Some(t) => now() - t >= LAST_SEEN_INTERVAL_SECS,
        None => true,
    }
}

pub fn touch_last_seen(conn: &Connection, device_id: &str) -> Result<(), AuthError> {
    conn.execute(
        "UPDATE devices SET last_seen = ?2 WHERE id = ?1",
        rusqlite::params![device_id, now()],
    )?;
    Ok(())
}

/// Record that a device cleared the password challenge.
pub fn mark_authenticated(conn: &Connection, device_id: &str) -> Result<(), AuthError> {
    conn.execute(
        "UPDATE devices SET last_auth_at = ?2 WHERE id = ?1",
        rusqlite::params![device_id, now()],
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<DeviceRow>, AuthError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, created_at, last_seen FROM devices ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(DeviceRow {
            id: r.get(0)?,
            name: r.get(1)?,
            created_at: r.get(2)?,
            last_seen: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Revoke one pairing. Returns whether there was one to revoke.
pub fn revoke(conn: &Connection, device_id: &str) -> Result<bool, AuthError> {
    Ok(conn.execute("DELETE FROM devices WHERE id = ?1", [device_id])? > 0)
}

fn purge_expired(conn: &Connection) -> Result<(), AuthError> {
    conn.execute("DELETE FROM pairings WHERE expires_at < ?1", [now()])?;
    Ok(())
}

/// Length-checked, branch-free comparison. Both inputs here are fixed-length
/// hex, so the length check leaks nothing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use rand::RngCore;
    let mut buf = [0u8; N];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

fn random_u32() -> u32 {
    use rand::RngCore;
    rand::thread_rng().next_u32()
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

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    #[test]
    fn a_pin_redeems_once_and_only_once() {
        let conn = db();
        let code = create_pairing(&conn, PairingKind::Pin).unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));

        let pairing = redeem(&conn, &code, "phone").unwrap();
        assert!(pairing.cookie_value.contains('.'));
        assert!(matches!(
            redeem(&conn, &code, "phone"),
            Err(AuthError::NoSuchCode)
        ));
    }

    #[test]
    fn a_redeemed_cookie_authenticates_and_a_tampered_one_does_not() {
        let conn = db();
        let code = create_pairing(&conn, PairingKind::Token).unwrap();
        let pairing = redeem(&conn, &code, "phone").unwrap();

        let device = verify_cookie(&conn, &pairing.cookie_value).expect("valid cookie");
        assert_eq!(device.id, pairing.device_id);

        // Every failure shape is the same answer.
        let (id, secret) = pairing.cookie_value.split_once('.').unwrap();
        // Change the last character to something it definitely is not — "append
        // a zero" is wrong one time in sixteen for a hex secret.
        let last = secret.chars().next_back().unwrap();
        let flipped = format!(
            "{id}.{}{}",
            &secret[..secret.len() - 1],
            if last == '0' { '1' } else { '0' }
        );
        assert!(verify_cookie(&conn, &flipped).is_none());
        assert!(verify_cookie(&conn, &format!("{id}.")).is_none());
        assert!(verify_cookie(&conn, "nonsense").is_none());
        assert!(verify_cookie(&conn, &format!("unknown.{secret}")).is_none());
    }

    #[test]
    fn ten_failed_guesses_destroy_the_pairing_session() {
        // Single use bounds a *successful* guess, not the number of attempts.
        // A million codes over a ten-minute window is ~1,700 guesses a second
        // to exhaust, which a LAN attacker can do trivially.
        let conn = db();
        let real = create_pairing(&conn, PairingKind::Pin).unwrap();

        for _ in 0..MAX_PAIRING_ATTEMPTS {
            let guess = if real == "000000" { "000001" } else { "000000" };
            assert!(redeem(&conn, guess, "attacker").is_err());
        }

        let outstanding: i64 = conn
            .query_row("SELECT COUNT(*) FROM pairings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(outstanding, 0, "the pairing session survived the attack");
        assert!(matches!(
            redeem(&conn, &real, "phone"),
            Err(AuthError::NoSuchCode)
        ));
    }

    #[test]
    fn an_expired_code_is_refused_and_removed() {
        let conn = db();
        let code = create_pairing(&conn, PairingKind::Pin).unwrap();
        conn.execute("UPDATE pairings SET expires_at = 1", []).unwrap();
        // purge_expired runs first, so this reads as "no such code" — which is
        // the same answer an attacker gets for a wrong guess.
        assert!(redeem(&conn, &code, "phone").is_err());
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM pairings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn last_seen_is_not_written_on_every_request() {
        // Auth runs on every thumbnail; an unconditional touch would take the
        // writer hundreds of times per scroll.
        let conn = db();
        let code = create_pairing(&conn, PairingKind::Token).unwrap();
        let pairing = redeem(&conn, &code, "phone").unwrap();

        assert!(!needs_last_seen_touch(&conn, &pairing.device_id));
        conn.execute(
            "UPDATE devices SET last_seen = ?2 WHERE id = ?1",
            rusqlite::params![pairing.device_id, now() - LAST_SEEN_INTERVAL_SECS - 1],
        )
        .unwrap();
        assert!(needs_last_seen_touch(&conn, &pairing.device_id));
    }

    #[test]
    fn revoking_a_device_ends_its_session() {
        // The reason `lightview devices` exists: nothing else could do this,
        // since nothing is `Owner` under `--serve`.
        let conn = db();
        let code = create_pairing(&conn, PairingKind::Token).unwrap();
        let pairing = redeem(&conn, &code, "lost phone").unwrap();

        assert_eq!(list(&conn).unwrap().len(), 1);
        assert!(revoke(&conn, &pairing.device_id).unwrap());
        assert!(verify_cookie(&conn, &pairing.cookie_value).is_none());
        assert!(list(&conn).unwrap().is_empty());
        assert!(!revoke(&conn, &pairing.device_id).unwrap());
    }

    #[test]
    fn the_secret_is_never_stored_in_the_clear() {
        let conn = db();
        let code = create_pairing(&conn, PairingKind::Token).unwrap();
        let pairing = redeem(&conn, &code, "phone").unwrap();
        let (_, secret) = pairing.cookie_value.split_once('.').unwrap();

        let stored: String = conn
            .query_row("SELECT secret_hash FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, secret);
        assert_eq!(stored, sha256_hex(secret));
    }

    #[test]
    fn constant_time_eq_is_length_checked() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
