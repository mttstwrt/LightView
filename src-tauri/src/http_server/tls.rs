//! Self-signed TLS for the remote-access server.
//!
//! Browsers only expose the async Clipboard API (and other powerful features)
//! in secure contexts, so the LAN server must speak HTTPS. There is no CA that
//! will sign a cert for a private LAN IP, so we mint our own: an ECDSA P-256
//! self-signed certificate whose SANs cover localhost and the machine's
//! current LAN IP. Each browser shows a one-time interstitial the user clicks
//! through (or they install the cert to trust it permanently).
//!
//! The cert/key pair is persisted under `<exe_dir>/data/tls/` so that trust
//! decisions survive restarts. It is regenerated only when missing, close to
//! expiry, or when the machine's LAN IP is no longer covered by the SANs
//! (e.g. after moving to a different network).
//!
//! [`detect_lan_ip`] sees only the interface *this process* routes through,
//! which in a container is the bridge address (e.g. `172.20.0.2`) — never the
//! host LAN IP a phone actually dials. A cert minted there fails hostname
//! verification on every client, leaving them on a click-through exception that
//! iOS drops readily and a standalone PWA cannot even re-accept. Deployments
//! that publish a port must therefore name the reachable address explicitly,
//! via `--tls-san` or `LIGHTVIEW_TLS_SAN` (see [`sans_from_env`]).

use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::paths::data_dir;

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
const META_FILE: &str = "meta.json";

/// Validity period. Kept under Apple's 825-day cap so the cert can be
/// manually trusted on iOS/macOS if the user wants to silence the warning.
const VALIDITY_DAYS: i64 = 730;
/// Regenerate this long before expiry so a long-running install never serves
/// an expired cert.
const RENEW_MARGIN_DAYS: i64 = 30;

/// Bump when `generate()` starts emitting extensions a persisted cert must
/// have (e.g. the serverAuth EKU iOS requires): certs minted by older builds
/// then regenerate on the next start instead of being served until expiry.
/// 1 = KeyUsage(digitalSignature) + EKU(serverAuth).
const CERT_SCHEMA: u32 = 1;

/// Sidecar metadata so we can decide whether the persisted cert is still
/// usable without pulling in an x509 parser.
#[derive(Serialize, Deserialize)]
struct CertMeta {
    /// SANs the cert was generated with (string form of DNS names and IPs).
    sans: Vec<String>,
    /// Unix seconds of the cert's notAfter.
    not_after: i64,
    /// `CERT_SCHEMA` at generation time; absent in pre-schema meta files.
    #[serde(default)]
    schema: u32,
}

