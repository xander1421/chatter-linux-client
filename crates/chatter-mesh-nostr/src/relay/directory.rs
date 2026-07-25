//! The geo relay directory: which relays to use for a given geohash.
//!
//! Ported from `bitchat/Nostr/GeoRelayDirectory.swift` and the CSV it ships,
//! `bitchat/relays/online_relays_gps.csv`.
//!
//! This module is pure: it parses a CSV and ranks its rows by distance. Fetching
//! the CSV, caching it, and refreshing it belong to the integration layer, which
//! already owns the Tor HTTP path — the same reason [`super::RelayConnector`]
//! exists. Nothing here reads a file or opens a socket, so the whole thing is
//! testable as data.
//!
//! # Why the ordering is pinned
//!
//! Two devices in the same geohash must independently pick the *same* relays, or a
//! publisher and a subscriber standing next to each other never see one another.
//! Selection is therefore fully deterministic: sort by distance, break ties by URL,
//! take the first `count`. That tie-break is not cosmetic — without it two clients
//! with equidistant relays can disagree, and the channel silently splits in two.
//!
//! # Format
//!
//! ```text
//! Relay URL,Latitude,Longitude
//! relay.lab.rytswd.com,49.4543,11.0746
//! relay.paulstephenborile.com:443,49.4543,11.0746
//! ```
//!
//! The first column is a bare address, not a URL: no scheme, sometimes a port.
//! Rows are normalised through [`super::directory_address`] and re-prefixed with
//! `wss://`, which is what bitchat dials — a `ws://` row is *upgraded*, never
//! dialled in the clear.

use super::directory_address;

/// How many relays a geohash channel uses.
pub const DEFAULT_GEO_RELAY_COUNT: usize = 5;

/// Mean Earth radius, in kilometres.
const EARTH_RADIUS_KM: f64 = 6371.0;

/// One relay and where it is.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoRelay {
    /// The full `wss://` URL to dial.
    pub url: String,
    pub lat: f64,
    pub lon: f64,
}

/// Parses the directory CSV.
///
/// Every rule here is "skip the row, keep the file": the CSV is downloaded from a
/// third party and one corrupt line must not cost the other four hundred relays.
/// A row is skipped when it has fewer than three columns, when the address is not
/// a relay address, or when the coordinates are not real numbers on the globe.
///
/// **Deviation from bitchat:** duplicate addresses are collapsed, keeping the first
/// row. Swift dedupes on the whole `(host, lat, lon)` triple, so the same relay
/// listed twice with different coordinates survives twice — and can then occupy two
/// of the five slots for one geohash, halving the geographic spread the selection
/// exists to produce. Keeping the first occurrence is deterministic and never
/// wrong in a way the triple-dedupe is not.
pub fn parse_directory(csv: &str) -> Vec<GeoRelay> {
    let mut relays: Vec<GeoRelay> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    let lines = csv
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty());

    for (index, line) in lines.enumerate() {
        // The header, when there is one. Only the first row may be a header, so a
        // later line that happens to mention "relay url" is still data.
        if index == 0 && line.to_lowercase().contains("relay url") {
            continue;
        }

        let mut columns = line.split(',').map(str::trim);
        let (Some(address), Some(lat), Some(lon)) =
            (columns.next(), columns.next(), columns.next())
        else {
            continue;
        };

        let Some(host) = directory_address(address) else {
            continue;
        };
        let (Ok(lat), Ok(lon)) = (lat.parse::<f64>(), lon.parse::<f64>()) else {
            continue;
        };
        // `"NaN"` parses as a number in both Swift and Rust; a coordinate off the
        // globe is a corrupt row, not a relay that happens to be far away.
        if !is_on_earth(lat, lon) {
            continue;
        }

        let url = format!("wss://{host}");
        if seen.iter().any(|existing| existing == &url) {
            continue;
        }
        seen.push(url.clone());
        relays.push(GeoRelay { url, lat, lon });
    }

    relays
}

/// The `count` relays closest to a point, nearest first.
///
/// Ties break by URL so that every device holding the same directory picks the same
/// set — see the module docs. Fewer than `count` relays are returned when the
/// directory is smaller than that; `count == 0` returns nothing.
pub fn relays_for_geohash(
    relays: &[GeoRelay],
    geohash_lat: f64,
    geohash_lon: f64,
    count: usize,
) -> Vec<&GeoRelay> {
    if count == 0 {
        return Vec::new();
    }

    let mut ranked: Vec<(f64, &GeoRelay)> = relays
        .iter()
        .map(|relay| {
            (
                haversine_km(geohash_lat, geohash_lon, relay.lat, relay.lon),
                relay,
            )
        })
        .collect();

    // `total_cmp` rather than `partial_cmp`: a NaN distance from a hostile or
    // simply broken input must produce a defined order, not a panic and not a
    // corrupted sort.
    ranked.sort_by(|(a_distance, a), (b_distance, b)| {
        a_distance
            .total_cmp(b_distance)
            .then_with(|| a.url.cmp(&b.url))
    });

    ranked
        .into_iter()
        .take(count)
        .map(|(_, relay)| relay)
        .collect()
}

