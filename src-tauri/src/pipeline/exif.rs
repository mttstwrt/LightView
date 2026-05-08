use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use exif::{In, Tag, Value};

use crate::companion::schema::Location;

/// Extract a GPS location from a still-image file's EXIF metadata.
/// Returns `None` if the file has no EXIF block, no GPS fields, or
/// the values fail to parse. Never errors — callers treat this as best-effort.
///
/// Container support comes from `kamadak-exif`'s `read_from_container`,
/// which handles JPEG, TIFF, HEIF/HEIC, PNG, and WebP. Unknown containers
/// silently return `None`.
pub fn extract_location(path: &Path) -> Option<Location> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new()
        .read_from_container(&mut reader)
        .ok()?;

    let lat_field = exif.get_field(Tag::GPSLatitude, In::PRIMARY)?;
    let lat_ref = exif.get_field(Tag::GPSLatitudeRef, In::PRIMARY)?;
    let lon_field = exif.get_field(Tag::GPSLongitude, In::PRIMARY)?;
    let lon_ref = exif.get_field(Tag::GPSLongitudeRef, In::PRIMARY)?;

    let lat = dms_to_decimal(&lat_field.value)?;
    let lon = dms_to_decimal(&lon_field.value)?;

    let lat = apply_hemisphere(lat, &lat_ref.value, b'S');
    let lon = apply_hemisphere(lon, &lon_ref.value, b'W');

    if !is_plausible(lat, lon) {
        return None;
    }

    let alt = exif
        .get_field(Tag::GPSAltitude, In::PRIMARY)
        .and_then(|f| rational_at(&f.value, 0))
        .map(|metres| {
            // GPSAltitudeRef = 1 means below sea level.
            let below = exif
                .get_field(Tag::GPSAltitudeRef, In::PRIMARY)
                .and_then(|f| match &f.value {
                    Value::Byte(v) => v.first().copied(),
                    _ => None,
                })
                .map(|b| b == 1)
                .unwrap_or(false);
            if below { -metres } else { metres }
        });

    Some(Location { lat, lon, alt })
}

/// Convert an EXIF rational triplet `[deg, min, sec]` to decimal degrees.
fn dms_to_decimal(value: &Value) -> Option<f64> {
    let d = rational_at(value, 0)?;
    let m = rational_at(value, 1).unwrap_or(0.0);
    let s = rational_at(value, 2).unwrap_or(0.0);
    Some(d + m / 60.0 + s / 3600.0)
}

fn rational_at(value: &Value, idx: usize) -> Option<f64> {
    match value {
        Value::Rational(v) => v.get(idx).map(|r| r.to_f64()),
        Value::SRational(v) => v.get(idx).map(|r| r.to_f64()),
        _ => None,
    }
}

/// Negate the magnitude when the hemisphere ref byte matches `negative_ref`
/// (`b'S'` for latitude, `b'W'` for longitude). EXIF stores the ref as a
/// 2-byte ASCII string ("N\0", "S\0", etc.).
fn apply_hemisphere(magnitude: f64, ref_value: &Value, negative_ref: u8) -> f64 {
    let first = match ref_value {
        Value::Ascii(parts) => parts.first().and_then(|p| p.first()).copied(),
        _ => None,
    };
    if first == Some(negative_ref) || first == Some(negative_ref.to_ascii_lowercase()) {
        -magnitude
    } else {
        magnitude
    }
}

fn is_plausible(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
        // Many cameras write 0,0 when GPS lock failed. Treat as missing.
        && !(lat.abs() < 1e-9 && lon.abs() < 1e-9)
}