pub struct TlsMaterial {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// Best-effort guess of the machine's primary LAN IP: a connected UDP socket
/// picks the interface the OS would route external traffic through, without
/// sending any packets.
pub fn detect_lan_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

fn tls_dir() -> PathBuf {
    data_dir().join("tls")
}

/// Environment variable naming additional SANs, comma- or space-separated.
/// Exists so a containerized deployment can declare its reachable address
/// without a CLI flag (`environment:` in docker-compose).
pub const SAN_ENV_VAR: &str = "LIGHTVIEW_TLS_SAN";

/// Extra SANs declared via [`SAN_ENV_VAR`]. Unset or empty yields no entries.
pub fn sans_from_env() -> Vec<String> {
    std::env::var(SAN_ENV_VAR).map(|v| split_sans(&v)).unwrap_or_default()
}

/// Split a user-supplied SAN list on commas and whitespace, dropping blanks.
pub fn split_sans(raw: &str) -> Vec<String> {
    raw.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn san_strings(lan_ip: Option<IpAddr>, extra: &[String]) -> Vec<String> {
    let mut sans = vec![
        "localhost".to_string(),
        IpAddr::V4(Ipv4Addr::LOCALHOST).to_string(),
        IpAddr::V6(Ipv6Addr::LOCALHOST).to_string(),
    ];
    // The detected address goes in first so the ordering (and therefore the
    // regenerate-on-change comparison) stays stable as extras come and go.
    for s in lan_ip
        .map(|ip| ip.to_string())
        .into_iter()
        .chain(extra.iter().map(|s| normalize_san(s)))
    {
        if !sans.contains(&s) {
            sans.push(s);
        }
    }
    sans
}

/// Canonicalize an operator-supplied SAN so the same address written two ways
/// (`::1` vs `0:0:0:0:0:0:0:1`) doesn't churn the cert on every start. Hostnames
/// pass through lowercased; anything that parses as an IP is re-rendered by
/// `IpAddr`'s own formatting, which is what `san_strings` compares against.
fn normalize_san(raw: &str) -> String {
    match raw.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => raw.to_ascii_lowercase(),
    }
}

/// Return persisted cert material if it exists, covers `sans`, and is not
/// close to expiry.
fn load_existing(dir: &PathBuf, sans: &[String]) -> Option<TlsMaterial> {
    let meta: CertMeta = serde_json::from_slice(&fs::read(dir.join(META_FILE)).ok()?).ok()?;
    if meta.schema < CERT_SCHEMA {
        log::info!("TLS cert predates schema {CERT_SCHEMA}; regenerating");
        return None;
    }
    if !sans.iter().all(|s| meta.sans.contains(s)) {
        log::info!("TLS cert SANs no longer cover {:?}; regenerating", sans);
        return None;
    }
    let renew_at = meta.not_after - RENEW_MARGIN_DAYS * 86_400;
    if chrono::Utc::now().timestamp() >= renew_at {
        log::info!("TLS cert is near expiry; regenerating");
        return None;
    }
    Some(TlsMaterial {
        cert_pem: fs::read(dir.join(CERT_FILE)).ok()?,
        key_pem: fs::read(dir.join(KEY_FILE)).ok()?,
    })
}

fn generate(sans: &[String]) -> Result<(TlsMaterial, i64), String> {
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "LightView");
    for san in sans {
        let san_type = match san.parse::<IpAddr>() {
            Ok(ip) => rcgen::SanType::IpAddress(ip),
            Err(_) => rcgen::SanType::DnsName(
                san.clone()
                    .try_into()
                    .map_err(|e| format!("invalid SAN {san:?}: {e}"))?,
            ),
        };
        params.subject_alt_names.push(san_type);
    }

    // iOS/macOS won't trust a TLS server cert — even one the user manually
    // installs — unless it carries the serverAuth EKU and a matching KeyUsage
    // (Apple's "Requirements for trusted certificates in iOS 13+"). Without
    // them Safari rejects the cert and drops the `Secure` pairing cookie, so a
    // paired device bounces straight back to the pairing screen; Chromium is
    // laxer and accepts the click-through, which is why it works there.
    params
        .key_usages
        .push(rcgen::KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);

    let now = time::OffsetDateTime::now_utc();
    // Backdate slightly so clients with mild clock skew accept it immediately.
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(VALIDITY_DAYS);
    let not_after_unix = params.not_after.unix_timestamp();

    let key_pair = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key_pair).map_err(|e| e.to_string())?;

    Ok((
        TlsMaterial {
            cert_pem: cert.pem().into_bytes(),
            key_pem: key_pair.serialize_pem().into_bytes(),
        },
        not_after_unix,
    ))
}

fn persist(dir: &PathBuf, material: &TlsMaterial, sans: &[String], not_after: i64) {
    let write_all = || -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join(CERT_FILE), &material.cert_pem)?;
        fs::write(dir.join(KEY_FILE), &material.key_pem)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(KEY_FILE), fs::Permissions::from_mode(0o600))?;
        }
        let meta = CertMeta {
            sans: sans.to_vec(),
            not_after,
            schema: CERT_SCHEMA,
        };
        fs::write(dir.join(META_FILE), serde_json::to_vec_pretty(&meta)?)?;
        Ok(())
    };
    // Persistence failing is not fatal — the in-memory cert still serves this
    // session; the next start just regenerates (and browsers re-prompt).
    if let Err(e) = write_all() {
        log::warn!("Failed to persist TLS cert to {}: {e}", dir.display());
    }
}

