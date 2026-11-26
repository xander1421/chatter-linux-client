use std::collections::HashMap;
use std::time::{Duration, Instant};

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::pairing::PairingBeacon;
use chatter_mesh_link::InboundLink;
use parking_lot::Mutex;

use super::super::config::ConnectionAdmission;
use super::{MeshConfig, MeshState};

const MATCHED_SCAN_LIFETIME: Duration = Duration::from_secs(15);
const MAX_MATCHED_ADDRESSES: usize = 128;
pub(super) const PAIRING_LINK_LIMIT: usize = 2;
const PAIRED_PROOF_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn recent_contact(
    state: &MeshState,
    scans: &Mutex<MatchedScans>,
    address: &str,
    now: Instant,
) -> Option<PeerId> {
    let beacon = scans.lock().recent(address, now)?;
    state.paired_contact_for_beacon(&beacon)
}

pub(super) fn inbound_admission(
    config: &MeshConfig,
    state: &MeshState,
    scans: &Mutex<MatchedScans>,
    inbound: &InboundLink,
    now: Instant,
) -> Option<ConnectionAdmission> {
    let paired = recent_contact(state, scans, &inbound.address, now).or_else(|| {
        inbound
            .beacon
            .as_ref()
            .and_then(|beacon| state.paired_contact_for_beacon(beacon))
    });
    config.connection_admission(paired, now)
}

#[derive(Default)]
pub(super) struct MatchedScans {
    entries: HashMap<String, (PairingBeacon, Instant)>,
}

impl MatchedScans {
    /// The caller has matched this beacon to a current pairing.
    pub(super) fn remember(&mut self, address: &str, beacon: PairingBeacon, now: Instant) {
        self.entries
            .retain(|_, (_, seen)| now.saturating_duration_since(*seen) < MATCHED_SCAN_LIFETIME);
        if self.entries.len() == MAX_MATCHED_ADDRESSES
            && !self.entries.contains_key(address)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(address, _)| address.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(address.to_owned(), (beacon, now));
    }

    pub(super) fn recent(&self, address: &str, now: Instant) -> Option<PairingBeacon> {
        self.entries
            .get(address)
            .filter(|(_, seen)| now.saturating_duration_since(*seen) < MATCHED_SCAN_LIFETIME)
            .map(|(beacon, _)| *beacon)
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

pub(super) struct LinkAdmission {
    pub(super) expected_contact: Option<PeerId>,
    pub(super) proof_deadline: Instant,
}

impl LinkAdmission {
    pub(super) fn new(expected_contact: Option<PeerId>, now: Instant) -> Self {
        Self {
            expected_contact,
            proof_deadline: now + PAIRED_PROOF_TIMEOUT,
        }
    }

    pub(super) fn permits_proof(&self, config: &MeshConfig, now: Instant) -> bool {
        now < self.proof_deadline && (self.expected_contact.is_some() || config.pairing_active(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matched_scan_is_address_specific_and_expires() {
        let mut cache = MatchedScans::default();
        let now = Instant::now();
        cache.remember("paired", [42; 13], now);
        assert_eq!(cache.recent("paired", now), Some([42; 13]));
        assert_eq!(cache.recent("unrelated", now), None);
        assert_eq!(cache.recent("paired", now + MATCHED_SCAN_LIFETIME), None);
        cache.clear();
        assert_eq!(cache.recent("paired", now), None);
    }

    #[test]
    fn cache_evicts_oldest_match_at_capacity() {
        let mut cache = MatchedScans::default();
        let now = Instant::now();
        cache.remember("oldest", [1; 13], now);
        let later = now + Duration::from_secs(1);
        for index in 1..MAX_MATCHED_ADDRESSES {
            cache.remember(&index.to_string(), [2; 13], later);
        }
        cache.remember("newest", [3; 13], later);
        assert_eq!(cache.recent("oldest", later), None);
        assert_eq!(cache.recent("newest", later), Some([3; 13]));
        assert_eq!(cache.entries.len(), MAX_MATCHED_ADDRESSES);
    }
}
