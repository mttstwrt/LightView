//! HTTP client for talking to the LightView server: TOFU certificate pinning,
//! the `/api/invoke` command bridge, and authenticated media downloads.
//!
//! The server's TLS cert is self-signed (see `http_server/tls.rs`), so WebPKI
//! validation is useless here. Instead the worker pins the certificate's
//! SHA-256 at pairing time and refuses anything else afterwards — signature
//! verification still runs, so a matching cert without the private key won't
//! handshake.

use std::path::Path;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use sha2::Digest;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Idle-read timeout: how long a request may go without receiving *any* bytes
/// before it errors. Resets on every successful read, so a large but steadily
/// progressing `/media` download is never cut off — only a genuinely stalled
/// connection is. Without this a single wedged download parks the downloader
/// task forever, and since the job loop keeps heartbeating, the server's stall
/// reaper never fires and the job runs forever with no error. Generous enough
/// that a busy server thinking about an `apply_plugin_tags` batch doesn't trip
/// it.
const READ_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Accepts only the certificate whose SHA-256 matches the pinned digest.
#[derive(Debug)]
struct PinnedVerifier {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

/// Accepts any certificate and records its digest — used once during `pair`
/// so the fingerprint can be shown to the user and pinned.
#[derive(Debug)]
struct CaptureVerifier {
    seen: std::sync::Mutex<Option<[u8; 32]>>,
    provider: Arc<CryptoProvider>,
}

fn digest_of(cert: &CertificateDer<'_>) -> [u8; 32] {
    sha2::Sha256::digest(cert.as_ref()).into()
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if digest_of(end_entity) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ServerCertVerifier for CaptureVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(digest_of(end_entity));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_with_verifier(
    verifier: Arc<dyn ServerCertVerifier>,
) -> Result<reqwest::Client, String> {
    let tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(READ_STALL_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

pub fn fingerprint_hex(pin: &[u8; 32]) -> String {
    pin.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_fingerprint(hex: &str) -> Result<[u8; 32], String> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return Err("cert_sha256 must be 64 hex chars".to_string());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "cert_sha256 contains non-hex characters".to_string())?;
    }
    Ok(out)
}

/// Connect once (`GET /healthz`, accepting any cert) and return the server
/// certificate's SHA-256 for the user to confirm and pin.
pub async fn capture_fingerprint(base: &str) -> Result<[u8; 32], String> {
    let verifier = Arc::new(CaptureVerifier {
        seen: std::sync::Mutex::new(None),
        provider: provider(),
    });
    let client = client_with_verifier(verifier.clone())?;
    client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .map_err(|e| format!("cannot reach {base}: {e}"))?;
    let seen = *verifier.seen.lock().unwrap();
    seen.ok_or_else(|| "no TLS certificate observed — is the server HTTPS?".to_string())
}

pub struct InvokeError {
    /// HTTP status; 0 for transport-level failures.
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for InvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(f, "HTTP {}: {}", self.status, self.message)
        }
    }
}

/// Authenticated, cert-pinned client bound to one server. Cheap to clone
/// (`reqwest::Client` is an `Arc` internally).
#[derive(Clone)]
pub struct ServerClient {
    http: reqwest::Client,
    base: String,
    cookie: String,
}

impl ServerClient {
    pub fn new(base: &str, cookie: &str, cert_sha256: &str) -> Result<Self, String> {
        let pin = parse_fingerprint(cert_sha256)?;
        let verifier = Arc::new(PinnedVerifier {
            pin,
            provider: provider(),
        });
        Ok(Self {
            http: client_with_verifier(verifier)?,
            base: base.trim_end_matches('/').to_string(),
            cookie: cookie.to_string(),
        })
    }

