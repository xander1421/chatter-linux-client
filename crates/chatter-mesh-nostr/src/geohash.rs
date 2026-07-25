//! Geohash encoding, decoding, and the precision levels bitchat's location
//! channels are built on.
//!
//! Ported from `bitchat/Protocols/Geohash.swift` and
//! `bitchat/Protocols/LocationChannel.swift`.
//!
//! The alphabet is the standard geohash base32 — `0123456789bcdefghjkmnpqrstuvwxyz`
//! — which omits `a`, `i`, `l`, and `o` so that hand-transcribed cells cannot be
//! confused with digits.
//!
//! Geohashes are canonically lowercase. Input is accepted in either case; every
//! string this module *returns* is lowercase, so a cell used as a Nostr `g` tag
//! matches what bitchat publishes regardless of how the caller spelled it.

use crate::error::NostrError;
use crate::types::GeohashLevel;

/// Standard geohash base32 alphabet.
const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Longest geohash bitchat accepts. Beyond 12 characters the cell is smaller
/// than GPS error, so the extra characters carry no information.
pub const MAX_PRECISION: usize = 12;

/// Value of a base32 character, or `None` if it is not in the alphabet.
fn decode_char(c: char) -> Option<u8> {
    if !c.is_ascii() {
        return None;
    }
    let lower = c.to_ascii_lowercase() as u8;
    BASE32.iter().position(|&b| b == lower).map(|i| i as u8)
}

/// True for a non-empty base32 geohash of at most [`MAX_PRECISION`] characters.
///
/// Case-insensitive, matching `Geohash.isValidGeohash`.
pub fn is_valid(geohash: &str) -> bool {
    let len = geohash.chars().count();
    (1..=MAX_PRECISION).contains(&len) && geohash.chars().all(|c| decode_char(c).is_some())
}

/// Encode coordinates to a geohash of exactly `precision` characters.
///
/// Latitude is clamped to ±90 and longitude to ±180, as in the Swift original.
/// Non-finite coordinates are rejected rather than clamped: `NaN` compares false
/// against every bound, so clamping it would silently produce a cell at the
/// origin.
pub fn encode(lat: f64, lon: f64, precision: usize) -> Result<String, NostrError> {
    if precision == 0 {
        return Err(NostrError::InvalidGeohash("precision must be at least 1"));
    }
    if precision > MAX_PRECISION {
        return Err(NostrError::InvalidGeohash("precision must be at most 12"));
    }
    if !lat.is_finite() || !lon.is_finite() {
        return Err(NostrError::InvalidGeohash("coordinates must be finite"));
    }

    let lat = lat.clamp(-90.0, 90.0);
    let lon = lon.clamp(-180.0, 180.0);

    let mut lat_interval = (-90.0f64, 90.0f64);
    let mut lon_interval = (-180.0f64, 180.0f64);

    let mut is_even = true;
    let mut bit = 0u32;
    let mut ch = 0usize;
    let mut out = String::with_capacity(precision);

    while out.len() < precision {
        if is_even {
            let mid = (lon_interval.0 + lon_interval.1) / 2.0;
            if lon >= mid {
                ch |= 1 << (4 - bit);
                lon_interval.0 = mid;
            } else {
                lon_interval.1 = mid;
            }
        } else {
            let mid = (lat_interval.0 + lat_interval.1) / 2.0;
            if lat >= mid {
                ch |= 1 << (4 - bit);
                lat_interval.0 = mid;
            } else {
                lat_interval.1 = mid;
            }
        }

        is_even = !is_even;
        if bit < 4 {
            bit += 1;
        } else {
            // `ch` is masked to 5 bits by construction, so indexing is in range.
            out.push(BASE32[ch & 0x1f] as char);
            bit = 0;
            ch = 0;
        }
    }

    Ok(out)
}

/// Decode a geohash to the centre of its bounding box, as `(lat, lon)`.
pub fn decode(geohash: &str) -> Result<(f64, f64), NostrError> {
    let (lat_min, lat_max, lon_min, lon_max) = bounds(geohash)?;
    Ok((
        (lat_min + lat_max) / 2.0,
        (lon_min + lon_max) / 2.0,
    ))
}

/// Decode a geohash to its bounding box, as `(lat_min, lat_max, lon_min, lon_max)`.
pub fn bounds(geohash: &str) -> Result<(f64, f64, f64, f64), NostrError> {
    if !is_valid(geohash) {
        return Err(NostrError::InvalidGeohash(
            "not a base32 geohash of 1 to 12 characters",
        ));
    }

    let mut lat_interval = (-90.0f64, 90.0f64);
    let mut lon_interval = (-180.0f64, 180.0f64);
    let mut is_even = true;

    for c in geohash.chars() {
        // `is_valid` above guarantees every character decodes.
        let cd = match decode_char(c) {
            Some(v) => v,
            None => {
                return Err(NostrError::InvalidGeohash(
                    "not a base32 geohash of 1 to 12 characters",
                ));
            }
        };
        for mask in [16u8, 8, 4, 2, 1] {
            if is_even {
                let mid = (lon_interval.0 + lon_interval.1) / 2.0;
                if cd & mask != 0 {
                    lon_interval.0 = mid;
                } else {
                    lon_interval.1 = mid;
                }
            } else {
                let mid = (lat_interval.0 + lat_interval.1) / 2.0;
                if cd & mask != 0 {
                    lat_interval.0 = mid;
                } else {
                    lat_interval.1 = mid;
                }
            }
            is_even = !is_even;
        }
    }

    Ok((
        lat_interval.0,
        lat_interval.1,
        lon_interval.0,
        lon_interval.1,
    ))
}

