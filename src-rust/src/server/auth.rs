//! Trust, and the two ways a client proves it has any.
//!
//! # Trust is a property of the bind
//!
//! | Level | Reachable from |
//! |---|---|
//! | `Device` | any paired client |
//! | `Owner`  | **a loopback bind only** |
//!
//! **`Owner` is granted by a loopback bind and by nothing else. There is no
//! flag that widens it, and no failure path widens it either.** That is the
//! single security rule of the system and it has to survive every later change.
//!
//! It is a property of the accepting *listener*, never of the peer address. A
//! `0.0.0.0` bind includes `127.0.0.1`, so a rule written as "the peer is
//! loopback" hands `Owner` to any local process on a served host — including
//! anything a browser on that host can be made to issue.
//!
//! Here that is structural rather than a check: `lightview <dir>` and
//! `lightview --serve <dir>` are mutually exclusive on one gallery (section
//! 3.3's one-process rule), so a process has exactly **one** listener and
//! therefore one trust ceiling, fixed at bind. [`Trust`] lives in application
//! state, nothing writes it after startup, and there is no request-derived path
//! that could raise it.
//!
//! # The loopback session
//!
//! **The bind is a random address in `127.0.0.0/8`**, not `127.0.0.1`, and this
//! is the one thing here that cannot be fixed later:
//!
//! > Cookies are not port-scoped, and `SameSite` is site-scoped. A cookie set by
//! > `127.0.0.1:54321` is a host-only cookie for `127.0.0.1` and is sent to
//! > **every other port on that host** — a dev server, a notebook, a downloaded
//! > repository's `npm run dev` — which can replay it from a non-browser client
//! > where no `Origin` is expected. `HttpOnly` does not help; the *server* reads
//! > it. And `SameSite=Strict` blocks cross-*site* requests, but
//! > `127.0.0.1:3000` and `127.0.0.1:54321` are the same site.
//!
//! The whole `/8` routes to `lo` on Linux and the entire range is a
//! "potentially trustworthy origin", so the secure context the async Clipboard
//! API needs is preserved. A random address in it makes the session cookie's
//! host belong to this process alone.
//!
//! The token is 32 random bytes, **rotated on every redemption**, held in memory
//! and mirrored into `<cache dir>/instance.json` at mode 0600. There is no TTL:
//! single use plus rotation bounds exposure, and the file is readable only by
//! the account that already owns the photos.

use std::path::Path;
use std::sync::Mutex;

use crate::server::devices::AuthError;

/// The session cookie on a loopback bind. Needs no per-install suffix: its host
/// is a process-unique address.
pub const SESSION_COOKIE: &str = "lv_session";

/// The minimum trust a command requires, and the ceiling a listener grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Any paired client. Browse, tag, rate, upload, move to trash, restore.
    Device,
    /// A loopback bind only. Copy, move, clipboard, open-with, open a gallery,
    /// list a directory, install a plugin, purge trash, merge duplicates.
    Owner,
}

impl Trust {
    /// Whether a listener at this level may run a command needing `required`.
    pub fn allows(self, required: Trust) -> bool {
        self >= required
    }
}

/// The launch token and the process-lifetime session it is exchanged for.
pub struct LaunchSession {
    token: Mutex<String>,
    /// Constant for the life of the process. A second browser tab shares it; a
    /// restart invalidates it, and a stale tab gets a 401.
    session: String,
}

impl Default for LaunchSession {
    fn default() -> Self {
        Self::new()
    }
}

impl LaunchSession {
    pub fn new() -> Self {
        Self {
            token: Mutex::new(random_hex::<32>()),
            session: random_hex::<32>(),
        }
    }

    /// The token to put in the launch URL right now.
    pub fn token(&self) -> String {
        self.token.lock().expect("launch token poisoned").clone()
    }

    /// Exchange a token for the session cookie, rotating the token.
    ///
    /// `None` means the token was wrong or already spent. **Single use means
    /// single use** — a second attempt with the same token is 401, and there is
    /// no "just trust loopback this once" path, because that shortcut is the
    /// whole vulnerability.
    pub fn redeem(&self, presented: &str) -> Option<String> {
        let mut token = self.token.lock().expect("launch token poisoned");
        if !constant_time_eq(token.as_bytes(), presented.as_bytes()) {
            return None;
        }
        *token = random_hex::<32>();
        Some(self.session.clone())
    }