/// Load the persisted self-signed certificate, or generate and persist a new
/// one when absent, near expiry, or no longer covering the current LAN IP.
///
/// `extra_sans` are additional names/addresses the cert must cover on top of
/// loopback and the auto-detected LAN IP — the escape hatch for deployments
/// where detection can't see the reachable address (containers, NAT, a DNS
/// name). Adding one regenerates the cert, so every paired browser will show
/// the interstitial once more.
pub fn load_or_generate(extra_sans: &[String]) -> Result<TlsMaterial, String> {
    // rustls 0.23 needs a process-level crypto provider before a ServerConfig
    // can be built. Install one explicitly so this doesn't depend on which
    // provider features other crates in the tree happen to enable.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let dir = tls_dir();
    let sans = san_strings(detect_lan_ip(), extra_sans);

    if let Some(existing) = load_existing(&dir, &sans) {
        return Ok(existing);
    }

    let (material, not_after) = generate(&sans)?;
    persist(&dir, &material, &sans, not_after);
    log::info!(
        "Generated self-signed TLS certificate (SANs: {}) at {}",
        sans.join(", "),
        dir.display()
    );
    Ok(material)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
    }

    #[test]
    fn splits_on_commas_and_whitespace() {
        assert_eq!(
            split_sans("192.168.1.2, gallery.lan\t10.0.0.5"),
            vec!["192.168.1.2", "gallery.lan", "10.0.0.5"]
        );
        assert!(split_sans("  ,, ").is_empty());
        assert!(split_sans("").is_empty());
    }

    /// The whole point of the flag: the address a client dials must land in the
    /// SANs even when detection reports something else (a container bridge IP).
    #[test]
    fn extra_sans_cover_an_undetectable_host_address() {
        let sans = san_strings(v4(172, 20, 0, 2), &["192.168.1.2".to_string()]);
        assert!(sans.contains(&"192.168.1.2".to_string()));
        assert!(sans.contains(&"172.20.0.2".to_string()));
        assert!(sans.contains(&"localhost".to_string()));
    }

    #[test]
    fn dns_names_are_accepted_alongside_ips() {
        let sans = san_strings(v4(10, 0, 0, 1), &["Gallery.LAN".to_string()]);
        assert!(sans.contains(&"gallery.lan".to_string()));
    }

    /// `load_existing` regenerates when a required SAN is absent, so an unstable
    /// spelling of the same address would re-mint the cert on every start and
    /// invalidate every browser's stored trust decision.
    #[test]
    fn equivalent_spellings_do_not_churn_the_cert() {
        let a = san_strings(None, &["0:0:0:0:0:0:0:1".to_string()]);
        let b = san_strings(None, &["::1".to_string()]);
        assert_eq!(a, b);
        assert_eq!(a.iter().filter(|s| *s == "::1").count(), 1);
    }

    #[test]
    fn duplicates_collapse() {
        let sans = san_strings(
            v4(192, 168, 1, 2),
            &["192.168.1.2".to_string(), "localhost".to_string()],
        );
        assert_eq!(sans.iter().filter(|s| *s == "192.168.1.2").count(), 1);
        assert_eq!(sans.iter().filter(|s| *s == "localhost").count(), 1);
    }

    /// A cert already covering everything required must not be re-minted just
    /// because it also carries names no longer asked for.
    #[test]
    fn superset_meta_still_satisfies_required_sans() {
        let required = san_strings(v4(192, 168, 1, 2), &[]);
        let stored = san_strings(v4(192, 168, 1, 2), &["gallery.lan".to_string()]);
        assert!(required.iter().all(|s| stored.contains(s)));
    }
}
