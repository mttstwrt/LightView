//! Coordinates → place names.
//!
//! Turns the `gps_lat`/`gps_lon` already cached in `media_meta` into the
//! country, region, and city names a person actually types into the filter bar
//! ("Japan", "Kyoto"). Nothing here reads image bytes: the coordinate has
//! already been extracted by [`crate::pipeline::exif`] and stored, and this
//! module is a pure function over two floats.
//!
//! **Why the names are written to companion files** (see
//! `commands::gallery::backfill_location_tags`) when the coordinate deliberately
//! is not: the coordinate lives in the file's own EXIF and any photo tool can
//! recover it, so mirroring it into a sidecar would be redundant. The *name* is
//! not in the file — recovering it needs the gazetteer below, at a particular
//! version — so it is the one part of this that would not survive the gallery
//! being read by anything other than LightView.
//!
//! **Matching is approximate, by construction.** The gazetteer is GeoNames
//! cities1000 (every populated place of 1,000 people or more) and the lookup is
//! nearest-neighbour, so a photo taken away from a town still resolves to
//! *some* town. Two ceilings keep that honest rather than confidently wrong:
//! past [`CITY_MAX_KM`] the nearest place is no longer where the photo was
//! taken and the city tag is dropped, while country and region stay valid much
//! further out; past [`REGION_MAX_KM`] — mid-ocean, deep desert, Antarctic
//! interior — even those are a guess and nothing is emitted.
//!
//! The gazetteer is built on first use and never freed. It costs roughly a
//! second to parse 144k rows into a k-d tree, so a gallery with no geotagged
//! media must never touch it — hence the [`OnceLock`] rather than
//! construction at startup.

use std::sync::OnceLock;

use reverse_geocoder::ReverseGeocoder;

mod countries;

/// Recorded as the `version` of the companion tag bucket, so a future dataset
/// swap can tell its own output apart from this one's.
pub const DATASET_VERSION: &str = "geonames-cities1000";

/// Mean Earth radius, IUGG. Only used to express the two ceilings below in
/// kilometres instead of the k-d tree's unit-sphere units.
const EARTH_RADIUS_KM: f64 = 6371.0;

/// Beyond this the nearest populated place is not where the photo was taken,
/// it is merely the closest inhabited place — a national park shot 40 km from
/// the nearest town would otherwise be tagged with that town.
const CITY_MAX_KM: f64 = 25.0;

/// Beyond this even the country is a guess: the match is across a border, out
/// at sea, or the sole gazetteer entry for an empty region. Emit nothing.
const REGION_MAX_KM: f64 = 100.0;

/// A resolved location, coarse to fine. Every field is optional because the
/// gazetteer's own coverage is uneven: `admin1` is blank on ~840 of its 144k
/// rows, and a country code it carries may not be one we can name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub country: Option<String>,
    /// State, province, or prefecture — GeoNames `admin1`, already a name
    /// rather than a code ("Kyoto", "Alabama").
    pub region: Option<String>,
    pub city: Option<String>,
}

impl Place {
    /// The tag values to write, coarse to fine and deduplicated.
    ///
    /// Deduplication is not cosmetic: city-states and same-named capitals
    /// ("Singapore", "Kyoto" city inside "Kyoto" prefecture) would otherwise
    /// write the identical string two or three times into one bucket.
    pub fn tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = Vec::with_capacity(3);
        for value in [&self.country, &self.region, &self.city].into_iter().flatten() {
            if !tags.iter().any(|existing| existing == value) {
                tags.push(value.clone());
            }
        }
        tags
    }
}

fn geocoder() -> &'static ReverseGeocoder {
    static GEOCODER: OnceLock<ReverseGeocoder> = OnceLock::new();
    GEOCODER.get_or_init(ReverseGeocoder::new)
}

/// Convert the k-d tree's squared-Euclidean distance between two points on a
/// unit sphere into kilometres on the Earth's surface.
///
/// The tree indexes 3D cartesian coordinates, so its distance is a squared
/// *chord* through the sphere, not an arc along it. Chord `c` subtends a
/// central angle of `2·asin(c/2)`; that angle times the radius is the great
/// circle distance. Treating the raw value as a distance would understate long
/// separations badly, which is exactly where the ceilings below have to bite.
fn unit_sphere_sq_to_km(squared_chord: f64) -> f64 {
    let chord = squared_chord.max(0.0).sqrt();
    2.0 * (chord / 2.0).clamp(-1.0, 1.0).asin() * EARTH_RADIUS_KM
}

/// Resolve decimal-degree WGS-84 coordinates to place names.
///
/// `None` when the nearest gazetteer entry is too far away to mean anything
/// (see [`REGION_MAX_KM`]) or when it yields no nameable field at all.
pub fn lookup(lat: f64, lon: f64) -> Option<Place> {
    let result = geocoder().search((lat, lon));
    let distance_km = unit_sphere_sq_to_km(result.distance);
    if distance_km > REGION_MAX_KM {
        return None;
    }

    let record = result.record;
    let place = Place {
        country: countries::name_for(&record.cc).map(str::to_string),
        region: non_empty(&record.admin1),
        city: if distance_km <= CITY_MAX_KM {
            non_empty(&record.name)
        } else {
            None
        },
    };

    if place.country.is_none() && place.region.is_none() && place.city.is_none() {
        return None;
    }
    Some(place)
}

/// GeoNames leaves unknown fields as empty strings rather than omitting them.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_japanese_city_to_prefecture_and_country() {
        // Kyoto Station.
        let place = lookup(34.9858, 135.7588).expect("Kyoto should resolve");
        assert_eq!(place.country.as_deref(), Some("Japan"));
        assert_eq!(place.region.as_deref(), Some("Kyoto"));
        // The city and the prefecture share a name; the tag list must not.
        assert_eq!(place.tags(), vec!["Japan".to_string(), "Kyoto".to_string()]);
    }

    #[test]
    fn resolves_a_us_city_to_state_and_country() {
        // Manhattan.
        let place = lookup(40.7831, -73.9712).expect("Manhattan should resolve");
        assert_eq!(place.country.as_deref(), Some("United States"));
        assert_eq!(place.region.as_deref(), Some("New York"));
        assert_eq!(place.city.as_deref(), Some("Manhattan"));
    }

    #[test]
    fn open_ocean_resolves_to_nothing() {
        // South Pacific gyre — the remotest water on Earth.
        assert_eq!(lookup(-48.0, -123.0), None);
    }

    #[test]
    fn distance_conversion_matches_known_separations() {
        // Antipodal points are half the circumference apart; the squared chord
        // between them on a unit sphere is 4.
        let half_circumference = std::f64::consts::PI * EARTH_RADIUS_KM;
        assert!((unit_sphere_sq_to_km(4.0) - half_circumference).abs() < 1.0);
        assert_eq!(unit_sphere_sq_to_km(0.0), 0.0);
    }

    #[test]
    fn tags_are_ordered_coarse_to_fine() {
        let place = Place {
            country: Some("France".into()),
            region: Some("Brittany".into()),
            city: Some("Rennes".into()),
        };
        assert_eq!(place.tags(), vec!["France", "Brittany", "Rennes"]);
    }

    #[test]
    fn tags_skip_absent_fields() {
        let place = Place {
            country: Some("Norway".into()),
            region: None,
            city: None,
        };
        assert_eq!(place.tags(), vec!["Norway"]);
    }
}