fn is_on_earth(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

/// Great-circle distance in kilometres, matching `GeoRelayDirectory.haversineKm`.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    EARTH_RADIUS_KM * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Representative rows from `bitchat/relays/online_relays_gps.csv`.
    const SAMPLE: &str = "Relay URL,Latitude,Longitude\n\
        relay.lab.rytswd.com,49.4543,11.0746\n\
        relay.paulstephenborile.com:443,43.6532,-79.3832\n\
        relay.binaryrobot.com,43.6532,-79.3832\n\
        nostr-2.21crypto.ch,47.5356,8.73209\n";

    fn urls(relays: &[&GeoRelay]) -> Vec<String> {
        relays.iter().map(|relay| relay.url.clone()).collect()
    }

    #[test]
    fn parses_the_bundled_format() {
        let relays = parse_directory(SAMPLE);
        assert_eq!(relays.len(), 4);
        assert_eq!(
            relays.first().map(|r| r.url.as_str()),
            Some("wss://relay.lab.rytswd.com")
        );
        assert_eq!(relays.first().map(|r| r.lat), Some(49.4543));
        assert_eq!(relays.first().map(|r| r.lon), Some(11.0746));
        // `:443` is the default port for wss and drops out of the canonical form.
        assert_eq!(
            relays.get(1).map(|r| r.url.as_str()),
            Some("wss://relay.paulstephenborile.com")
        );
    }

    #[test]
    fn a_header_is_only_skipped_on_the_first_line() {
        // No header at all: every line is data.
        let relays = parse_directory("a.example,1,2\nb.example,3,4\n");
        assert_eq!(relays.len(), 2);

        // A later line mentioning the header text is a (bad) data row, not a header
        // — and is skipped for having an unparseable address, not for its wording.
        let relays = parse_directory("a.example,1,2\nRelay URL,Latitude,Longitude\n");
        assert_eq!(relays.len(), 1);
    }

    #[test]
    fn handles_crlf_blank_lines_and_stray_whitespace() {
        let csv = "Relay URL,Latitude,Longitude\r\n\r\n  a.example , 1.5 , 2.5 \r\n\n";
        let relays = parse_directory(csv);
        assert_eq!(
            relays,
            vec![GeoRelay {
                url: "wss://a.example".to_string(),
                lat: 1.5,
                lon: 2.5,
            }]
        );
    }

    #[test]
    fn skips_rows_it_cannot_use_without_losing_the_file() {
        let csv = "\
a.example,1,2\n\
too,few\n\
,3,4\n\
not a host,5,6\n\
b.example,north,east\n\
c.example,91,0\n\
d.example,0,181\n\
e.example,NaN,0\n\
f.example,inf,0\n\
ftp://g.example,1,2\n\
h.example,7,8\n";
        let relays = parse_directory(csv);
        assert_eq!(
            urls(&relays.iter().collect::<Vec<_>>()),
            vec!["wss://a.example", "wss://h.example"]
        );
    }

    #[test]
    fn extra_columns_are_ignored() {
        let relays = parse_directory("a.example,1,2,extra,columns\n");
        assert_eq!(relays.len(), 1);
    }

    #[test]
    fn ws_rows_are_upgraded_and_ports_preserved() {
        let relays = parse_directory("ws://a.example,1,2\nb.example:8080,3,4\n");
        assert_eq!(
            urls(&relays.iter().collect::<Vec<_>>()),
            vec!["wss://a.example", "wss://b.example:8080"]
        );
    }

    #[test]
    fn duplicate_addresses_collapse_to_the_first_row() {
        let csv = "a.example,1,2\na.example:443,50,60\nA.EXAMPLE,70,80\nb.example,3,4\n";
        let relays = parse_directory(csv);
        assert_eq!(relays.len(), 2);
        assert_eq!(
            relays.first().map(|r| r.lat),
            Some(1.0),
            "the first row wins"
        );
    }

    #[test]
    fn an_empty_directory_parses_to_nothing() {
        assert!(parse_directory("").is_empty());
        assert!(parse_directory("Relay URL,Latitude,Longitude\n").is_empty());
        assert!(parse_directory("\n\n\n").is_empty());
    }

    // --- Selection ---

    fn directory() -> Vec<GeoRelay> {
        parse_directory(
            "\
nuremberg.example,49.4543,11.0746\n\
zurich.example,47.5356,8.73209\n\
toronto.example,43.6532,-79.3832\n\
sydney.example,-33.8688,151.2093\n\
saopaulo.example,-23.5505,-46.6333\n\
tokyo.example,35.6762,139.6503\n",
        )
    }

    #[test]
    fn picks_the_nearest_relays_nearest_first() {
        let relays = directory();
        // Munich: Nuremberg is ~150 km, Zurich ~240 km, then the intercontinentals.
        let closest = relays_for_geohash(&relays, 48.1351, 11.5820, 3);
        assert_eq!(
            urls(&closest),
            vec![
                "wss://nuremberg.example",
                "wss://zurich.example",
                "wss://toronto.example",
            ]
        );
    }

    #[test]
    fn selection_is_the_same_from_the_same_directory_in_any_row_order() {
        // Two devices with the same directory must agree, whatever order the rows
        // arrived in — otherwise a publisher and a subscriber in one geohash never
        // meet.
        let mut forward = directory();
        let mut reversed = forward.clone();
        reversed.reverse();
        forward.rotate_left(2);

        let a = relays_for_geohash(&forward, 35.0, 139.0, DEFAULT_GEO_RELAY_COUNT);
        let b = relays_for_geohash(&reversed, 35.0, 139.0, DEFAULT_GEO_RELAY_COUNT);
        assert_eq!(urls(&a), urls(&b));
    }

    #[test]
    fn ties_break_by_url() {
        let relays = vec![
            GeoRelay {
                url: "wss://zzz.example".to_string(),
                lat: 10.0,
                lon: 10.0,
            },
            GeoRelay {
                url: "wss://aaa.example".to_string(),
                lat: 10.0,
                lon: 10.0,
            },
            GeoRelay {
                url: "wss://mmm.example".to_string(),
                lat: 10.0,
                lon: 10.0,
            },
        ];
        assert_eq!(
            urls(&relays_for_geohash(&relays, 10.0, 10.0, 2)),
            vec!["wss://aaa.example", "wss://mmm.example"]
        );
    }

    #[test]
    fn returns_at_most_count_and_never_more_than_it_has() {
        let relays = directory();
        assert_eq!(relays_for_geohash(&relays, 0.0, 0.0, 1).len(), 1);
        assert_eq!(
            relays_for_geohash(&relays, 0.0, 0.0, DEFAULT_GEO_RELAY_COUNT).len(),
            DEFAULT_GEO_RELAY_COUNT
        );
        assert_eq!(
            relays_for_geohash(&relays, 0.0, 0.0, 100).len(),
            relays.len()
        );
        assert!(relays_for_geohash(&relays, 0.0, 0.0, 0).is_empty());
        assert!(relays_for_geohash(&[], 0.0, 0.0, 5).is_empty());
    }

    #[test]
    fn distance_is_a_great_circle_not_a_flat_difference() {
        // London → Paris, ~344 km. A naive Euclidean metric on degrees gets this
        // badly wrong at high latitudes, which is where half the relays are.
        let km = haversine_km(51.5074, -0.1278, 48.8566, 2.3522);
        assert!((km - 344.0).abs() < 5.0, "{km}");

        // Antipodal-ish: Madrid → Wellington, ~19_800 km.
        let km = haversine_km(40.4168, -3.7038, -41.2865, 174.7762);
        assert!((km - 19_800.0).abs() < 200.0, "{km}");

        // Same point.
        assert!(haversine_km(10.0, 20.0, 10.0, 20.0).abs() < 1e-9);

        // Across the antimeridian: 2° apart, not 358°.
        let km = haversine_km(0.0, 179.0, 0.0, -179.0);
        assert!((km - 222.0).abs() < 5.0, "{km}");
    }

    #[test]
    fn selection_is_total_even_with_a_broken_coordinate() {
        // `parse_directory` rejects these, but a caller can build `GeoRelay`
        // directly and sorting must still terminate with a defined order.
        let relays = vec![
            GeoRelay {
                url: "wss://nan.example".to_string(),
                lat: f64::NAN,
                lon: 0.0,
            },
            GeoRelay {
                url: "wss://near.example".to_string(),
                lat: 0.0,
                lon: 0.0,
            },
        ];
        let closest = relays_for_geohash(&relays, 0.0, 0.0, 2);
        assert_eq!(closest.len(), 2);
        assert_eq!(
            closest.first().map(|r| r.url.as_str()),
            Some("wss://near.example"),
            "a real relay must outrank an unplaceable one"
        );
    }
}