/// The location-channel level for a geohash, by its character count.
///
/// Only the six precisions bitchat's channel picker offers map to a level;
/// anything else (1, 3, or 9 and finer) is a valid geohash with no channel
/// level, hence `None`. Callers enforcing the presence rule must use the
/// character count directly — see [`crate::builders::geohash_presence`] — and
/// must not treat `None` as "no restriction".
pub fn level_for(geohash: &str) -> Option<GeohashLevel> {
    if !is_valid(geohash) {
        return None;
    }
    match geohash.chars().count() {
        2 => Some(GeohashLevel::Region),
        4 => Some(GeohashLevel::Province),
        5 => Some(GeohashLevel::City),
        6 => Some(GeohashLevel::Neighborhood),
        7 => Some(GeohashLevel::Block),
        8 => Some(GeohashLevel::Building),
        _ => None,
    }
}

/// The enclosing cell of `geohash` at `precision` characters.
///
/// Truncation is exact for geohashes: a prefix is always the cell that contains
/// the longer one. Widening (a `precision` longer than the input) is an error
/// rather than a pad, because there is no single parent to widen to.
pub fn parent(geohash: &str, precision: usize) -> Result<String, NostrError> {
    if !is_valid(geohash) {
        return Err(NostrError::InvalidGeohash(
            "not a base32 geohash of 1 to 12 characters",
        ));
    }
    if precision == 0 {
        return Err(NostrError::InvalidGeohash("precision must be at least 1"));
    }
    let len = geohash.chars().count();
    if precision > len {
        return Err(NostrError::InvalidGeohash(
            "precision is finer than the geohash",
        ));
    }
    Ok(geohash
        .chars()
        .take(precision)
        .map(|c| c.to_ascii_lowercase())
        .collect())
}

