//! Flood-control policy: whether a received packet is relayed, with what TTL,
//! and after how long.
//!
//! Ported from `Services/RelayController.swift` (the policy),
//! `Services/BLE/BLEReceivePipeline.swift` (`relayDecision`, which maps packet
//! types onto the policy's boolean inputs), and `Services/TransportConfig.swift`
//! (every constant below).
//!
//! The branch order in [`decide`] is load-bearing and mirrors the Swift line for
//! line. Reordering it changes which packets flood and which die — the two
//! failure modes being a broadcast storm and silent message loss.
//!
//! Jitter is injected as a closure rather than drawn from an RNG, so every branch
//! is exactly testable. It is the only nondeterminism in this crate.

use crate::packet::{BitchatPacket, MessageType};

/// `TransportConfig.messageTTLDefault` — the ceiling every decision starts from.
pub const MESSAGE_TTL_DEFAULT: u8 = 7;

/// `TransportConfig.bleHighDegreeThreshold` — neighbour count at which a node
/// treats itself as living in a dense graph and clamps harder.
pub const HIGH_DEGREE_THRESHOLD: usize = 6;

/// `TransportConfig.bleFragmentRelayTtlCap` — sparse-graph fragment/voice cap.
/// Matches [`MESSAGE_TTL_DEFAULT`] so media reaches as far as text.
pub const FRAGMENT_RELAY_TTL_CAP: u8 = 7;

/// `TransportConfig.bleFragmentRelayTtlCapDense` — dense-graph fragment/voice cap.
pub const FRAGMENT_RELAY_TTL_CAP_DENSE: u8 = 5;

/// `TransportConfig.bleFragmentRelayMinDelayMs`.
pub const FRAGMENT_RELAY_MIN_DELAY_MS: u32 = 8;

/// `TransportConfig.bleFragmentRelayMaxDelayMs`.
pub const FRAGMENT_RELAY_MAX_DELAY_MS: u32 = 25;

/// One relay scheduling choice.
///
/// `new_ttl` is meaningful even when `should_relay` is false: the Swift returns
/// the clamped TTL in every branch, and callers log it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayDecision {
    pub should_relay: bool,
    pub new_ttl: u8,
    pub delay_ms: u32,
}

/// The policy's inputs, one field per `RelayController.decide` parameter.
///
/// `isEncrypted` is deliberately absent: the Swift signature takes it as
/// `isEncrypted _: Bool` and never reads it. `is_directed_encrypted` is the one
/// that matters, and it covers more types than `noiseEncrypted` alone — see
/// [`RelayInput::from_packet`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayInput {
    pub ttl: u8,
    pub sender_is_self: bool,
    pub recipient_is_self: bool,
    pub is_directed_encrypted: bool,
    pub is_fragment: bool,
    pub is_directed_fragment: bool,
    pub is_handshake: bool,
    pub is_announce: bool,
    pub is_request_sync: bool,
    pub is_urgent_board_post: bool,
    pub is_voice_frame: bool,
    /// Number of directly connected neighbours.
    pub degree: usize,
    /// Usually [`HIGH_DEGREE_THRESHOLD`].
    pub high_degree_threshold: usize,
}