    /// `POST /api/invoke` with `{ command, args }`, deserializing the response.
    pub async fn invoke<T: DeserializeOwned>(
        &self,
        command: &str,
        args: serde_json::Value,
    ) -> Result<T, InvokeError> {
        let resp = self
            .http
            .post(format!("{}/api/invoke", self.base))
            .header("Cookie", &self.cookie)
            .timeout(std::time::Duration::from_secs(60))
            .json(&serde_json::json!({ "command": command, "args": args }))
            .send()
            .await
            .map_err(|e| InvokeError {
                status: 0,
                message: check_pin_error(e),
            })?;

        let status = resp.status().as_u16();
        if status != 200 {
            let message = resp.text().await.unwrap_or_default();
            return Err(InvokeError { status, message });
        }
        resp.json::<T>().await.map_err(|e| InvokeError {
            status: 0,
            message: format!("bad response for {command}: {e}"),
        })
    }

    /// Download one media file to `dest_dir`, named `<seq>.<ext>` with the
    /// extension taken from the response Content-Type (fit downloads are
    /// re-encoded server-side, so the original extension would lie). Returns
    /// the written path.
    pub async fn download_media(
        &self,
        server_path: &str,
        fit_edge: u32,
        dest_dir: &Path,
        seq: usize,
    ) -> Result<std::path::PathBuf, String> {
        let rel = server_path.strip_prefix('/').unwrap_or(server_path);
        let encoded = encode_media_path(rel);
        let query = if fit_edge > 0 {
            format!("?fit={fit_edge}")
        } else {
            String::new()
        };
        let resp = self
            .http
            .get(format!("{}/media/{}{}", self.base, encoded, query))
            .header("Cookie", &self.cookie)
            .send()
            .await
            .map_err(check_pin_error)?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }

        let ext = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .and_then(ext_for_content_type)
            .or_else(|| {
                Path::new(server_path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
            })
            .unwrap_or_else(|| "bin".to_string());

        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let dest = dest_dir.join(format!("{seq:06}.{ext}"));
        tokio::fs::write(&dest, &bytes)
            .await
            .map_err(|e| e.to_string())?;
        Ok(dest)
    }
}

/// Turn reqwest's opaque TLS error into an actionable message when the pinned
/// cert no longer matches (the server regenerates it when the LAN IP changes).
fn check_pin_error(e: reqwest::Error) -> String {
    let text = e.to_string();
    if text.contains("invalid peer certificate") || text.contains("ApplicationVerification") {
        format!(
            "{text}\nThe server's TLS certificate no longer matches the pinned fingerprint \
             (it regenerates when the server's LAN IP changes). If you trust the server, \
             re-run: lightview-worker pair --server <url> --trust-new"
        )
    } else {
        text
    }
}

fn ext_for_content_type(ct: &str) -> Option<String> {
    let ext = match ct.split(';').next().unwrap_or("").trim() {
        "image/webp" => "webp",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/avif" => "avif",
        _ => return None,
    };
    Some(ext.to_string())
}

/// Percent-encode a gallery-relative path per segment, mirroring the web
/// client's `encodeMediaPath` (JS `encodeURIComponent` per `/`-segment).
fn encode_media_path(rel: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    // encodeURIComponent leaves these unescaped.
    const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    rel.split('/')
        .map(|seg| utf8_percent_encode(seg, COMPONENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Redeem a pairing PIN for the server's device cookie, over a pinned
/// connection.
pub async fn redeem_pin(
    base: &str,
    cert_sha256: &str,
    pin_code: &str,
    device_name: &str,
) -> Result<String, String> {
    let fp = parse_fingerprint(cert_sha256)?;
    let verifier = Arc::new(PinnedVerifier {
        pin: fp,
        provider: provider(),
    });
    let client = client_with_verifier(verifier)?;
    let resp = client
        .post(format!("{base}/pair/redeem"))
        .json(&serde_json::json!({ "code": pin_code, "device_name": device_name }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("pairing failed (HTTP {status}): {body}"));
    }
    resp.headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.split(';').next())
        // The cookie name carries a per-gallery suffix (`lv_device_<id>`) so
        // two galleries on one host don't share a jar entry; match the base
        // name, and keep accepting the bare legacy name from an older server.
        .filter(|pair| pair.starts_with("lv_device"))
        .map(|pair| pair.to_string())
        .ok_or_else(|| "server response had no lv_device cookie".to_string())
}
