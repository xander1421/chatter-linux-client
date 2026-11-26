use std::collections::HashMap;
use std::time::{Duration, Instant};

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::pairing::PairingBeacon;
use chatter_mesh_link::{InboundLink, LinkRole};
use parking_lot::Mutex;

use super::super::config::ConnectionAdmission;
use super::{MeshConfig, MeshState};

const MATCHED_SCAN_LIFETIME: Duration = Duration::from_secs(15);
const MAX_MATCHED_ADDRESSES: usize = 128;
const PAIRED_PROOF_TIMEOUT: Duration = Duration::from_secs(30);
const PAIRING_PREVIEW: Duration = Duration::from_secs(8);
const INBOUND_RESERVATION: Duration = Duration::from_secs(5);
const PREVIEW_COOLDOWN: Duration = Duration::from_secs(20);
const PROOF_BACKOFF_RESET: Duration = Duration::from_secs(120);

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
    let paired = match &inbound.beacon {
        Some(preface) => state.paired_contact_for_preface(preface),
        None => recent_contact(state, scans, &inbound.address, now),
    };
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
    pub(super) role: Option<LinkRole>,
    pub(super) preview: Option<(LinkRole, String)>,
    pub(super) candidate: Option<PeerId>,
    preview_deadline: Instant,
}

impl LinkAdmission {
    pub(super) fn new(expected_contact: Option<PeerId>, now: Instant) -> Self {
        Self {
            expected_contact,
            proof_deadline: now + PAIRED_PROOF_TIMEOUT,
            role: None,
            preview: None,
            candidate: None,
            preview_deadline: now + PAIRING_PREVIEW,
        }
    }

    pub(super) fn permits_proof(&self, config: &MeshConfig, now: Instant) -> bool {
        self.expected_contact.as_ref().map_or_else(
            || {
                if !config.pairing_active(now) {
                    return false;
                }
                match (&self.candidate, config.pairing_peer(now)) {
                    (Some(candidate), Some(selected)) => *candidate == selected,
                    _ => now < self.preview_deadline,
                }
            },
            |_| now < self.proof_deadline,
        )
    }
}

/// A peripheral preview can reserve its slot from a radio callback. An
/// independent central slot remains available to discover the intended peer.
#[derive(Default)]
pub(super) struct PairingPreviews {
    inbound: Option<(String, Instant)>,
    cooling: HashMap<String, Instant>,
}

impl PairingPreviews {
    pub(super) fn reserve_inbound(&mut self, address: &str, now: Instant) -> bool {
        if self.is_cooling(address, now) {
            return false;
        }
        if let Some((reserved, until)) = &self.inbound
            && now < *until
        {
            return reserved == address;
        }
        self.inbound = Some((address.to_owned(), now + INBOUND_RESERVATION));
        true
    }

    pub(super) fn hold_inbound(&mut self, address: &str, window_end: Instant) {
        self.inbound = Some((address.to_owned(), window_end));
    }

    pub(super) fn allows_central(&self, address: &str, now: Instant) -> bool {
        !self.is_cooling(address, now)
            && !self
                .inbound
                .as_ref()
                .is_some_and(|(held, until)| held == address && now < *until)
    }

    fn is_cooling(&self, address: &str, now: Instant) -> bool {
        self.cooling.get(address).is_some_and(|until| now < *until)
    }

    pub(super) fn release(&mut self, role: LinkRole, address: String, now: Instant, cool: bool) {
        if role == LinkRole::Peripheral
            && self
                .inbound
                .as_ref()
                .is_some_and(|(held, _)| *held == address)
        {
            self.inbound = None;
        }
        if cool {
            self.cooling.retain(|_, until| now < *until);
            if self.cooling.len() < MAX_MATCHED_ADDRESSES {
                self.cooling.insert(address, now + PREVIEW_COOLDOWN);
            }
        }
    }
}

/// Keep failed proof backoff across private address and local radio changes.
/// An incoming connection can still prove the pairing and clear the delay.
#[derive(Default)]
pub(super) struct FailedPairingProofs {
    contacts: HashMap<PeerId, (u8, Instant, Instant)>,
}

impl FailedPairingProofs {
    pub(super) fn blocks(&self, contact: &PeerId, now: Instant) -> bool {
        self.contacts
            .get(contact)
            .is_some_and(|(_, retry, _)| now < *retry)
    }

    pub(super) fn record(&mut self, contact: PeerId, now: Instant) {
        self.contacts.retain(|_, (_, _, expiry)| now < *expiry);
        if self.contacts.len() >= chatter_mesh_link::registry::MAX_PINNED_IDENTITIES
            && !self.contacts.contains_key(&contact)
        {
            return;
        }
        let failures = self
            .contacts
            .get(&contact)
            .map_or(0, |(failures, _, _)| *failures);
        let delay = Duration::from_secs((5u64 << failures.min(4)).min(60));
        self.contacts.insert(
            contact,
            (
                failures.saturating_add(1),
                now + delay,
                now + PROOF_BACKOFF_RESET,
            ),
        );
    }

    pub(super) fn clear(&mut self, contact: &PeerId) {
        self.contacts.remove(contact);
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

    #[test]
    fn only_the_selected_preview_outlives_its_short_deadline() {
        let config = MeshConfig::default();
        let now = Instant::now();
        config.set_pairing(true, now);
        let selected = PeerId::from_bytes(&[9; 8]);
        let mut record = LinkAdmission::new(None, now);
        record.candidate = Some(selected.clone());
        assert!(record.permits_proof(&config, now));
        assert!(!record.permits_proof(&config, now + PAIRING_PREVIEW));
        config.select_pairing_peer(selected, now);
        assert!(record.permits_proof(&config, now + PAIRING_PREVIEW));
        config.select_pairing_peer(PeerId::from_bytes(&[8; 8]), now);
        assert!(!record.permits_proof(&config, now));
    }

    #[test]
    fn failed_proof_backoff_grows_and_expires_per_contact() {
        let now = Instant::now();
        let contact = PeerId::from_bytes(&[7; 8]);
        let other = PeerId::from_bytes(&[8; 8]);
        let mut backoff = FailedPairingProofs::default();
        backoff.record(contact.clone(), now);
        assert!(!backoff.blocks(&other, now));
        let retry = now + Duration::from_secs(5);
        assert!(!backoff.blocks(&contact, retry));
        backoff.record(contact.clone(), retry);
        assert!(backoff.blocks(&contact, retry + Duration::from_secs(5)));
        assert!(!backoff.blocks(&contact, retry + Duration::from_secs(10)));
        let reset = retry + PROOF_BACKOFF_RESET;
        backoff.record(contact.clone(), reset);
        assert!(!backoff.blocks(&contact, reset + Duration::from_secs(5)));
        backoff.clear(&contact);
        assert!(!backoff.blocks(&contact, reset));
    }
}