/// Lowercase a validated geohash for use as a tag value.
pub(crate) fn normalize(geohash: &str) -> Result<String, NostrError> {
    if !is_valid(geohash) {
        return Err(NostrError::InvalidGeohash(
            "not a base32 geohash of 1 to 12 characters",
        ));
    }
    Ok(geohash.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical geohash example: Jutland, Denmark.
    #[test]
    fn encodes_the_canonical_reference_coordinate() {
        assert_eq!(encode(57.64911, 10.40744, 11).unwrap(), "u4pruydqqvj");
        assert_eq!(encode(57.64911, 10.40744, 5).unwrap(), "u4pru");
        assert_eq!(encode(57.64911, 10.40744, 2).unwrap(), "u4");
    }

    #[test]
    fn encodes_known_city_coordinates() {
        // San Francisco.
        assert_eq!(encode(37.7749, -122.4194, 6).unwrap(), "9q8yyk");
        assert_eq!(encode(37.7749, -122.4194, 12).unwrap(), "9q8yyk8ytpxr");
        // Greenwich observatory, just west of the prime meridian.
        assert_eq!(encode(51.4779, -0.0015, 7).unwrap(), "gcpuzgq");
        // Statue of Liberty.
        assert_eq!(encode(40.6892, -74.0445, 7).unwrap(), "dr5r7p4");
        // Null island: the origin sits on both the equator and the meridian, so
        // every comparison takes the `>=` branch.
        assert_eq!(encode(0.0, 0.0, 5).unwrap(), "s0000");
    }

    #[test]
    fn decode_returns_a_point_inside_the_cell_it_came_from() {
        for (lat, lon) in [
            (57.64911, 10.40744),
            (37.7749, -122.4194),
            (-33.8688, 151.2093),
            (0.0, 0.0),
            (89.9, -179.9),
        ] {
            for precision in 1..=MAX_PRECISION {
                let gh = encode(lat, lon, precision).unwrap();
                let (dlat, dlon) = decode(&gh).unwrap();
                assert_eq!(
                    encode(dlat, dlon, precision).unwrap(),
                    gh,
                    "round trip failed for {lat},{lon} at precision {precision}"
                );
            }
        }
    }

    #[test]
    fn decode_of_a_long_geohash_is_close_to_the_source_coordinate() {
        let gh = encode(57.64911, 10.40744, 11).unwrap();
        let (lat, lon) = decode(&gh).unwrap();
        assert!((lat - 57.64911).abs() < 1e-4, "lat drifted: {lat}");
        assert!((lon - 10.40744).abs() < 1e-4, "lon drifted: {lon}");
    }

    #[test]
    fn bounds_contain_the_centre_and_shrink_with_precision() {
        let coarse = bounds("u4").unwrap();
        let fine = bounds("u4pruydqqvj").unwrap();
        assert!(fine.1 - fine.0 < coarse.1 - coarse.0);
        assert!(fine.3 - fine.2 < coarse.3 - coarse.2);

        let (lat, lon) = decode("u4pruydqqvj").unwrap();
        assert!(lat >= fine.0 && lat <= fine.1);
        assert!(lon >= fine.2 && lon <= fine.3);
    }

    #[test]
    fn coarser_geohashes_are_prefixes_of_finer_ones() {
        let lat = 40.6892;
        let lon = -74.0445;
        let block = encode(lat, lon, 7).unwrap();
        let neighborhood = encode(lat, lon, 6).unwrap();
        let city = encode(lat, lon, 5).unwrap();
        let province = encode(lat, lon, 4).unwrap();
        let region = encode(lat, lon, 2).unwrap();

        assert!(block.starts_with(&neighborhood));
        assert!(neighborhood.starts_with(&city));
        assert!(city.starts_with(&province));
        assert!(province.starts_with(&region));
    }

    #[test]
    fn encode_rejects_out_of_range_precision_and_non_finite_coordinates() {
        assert!(encode(0.0, 0.0, 0).is_err());
        assert!(encode(0.0, 0.0, 13).is_err());
        assert!(encode(f64::NAN, 0.0, 5).is_err());
        assert!(encode(0.0, f64::INFINITY, 5).is_err());
    }

    #[test]
    fn encode_clamps_coordinates_beyond_the_poles_and_antimeridian() {
        assert_eq!(
            encode(1000.0, 1000.0, 6).unwrap(),
            encode(90.0, 180.0, 6).unwrap()
        );
        assert_eq!(
            encode(-1000.0, -1000.0, 6).unwrap(),
            encode(-90.0, -180.0, 6).unwrap()
        );
    }

    #[test]
    fn validity_follows_the_base32_alphabet() {
        assert!(is_valid("u4pruydqqvj"));
        assert!(is_valid("9"));
        assert!(is_valid("U4PRU")); // case-insensitive
        assert!(!is_valid(""));
        assert!(!is_valid("u4pruydqqvjxy")); // 13 characters
        // a, i, l and o are deliberately absent from the alphabet.
        for bad in ["a", "i", "l", "o", "u4pra", "u4pr-", "u4pr ", "u4prü"] {
            assert!(!is_valid(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn decode_rejects_invalid_geohashes() {
        assert!(decode("").is_err());
        assert!(decode("abc").is_err());
        assert!(decode("u4pruydqqvjxy").is_err());
    }

    #[test]
    fn levels_map_only_the_six_channel_precisions() {
        assert_eq!(level_for("u4"), Some(GeohashLevel::Region));
        assert_eq!(level_for("u4pr"), Some(GeohashLevel::Province));
        assert_eq!(level_for("u4pru"), Some(GeohashLevel::City));
        assert_eq!(level_for("u4pruy"), Some(GeohashLevel::Neighborhood));
        assert_eq!(level_for("u4pruyd"), Some(GeohashLevel::Block));
        assert_eq!(level_for("u4pruydq"), Some(GeohashLevel::Building));

        assert_eq!(level_for("u"), None);
        assert_eq!(level_for("u4p"), None);
        assert_eq!(level_for("u4pruydqq"), None);
        assert_eq!(level_for("not-a-geohash"), None);
    }

    #[test]
    fn levels_agree_with_the_presence_rule() {
        for gh in ["u4", "u4pr", "u4pru"] {
            assert!(level_for(gh).unwrap().allows_presence(), "{gh}");
        }
        for gh in ["u4pruy", "u4pruyd", "u4pruydq"] {
            assert!(!level_for(gh).unwrap().allows_presence(), "{gh}");
        }
    }

    #[test]
    fn parent_truncates_to_the_enclosing_cell() {
        assert_eq!(parent("u4pruydqqvj", 5).unwrap(), "u4pru");
        assert_eq!(parent("u4pruydqqvj", 2).unwrap(), "u4");
        assert_eq!(parent("u4pru", 5).unwrap(), "u4pru");
        assert_eq!(parent("U4PRUYD", 4).unwrap(), "u4pr");
    }

    #[test]
    fn parent_refuses_to_widen_or_take_a_zero_precision() {
        assert!(parent("u4pru", 6).is_err());
        assert!(parent("u4pru", 0).is_err());
        assert!(parent("", 1).is_err());
    }

    #[test]
    fn a_parent_cell_contains_its_child() {
        let child = "u4pruydqqvj";
        let (clat, clon) = decode(child).unwrap();
        for precision in 1..=child.len() {
            let p = parent(child, precision).unwrap();
            let (lat_min, lat_max, lon_min, lon_max) = bounds(&p).unwrap();
            assert!(clat >= lat_min && clat <= lat_max, "precision {precision}");
            assert!(clon >= lon_min && clon <= lon_max, "precision {precision}");
        }
    }

    #[test]
    fn normalize_lowercases_and_rejects_junk() {
        assert_eq!(normalize("U4PRU").unwrap(), "u4pru");
        assert_eq!(normalize("u4pru").unwrap(), "u4pru");
        assert!(normalize("u4pra").is_err());
    }
}