impl RelayInput {
    /// Maps a packet plus local context onto the policy inputs, per
    /// `BLEReceivePipeline.relayDecision`.
    ///
    /// Unknown `msg_type` values leave every boolean false and therefore take the
    /// broadcast branch. That is intended: relays forward types they do not
    /// understand, which is how bitchat ships new types to a mixed-version mesh.
    pub fn from_packet(
        packet: &BitchatPacket,
        sender_is_self: bool,
        recipient_is_self: bool,
        degree: usize,
        high_degree_threshold: usize,
    ) -> Self {
        let msg_type = packet.msg_type;
        let has_recipient = packet.recipient_id.is_some();

        // Courier envelopes are directed opaque ciphertext like DMs. Ping/pong
        // diagnostics ride the same deterministic path so RTT and hop counts
        // reflect the real route, and directed nostrCarrier uplinks need it to
        // reach a non-adjacent gateway. All four only when a recipient is
        // present — see `BLEReceivePipeline.relayDecision`.
        let is_directed_encrypted = matches!(
            MessageType::from_u8(msg_type),
            Some(
                MessageType::NoiseEncrypted
                    | MessageType::CourierEnvelope
                    | MessageType::Ping
                    | MessageType::Pong
                    | MessageType::NostrCarrier
            )
        ) && has_recipient;

        let is_fragment = msg_type == MessageType::Fragment.as_u8();

        Self {
            ttl: packet.ttl,
            sender_is_self,
            recipient_is_self,
            is_directed_encrypted,
            is_fragment,
            // Swift tests `packet.recipientID != nil`, not "not broadcast". A
            // fragment carrying the explicit all-0xFF recipient therefore takes
            // the directed (uncapped, always-relay) path. Ported as written.
            is_directed_fragment: is_fragment && has_recipient,
            is_handshake: msg_type == MessageType::NoiseHandshake.as_u8(),
            is_announce: msg_type == MessageType::Announce.as_u8(),
            is_request_sync: msg_type == MessageType::RequestSync.as_u8(),
            is_urgent_board_post: msg_type == MessageType::BoardPost.as_u8()
                && board_post_is_urgent(&packet.payload),
            is_voice_frame: msg_type == MessageType::VoiceFrame.as_u8(),
            degree,
            high_degree_threshold,
        }
    }
}

/// The flood-control decision.
///
/// `jitter(lo, hi)` must return a value in `lo..=hi`; it stands in for
/// `Int.random(in:)`. Branch order is exactly `RelayController.decide`'s.
pub fn decide(input: &RelayInput, jitter: &mut impl FnMut(u32, u32) -> u32) -> RelayDecision {
    let ttl_cap = input.ttl.min(MESSAGE_TTL_DEFAULT);

    // 1. REQUEST_SYNC is link-local: never relayed, even when a peer crafts one
    //    with TTL headroom to turn every reachable node into a full-store
    //    responder. This check precedes everything, including the self checks.
    if input.is_request_sync {
        return RelayDecision {
            should_relay: false,
            new_ttl: ttl_cap,
            delay_ms: 0,
        };
    }

    // 2. Obvious non-relays.
    if ttl_cap <= 1 || input.sender_is_self || input.recipient_is_self {
        return RelayDecision {
            should_relay: false,
            new_ttl: ttl_cap,
            delay_ms: 0,
        };
    }

    // 3. Session-critical and directed traffic: deterministic and reliable, with
    //    no TTL cap beyond the initial one. Tighter jitter for handshakes so
    //    multi-hop sessions come up fast.
    if input.is_handshake || input.is_directed_fragment || input.is_directed_encrypted {
        let delay_ms = if input.is_handshake {
            jitter(10, 35)
        } else {
            jitter(20, 60)
        };
        return RelayDecision {
            should_relay: true,
            new_ttl: ttl_cap.saturating_sub(1),
            delay_ms,
        };
    }

    // 4. Live voice floods with the fragment policy: the dense clamp contains a
    //    sustained ~15 pkt/s per-talker stream, and the tight jitter window keeps
    //    per-hop latency inside the receiver's ~350 ms jitter buffer.
    if input.is_fragment || input.is_voice_frame {
        let fragment_cap = if input.degree >= input.high_degree_threshold {
            FRAGMENT_RELAY_TTL_CAP_DENSE
        } else {
            FRAGMENT_RELAY_TTL_CAP
        };
        let ttl_limit = ttl_cap.min(fragment_cap);
        if ttl_limit <= 1 {
            return RelayDecision {
                should_relay: false,
                new_ttl: ttl_limit,
                delay_ms: 0,
            };
        }
        return RelayDecision {
            should_relay: true,
            new_ttl: ttl_limit - 1,
            delay_ms: jitter(FRAGMENT_RELAY_MIN_DELAY_MS, FRAGMENT_RELAY_MAX_DELAY_MS),
        };
    }

    // 5. Broadcast clamping.
    //    - Dense graphs stay low but still bridge multiple hops.
    //    - Thin chains (degree <= 2) relay at full incoming depth: every hop
    //      counts there and the flood costs almost nothing.
    //    - Announces and urgent board posts get one extra hop of headroom.
    let ttl_limit = if input.degree >= input.high_degree_threshold {
        ttl_cap.clamp(2, 5)
    } else if input.degree <= 2 {
        ttl_cap
    } else {
        let preferred = if input.is_announce || input.is_urgent_board_post {
            7
        } else {
            6
        };
        ttl_cap.min(preferred).max(2)
    };

    // Wider jitter as the graph thickens, so duplicate suppression wins more
    // often; sparse graphs relay quickly to avoid cancellation races.
    let delay_ms = match input.degree {
        0..=2 => jitter(10, 40),
        3..=5 => jitter(60, 150),
        6..=9 => jitter(80, 180),
        _ => jitter(100, 220),
    };

    RelayDecision {
        should_relay: true,
        // ttl_limit is >= 2 in every arm above (ttl_cap <= 1 already returned),
        // so this never underflows.
        new_ttl: ttl_limit.saturating_sub(1),
        delay_ms,
    }
}