    /// Whether a presented session cookie is this process's.
    pub fn verify(&self, cookie: &str) -> bool {
        constant_time_eq(self.session.as_bytes(), cookie.as_bytes())
    }
}

/// What a second `lightview <dir>` reads to find the live window.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Instance {
    pub pid: u32,
    /// The full launch URL, token included, rewritten on every rotation.
    pub url: String,
}

impl Instance {
    /// Write `instance.json` at mode 0600, beside the gallery lock.
    ///
    /// Mode matters: the file carries a live credential. It is an acceptable
    /// thing to have on disk — single-use, rotating, on a process-private
    /// address — only because nothing but the account that already owns the
    /// photos can read it.
    pub fn write(dir: &Path, url: &str) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("instance.json");
        let body = serde_json::to_vec_pretty(&Instance {
            pid: std::process::id(),
            url: url.to_string(),
        })?;
        crate::util::fs_atomic::write_durable(&path, &body)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Read it, for a second launch that found the lock held.
    pub fn read(dir: &Path) -> Option<Instance> {
        let body = std::fs::read(dir.join("instance.json")).ok()?;
        serde_json::from_slice(&body).ok()
    }

    /// Remove it, on the way out of a local session.
    ///
    /// **Before the listener stops, not after.** A second `lightview <dir>`
    /// that finds the lock still held reads this file and opens a browser at
    /// the URL in it; racing the exit, that tab would land on a port that has
    /// stopped answering. Deleting it first makes the race resolve the other
    /// way — the launcher finds nothing, and starts a session of its own.
    pub fn remove(dir: &Path) {
        let _ = std::fs::remove_file(dir.join("instance.json"));
    }
}

/// Pick the loopback address this process will own.
///
/// `127.<r>.<r>.<r>`, avoiding `127.0.0.1` itself so the session cookie's host
/// cannot collide with the address every other local service uses, and avoiding
/// a final octet of 0 or 255.
pub fn random_loopback() -> std::net::Ipv4Addr {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    loop {
        let addr = std::net::Ipv4Addr::new(
            127,
            rng.gen_range(0..=255),
            rng.gen_range(0..=255),
            rng.gen_range(1..255),
        );
        if addr != std::net::Ipv4Addr::LOCALHOST {
            return addr;
        }
    }
}

/// The `Origin` rule, stated precisely enough to implement.
///
/// **Reject when `Origin` is present and is not byte-equal to this server's
/// scheme + host + port. Allow `Origin` absent.** Two traps, both of which a
/// looser wording walks into: a *site*-level comparison, or accepting
/// `Sec-Fetch-Site: same-site`, both pass an attacker on another local port;
/// and *requiring* the header breaks every non-browser client, because they do
/// not send it. Browsers always send `Origin` on a cross-origin POST, so
/// absence is safe.
pub fn origin_allowed(expected_origin: &str, presented: Option<&str>) -> bool {
    match presented {
        None => true,
        Some(origin) => origin == expected_origin,
    }
}

/// The same check under `--serve`, where a `0.0.0.0` bind has no single origin
/// to name — which is why CORS was `Any` before.
///
/// `Sec-Fetch-Site` is a browser-populated header a page cannot forge. Absence
/// is allowed for the same reason as above: `curl` does not send it.
pub fn fetch_site_allowed(presented: Option<&str>) -> bool {
    match presented {
        None => true,
        Some(site) => site == "same-origin" || site == "none",
    }
}

/// Hash a passphrase for `server.toml`. argon2id with the crate's defaults.
///
/// The opposite case from a device secret: a passphrase has low entropy and is
/// verified rarely, so a slow hash is exactly right here and wrong there.
pub fn hash_password(passphrase: &str) -> Result<String, AuthError> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(passphrase.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Io(std::io::Error::other(e.to_string())))
}

/// Verify a passphrase against a stored PHC string.
pub fn verify_password(stored: &str, passphrase: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    argon2::Argon2::default()
        .verify_password(passphrase.as_bytes(), &parsed)
        .is_ok()
}

