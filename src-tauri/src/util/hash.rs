use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Compute SHA-256 hash of a file, returned as "sha256:<hex>".
pub fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536]; // 64KB chunks

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    Ok(format!("sha256:{:x}", result))
}