/// TLV type byte for a board post's flags field (`BoardTLVType.flags`).
const BOARD_TLV_FLAGS: u8 = 0x09;
/// `BoardPostPacket.urgentFlag`.
const BOARD_URGENT_FLAG: u8 = 0x01;

/// Cheap TLV peek for the relay hot path: is this board payload urgent?
///
/// Ported from `BoardWire.urgentFlag(in:)`. Deliberately not a full decode —
/// it walks 1-byte type / 2-byte big-endian length records and stops at the
/// first `flags` field. Any malformation answers "not urgent".
fn board_post_is_urgent(payload: &[u8]) -> bool {
    let mut offset = 0usize;
    while let Some(&[tag, len_hi, len_lo]) = payload.get(offset..offset + 3) {
        let len = ((len_hi as usize) << 8) | len_lo as usize;
        offset += 3;
        let Some(end) = offset.checked_add(len) else {
            return false;
        };
        if end > payload.len() {
            return false;
        }
        if tag == BOARD_TLV_FLAGS && len == 1 {
            return payload
                .get(offset)
                .is_some_and(|flags| flags & BOARD_URGENT_FLAG != 0);
        }
        offset = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{BROADCAST_RECIPIENT, SENDER_ID_LEN};

    /// Jitter stub returning the low bound and recording every window asked for.
    fn recording_jitter(log: &mut Vec<(u32, u32)>) -> impl FnMut(u32, u32) -> u32 + '_ {
        move |lo, hi| {
            log.push((lo, hi));
            lo
        }
    }

    fn lo_jitter(lo: u32, _hi: u32) -> u32 {
        lo
    }

    fn hi_jitter(_lo: u32, hi: u32) -> u32 {
        hi
    }

    fn base_input() -> RelayInput {
        RelayInput {
            ttl: 7,
            sender_is_self: false,
            recipient_is_self: false,
            is_directed_encrypted: false,
            is_fragment: false,
            is_directed_fragment: false,
            is_handshake: false,
            is_announce: false,
            is_request_sync: false,
            is_urgent_board_post: false,
            is_voice_frame: false,
            degree: 4,
            high_degree_threshold: HIGH_DEGREE_THRESHOLD,
        }
    }

    fn packet(msg_type: MessageType, recipient: Option<[u8; SENDER_ID_LEN]>) -> BitchatPacket {
        BitchatPacket::new(msg_type, [0xAA; SENDER_ID_LEN], recipient, 1, vec![], 7)
    }

    // --- branch 1: request sync -------------------------------------------

    #[test]
    fn request_sync_never_relays_however_much_ttl_it_claims() {
        for ttl in [0u8, 1, 2, 7, 255] {
            let input = RelayInput {
                ttl,
                is_request_sync: true,
                ..base_input()
            };
            let decision = decide(&input, &mut lo_jitter);
            assert!(!decision.should_relay, "ttl {ttl}");
            assert_eq!(decision.new_ttl, ttl.min(MESSAGE_TTL_DEFAULT));
            assert_eq!(decision.delay_ms, 0);
        }
    }

    #[test]
    fn request_sync_check_precedes_the_directed_branch() {
        // A crafted REQUEST_SYNC that also looks directed must still not relay.
        let input = RelayInput {
            ttl: 7,
            is_request_sync: true,
            is_directed_encrypted: true,
            is_handshake: true,
            ..base_input()
        };
        assert!(!decide(&input, &mut lo_jitter).should_relay);
    }

    // --- branch 2: suppression --------------------------------------------

    #[test]
    fn ttl_one_is_the_boundary_between_death_and_relay() {
        let one = RelayInput {
            ttl: 1,
            ..base_input()
        };
        let decision = decide(&one, &mut lo_jitter);
        assert!(!decision.should_relay);
        assert_eq!(decision.new_ttl, 1);
        assert_eq!(decision.delay_ms, 0);

        let two = RelayInput {
            ttl: 2,
            ..base_input()
        };
        let decision = decide(&two, &mut lo_jitter);
        assert!(decision.should_relay);
        // degree 4: preferred 6, ttl_cap 2 -> limit max(2, min(2, 6)) = 2.
        assert_eq!(decision.new_ttl, 1);
        assert_eq!(decision.delay_ms, 60);
    }

    #[test]
    fn ttl_zero_does_not_relay() {
        let input = RelayInput {
            ttl: 0,
            ..base_input()
        };
        let decision = decide(&input, &mut lo_jitter);
        assert!(!decision.should_relay);
        assert_eq!(decision.new_ttl, 0);
    }

    #[test]
    fn own_packets_and_packets_for_us_do_not_relay() {
        let mine = RelayInput {
            sender_is_self: true,
            ..base_input()
        };
        assert!(!decide(&mine, &mut lo_jitter).should_relay);

        let ours = RelayInput {
            recipient_is_self: true,
            ..base_input()
        };
        assert!(!decide(&ours, &mut lo_jitter).should_relay);

        // Even for otherwise always-relayed directed traffic.
        let directed_to_us = RelayInput {
            recipient_is_self: true,
            is_directed_encrypted: true,
            ..base_input()
        };
        assert!(!decide(&directed_to_us, &mut lo_jitter).should_relay);
    }

    // --- branch 3: handshake / directed ------------------------------------

    #[test]
    fn handshake_relays_uncapped_with_the_tight_window() {
        let mut log = Vec::new();
        let input = RelayInput {
            ttl: 7,
            is_handshake: true,
            degree: 12,
            ..base_input()
        };
        let decision = decide(&input, &mut recording_jitter(&mut log));
        assert!(decision.should_relay);
        // No TTL cap beyond the initial min(ttl, 7): dense degree is ignored.
        assert_eq!(decision.new_ttl, 6);
        assert_eq!(decision.delay_ms, 10);
        assert_eq!(log, vec![(10, 35)]);
    }

    #[test]
    fn directed_encrypted_and_directed_fragment_share_the_wider_window() {
        for input in [
            RelayInput {
                is_directed_encrypted: true,
                ..base_input()
            },
            RelayInput {
                is_fragment: true,
                is_directed_fragment: true,
                ..base_input()
            },
        ] {
            let mut log = Vec::new();
            let decision = decide(&input, &mut recording_jitter(&mut log));
            assert!(decision.should_relay);
            assert_eq!(decision.new_ttl, 6);
            assert_eq!(log, vec![(20, 60)]);
            assert_eq!(decision.delay_ms, 20);
        }
    }

    #[test]
    fn directed_fragment_outranks_the_fragment_clamp() {
        // Dense graph + fragment would clamp to 5; the directed branch runs first.
        let input = RelayInput {
            ttl: 7,
            is_fragment: true,
            is_directed_fragment: true,
            degree: 20,
            ..base_input()
        };
        let decision = decide(&input, &mut hi_jitter);
        assert_eq!(decision.new_ttl, 6);
        assert_eq!(decision.delay_ms, 60);
    }

    #[test]
    fn incoming_ttl_above_the_default_is_capped_first() {
        let input = RelayInput {
            ttl: 200,
            is_handshake: true,
            ..base_input()
        };
        assert_eq!(decide(&input, &mut lo_jitter).new_ttl, 6);
    }

    // --- branch 4: fragments and voice -------------------------------------

    #[test]
    fn fragment_cap_switches_exactly_at_the_threshold() {
        let sparse = RelayInput {
            ttl: 7,
            is_fragment: true,
            degree: HIGH_DEGREE_THRESHOLD - 1,
            ..base_input()
        };
        let mut log = Vec::new();
        let decision = decide(&sparse, &mut recording_jitter(&mut log));
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 6); // min(7, 7) - 1
        assert_eq!(log, vec![(8, 25)]);
        assert_eq!(decision.delay_ms, 8);

        let dense = RelayInput {
            degree: HIGH_DEGREE_THRESHOLD,
            ..sparse
        };
        let decision = decide(&dense, &mut hi_jitter);
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 4); // min(7, 5) - 1
        assert_eq!(decision.delay_ms, 25);
    }

    #[test]
    fn voice_frames_follow_the_fragment_policy() {
        let input = RelayInput {
            ttl: 7,
            is_voice_frame: true,
            degree: HIGH_DEGREE_THRESHOLD,
            ..base_input()
        };
        let decision = decide(&input, &mut lo_jitter);
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 4);
        assert_eq!(decision.delay_ms, 8);
    }

    #[test]
    fn fragment_with_ttl_two_still_relays_but_ttl_one_does_not() {
        let two = RelayInput {
            ttl: 2,
            is_fragment: true,
            ..base_input()
        };
        let decision = decide(&two, &mut lo_jitter);
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 1);

        // ttl 1 is caught by the suppression branch before the fragment branch.
        let one = RelayInput { ttl: 1, ..two };
        let decision = decide(&one, &mut lo_jitter);
        assert!(!decision.should_relay);
        assert_eq!(decision.new_ttl, 1);
    }

    // --- branch 5: broadcast ------------------------------------------------

    #[test]
    fn dense_broadcast_clamps_to_five() {
        let input = RelayInput {
            ttl: 7,
            degree: HIGH_DEGREE_THRESHOLD,
            ..base_input()
        };
        let mut log = Vec::new();
        let decision = decide(&input, &mut recording_jitter(&mut log));
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 4);
        assert_eq!(log, vec![(80, 180)]);
    }

    #[test]
    fn dense_broadcast_has_a_floor_of_two() {
        let input = RelayInput {
            ttl: 2,
            degree: 30,
            ..base_input()
        };
        let decision = decide(&input, &mut lo_jitter);
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 1);
        assert_eq!(decision.delay_ms, 100);
    }

    #[test]
    fn thin_chains_relay_at_full_incoming_depth() {
        for degree in 0..=2usize {
            let input = RelayInput {
                ttl: 7,
                degree,
                ..base_input()
            };
            let mut log = Vec::new();
            let decision = decide(&input, &mut recording_jitter(&mut log));
            assert!(decision.should_relay, "degree {degree}");
            assert_eq!(decision.new_ttl, 6, "degree {degree}");
            assert_eq!(log, vec![(10, 40)]);
        }

        // degree 3 leaves the thin-chain arm: preferred 6 applies (no clamp at
        // ttl 7 -> 6) and the window widens.
        let input = RelayInput {
            ttl: 7,
            degree: 3,
            ..base_input()
        };
        let mut log = Vec::new();
        let decision = decide(&input, &mut recording_jitter(&mut log));
        assert_eq!(decision.new_ttl, 5);
        assert_eq!(log, vec![(60, 150)]);
    }

    #[test]
    fn announces_and_urgent_board_posts_get_one_more_hop() {
        let plain = RelayInput {
            ttl: 7,
            degree: 4,
            ..base_input()
        };
        assert_eq!(decide(&plain, &mut lo_jitter).new_ttl, 5); // min(7, 6) - 1

        let announce = RelayInput {
            is_announce: true,
            ..plain
        };
        assert_eq!(decide(&announce, &mut lo_jitter).new_ttl, 6); // min(7, 7) - 1

        let urgent = RelayInput {
            is_urgent_board_post: true,
            ..plain
        };
        assert_eq!(decide(&urgent, &mut lo_jitter).new_ttl, 6);

        // The headroom does not survive a dense graph.
        let dense_announce = RelayInput {
            degree: HIGH_DEGREE_THRESHOLD,
            ..announce
        };
        assert_eq!(decide(&dense_announce, &mut lo_jitter).new_ttl, 4);
    }

    #[test]
    fn every_broadcast_jitter_window_is_selected_by_degree() {
        let expected = [
            (0usize, (10u32, 40u32)),
            (2, (10, 40)),
            (3, (60, 150)),
            (5, (60, 150)),
            (6, (80, 180)),
            (9, (80, 180)),
            (10, (100, 220)),
            (99, (100, 220)),
        ];
        for (degree, window) in expected {
            let input = RelayInput {
                ttl: 7,
                degree,
                // Keep the dense clamp out of the way so only the jitter switch
                // is under test.
                high_degree_threshold: usize::MAX,
                ..base_input()
            };
            let mut log = Vec::new();
            let decision = decide(&input, &mut recording_jitter(&mut log));
            assert_eq!(log, vec![window], "degree {degree}");
            assert_eq!(decision.delay_ms, window.0);
        }
    }

    // --- from_packet mapping ------------------------------------------------

    #[test]
    fn directed_encrypted_covers_five_types_and_needs_a_recipient() {
        let directed = [
            MessageType::NoiseEncrypted,
            MessageType::CourierEnvelope,
            MessageType::Ping,
            MessageType::Pong,
            MessageType::NostrCarrier,
        ];
        for msg_type in directed {
            let with = RelayInput::from_packet(
                &packet(msg_type, Some([0x11; SENDER_ID_LEN])),
                false,
                false,
                4,
                HIGH_DEGREE_THRESHOLD,
            );
            assert!(with.is_directed_encrypted, "{msg_type:?} with recipient");

            let without =
                RelayInput::from_packet(&packet(msg_type, None), false, false, 4, HIGH_DEGREE_THRESHOLD);
            assert!(!without.is_directed_encrypted, "{msg_type:?} without recipient");
        }

        // Types outside that set never take the directed path.
        for msg_type in [
            MessageType::Message,
            MessageType::Announce,
            MessageType::GroupMessage,
            MessageType::FileTransfer,
        ] {
            let input = RelayInput::from_packet(
                &packet(msg_type, Some([0x11; SENDER_ID_LEN])),
                false,
                false,
                4,
                HIGH_DEGREE_THRESHOLD,
            );
            assert!(!input.is_directed_encrypted, "{msg_type:?}");
        }
    }

    #[test]
    fn fragment_flags_track_the_recipient_field() {
        let broadcast_stream =
            RelayInput::from_packet(&packet(MessageType::Fragment, None), false, false, 4, 6);
        assert!(broadcast_stream.is_fragment);
        assert!(!broadcast_stream.is_directed_fragment);

        let directed = RelayInput::from_packet(
            &packet(MessageType::Fragment, Some([0x11; SENDER_ID_LEN])),
            false,
            false,
            4,
            6,
        );
        assert!(directed.is_fragment);
        assert!(directed.is_directed_fragment);

        // Ported quirk: the explicit broadcast recipient still counts as directed.
        let explicit_broadcast = RelayInput::from_packet(
            &packet(MessageType::Fragment, Some(BROADCAST_RECIPIENT)),
            false,
            false,
            4,
            6,
        );
        assert!(explicit_broadcast.is_directed_fragment);
    }

    #[test]
    fn simple_type_flags_map_one_to_one() {
        let cases = [
            (MessageType::NoiseHandshake, "handshake"),
            (MessageType::Announce, "announce"),
            (MessageType::RequestSync, "request_sync"),
            (MessageType::VoiceFrame, "voice"),
        ];
        for (msg_type, which) in cases {
            let input = RelayInput::from_packet(&packet(msg_type, None), false, false, 4, 6);
            let flag = match which {
                "handshake" => input.is_handshake,
                "announce" => input.is_announce,
                "request_sync" => input.is_request_sync,
                _ => input.is_voice_frame,
            };
            assert!(flag, "{which}");
        }
    }

    #[test]
    fn unknown_message_types_take_the_broadcast_path() {
        let mut unknown = packet(MessageType::Message, None);
        unknown.msg_type = 0xEE;
        let input = RelayInput::from_packet(&unknown, false, false, 1, HIGH_DEGREE_THRESHOLD);
        assert!(!input.is_handshake);
        assert!(!input.is_directed_encrypted);
        assert!(!input.is_fragment);

        let decision = decide(&input, &mut lo_jitter);
        assert!(decision.should_relay);
        assert_eq!(decision.new_ttl, 6);
        assert_eq!(decision.delay_ms, 10);
    }

    #[test]
    fn self_flags_are_passed_through() {
        let input = RelayInput::from_packet(
            &packet(MessageType::Message, None),
            true,
            true,
            9,
            HIGH_DEGREE_THRESHOLD,
        );
        assert!(input.sender_is_self);
        assert!(input.recipient_is_self);
        assert_eq!(input.degree, 9);
        assert_eq!(input.high_degree_threshold, HIGH_DEGREE_THRESHOLD);
        assert_eq!(input.ttl, 7);
    }

    // --- board urgency peek --------------------------------------------------

    fn board_tlv(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = value.len() as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn urgent_flag_is_found_after_earlier_tlvs() {
        let mut payload = board_tlv(0x01, &[0x01]);
        payload.extend(board_tlv(0x02, &[0xAB; 16]));
        payload.extend(board_tlv(0x09, &[0x01]));
        assert!(board_post_is_urgent(&payload));

        let mut not_urgent = board_tlv(0x01, &[0x01]);
        not_urgent.extend(board_tlv(0x09, &[0x00]));
        assert!(!board_post_is_urgent(&not_urgent));
    }

    #[test]
    fn urgent_peek_is_total_on_malformed_payloads() {
        assert!(!board_post_is_urgent(&[]));
        assert!(!board_post_is_urgent(&[0x09]));
        assert!(!board_post_is_urgent(&[0x09, 0x00]));
        // Length runs off the end.
        assert!(!board_post_is_urgent(&[0x09, 0xFF, 0xFF, 0x01]));
        // Wrong length for a flags field: skipped, not misread.
        let mut wide = board_tlv(0x09, &[0x01, 0x01]);
        wide.extend(board_tlv(0x09, &[0x01]));
        assert!(board_post_is_urgent(&wide));
        assert!(!board_post_is_urgent(&board_tlv(0x09, &[0x01, 0x01])));
    }

    #[test]
    fn urgent_board_post_reaches_the_relay_input() {
        let mut post = packet(MessageType::BoardPost, None);
        post.payload = board_tlv(0x09, &[0x01]);
        let input = RelayInput::from_packet(&post, false, false, 4, HIGH_DEGREE_THRESHOLD);
        assert!(input.is_urgent_board_post);
        assert_eq!(decide(&input, &mut lo_jitter).new_ttl, 6);

        post.payload = board_tlv(0x09, &[0x00]);
        let input = RelayInput::from_packet(&post, false, false, 4, HIGH_DEGREE_THRESHOLD);
        assert!(!input.is_urgent_board_post);
        assert_eq!(decide(&input, &mut lo_jitter).new_ttl, 5);
    }
}