/// Whether a device must clear the password challenge again.
pub fn challenge_due(last_auth_at: Option<i64>, inactivity_secs: i64) -> bool {
    match last_auth_at {
        None => true,
        Some(t) => now() - t >= inactivity_secs,
    }
}

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

fn random_hex<const N: usize>() -> String {
    use rand::RngCore;
    use std::fmt::Write;
    let mut buf = [0u8; N];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
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

    #[test]
    fn owner_covers_device_and_device_does_not_cover_owner() {
        assert!(Trust::Owner.allows(Trust::Device));
        assert!(Trust::Owner.allows(Trust::Owner));
        assert!(Trust::Device.allows(Trust::Device));
        assert!(
            !Trust::Device.allows(Trust::Owner),
            "a served bind must never reach an Owner command"
        );
    }

    #[test]
    fn a_token_redeems_once_and_rotates() {
        let s = LaunchSession::new();
        let first = s.token();
        let session = s.redeem(&first).expect("first redemption");
        assert!(s.verify(&session));

        // Single use means single use. There is no failure path that widens
        // Owner "just this once".
        assert!(s.redeem(&first).is_none());
        assert_ne!(s.token(), first, "the token did not rotate");

        let second = s.token();
        assert_eq!(s.redeem(&second).as_deref(), Some(session.as_str()));
    }

    #[test]
    fn a_wrong_token_never_mints_a_session() {
        let s = LaunchSession::new();
        assert!(s.redeem("").is_none());
        assert!(s.redeem("0".repeat(64).as_str()).is_none());
        assert!(!s.verify(""));
        assert!(!s.verify("0".repeat(64).as_str()));
    }

    #[test]
    fn two_tabs_share_one_session_and_a_restart_invalidates_it() {
        let s = LaunchSession::new();
        let session = s.redeem(&s.token()).unwrap();
        assert!(s.verify(&session));

        // A restart is a new process: new token, new session, new address.
        let restarted = LaunchSession::new();
        assert!(
            !restarted.verify(&session),
            "a stale tab kept its session across a restart"
        );
    }

    #[test]
    fn the_loopback_address_is_never_the_one_everything_else_uses() {
        // The whole point: the session cookie's host must belong to this
        // process alone, because cookies are not port-scoped.
        for _ in 0..200 {
            let addr = random_loopback();
            assert_eq!(addr.octets()[0], 127);
            assert_ne!(addr, std::net::Ipv4Addr::LOCALHOST);
            assert_ne!(addr.octets()[3], 0);
            assert_ne!(addr.octets()[3], 255);
        }
    }

    #[test]
    fn origin_is_byte_equal_or_absent() {
        let me = "http://127.42.7.9:54321";
        assert!(origin_allowed(me, None), "curl sends no Origin");
        assert!(origin_allowed(me, Some(me)));
        // A *site*-level comparison would pass these, and each is an attacker
        // on another local port.
        assert!(!origin_allowed(me, Some("http://127.0.0.1:3000")));
        assert!(!origin_allowed(me, Some("http://127.42.7.9:3000")));
        assert!(!origin_allowed(me, Some("https://127.42.7.9:54321")));
        assert!(!origin_allowed(me, Some("http://evil.example")));
    }

    #[test]
    fn fetch_site_accepts_only_same_origin_or_nothing() {
        assert!(fetch_site_allowed(None));
        assert!(fetch_site_allowed(Some("same-origin")));
        assert!(fetch_site_allowed(Some("none")));
        // `same-site` is the trap: it passes an attacker on another port.
        assert!(!fetch_site_allowed(Some("same-site")));
        assert!(!fetch_site_allowed(Some("cross-site")));
    }

    #[test]
    fn a_password_round_trips_and_a_wrong_one_does_not() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "wrong"));
        assert!(!verify_password("not a phc string", "anything"));
    }

    #[test]
    fn the_challenge_is_due_for_a_device_that_has_never_cleared_it() {
        assert!(challenge_due(None, 3600));
        assert!(!challenge_due(Some(now()), 3600));
        assert!(challenge_due(Some(now() - 3601), 3600));
    }
}
