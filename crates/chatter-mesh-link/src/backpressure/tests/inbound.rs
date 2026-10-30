use super::*;

#[test]
fn chunked_writes_reassemble_into_a_packet() {
    let mut assembler = InboundAssembler::new();
    let limits = InboundLimits {
        cap_bytes: 1024,
        stall_reset_ms: 250,
    };
    let l = link("central-1");
    let bytes = frame(0x0102030405);
    let split = (bytes.len() / 2).max(1);

    let first = assembler.append(
        &l,
        &[InboundChunk {
            offset: 0,
            bytes: bytes[..split].to_vec(),
        }],
        limits,
        0,
    );
    match first {
        InboundAppend::Waiting { metadata } => {
            assert_eq!(metadata.accumulated_bytes, split);
            assert_eq!(metadata.appended_bytes, split);
            assert_eq!(metadata.offsets, vec![0]);
            assert_eq!(metadata.packet_type, Some(MessageType::Message.as_u8()));
            assert_eq!(metadata.stall_discarded_bytes, 0);
        }
        other => panic!("expected a partial frame to wait, got {other:?}"),
    }
    assert_eq!(assembler.buffered_bytes(&l), split);

    let second = assembler.append(
        &l,
        &[InboundChunk {
            offset: split,
            bytes: bytes[split..].to_vec(),
        }],
        limits,
        10,
    );
    match second {
        InboundAppend::Decoded { packet, metadata } => {
            assert_eq!(packet.timestamp, 0x0102030405);
            assert_eq!(packet.payload, vec![0xDE, 0xAD, 0xBE, 0xEF]);
            assert_eq!(metadata.accumulated_bytes, bytes.len());
            assert_eq!(metadata.offsets, vec![split]);
        }
        other => panic!("expected the completed frame to decode, got {other:?}"),
    }
    // Decoding releases the buffer.
    assert!(assembler.is_empty());
}

#[test]
fn multiple_offset_chunks_merge_in_one_call() {
    let mut assembler = InboundAssembler::new();
    let limits = InboundLimits {
        cap_bytes: 1024,
        stall_reset_ms: 250,
    };
    let bytes = frame(0xABC);
    let split = (bytes.len() / 3).max(1);

    // Out of order on purpose: GATT prepared writes carry offsets precisely
    // because they are not guaranteed to arrive in sequence.
    let result = assembler.append(
        &link("central-1"),
        &[
            InboundChunk {
                offset: split,
                bytes: bytes[split..].to_vec(),
            },
            InboundChunk {
                offset: 0,
                bytes: bytes[..split].to_vec(),
            },
        ],
        limits,
        0,
    );

    match result {
        InboundAppend::Decoded { packet, metadata } => {
            assert_eq!(packet.timestamp, 0xABC);
            assert_eq!(metadata.appended_bytes, bytes.len());
            assert_eq!(metadata.offsets, vec![split, 0]);
        }
        other => panic!("expected merged chunks to decode, got {other:?}"),
    }
}

#[test]
fn inbound_cap_admits_exactly_at_the_boundary_and_rejects_one_past_it() {
    let mut assembler = InboundAssembler::new();
    let l = link("central-1");
    let limits = InboundLimits {
        cap_bytes: 8,
        stall_reset_ms: 250,
    };

    let at_cap = assembler.append(
        &l,
        &[InboundChunk {
            offset: 0,
            bytes: vec![0xAA; 8],
        }],
        limits,
        0,
    );
    assert!(matches!(at_cap, InboundAppend::Waiting { .. }));
    assert_eq!(assembler.buffered_bytes(&l), 8);

    let past_cap = assembler.append(
        &l,
        &[InboundChunk {
            offset: 8,
            bytes: vec![0xBB; 1],
        }],
        limits,
        1,
    );
    match past_cap {
        InboundAppend::Oversized { metadata } => {
            assert_eq!(metadata.accumulated_bytes, 9);
            assert_eq!(metadata.appended_bytes, 1);
        }
        other => panic!("expected the cap to reject, got {other:?}"),
    }
    // The partial frame is released rather than pinned at the cap.
    assert!(assembler.is_empty());
}

#[test]
fn an_oversized_write_is_dropped_and_a_later_clean_frame_still_decodes() {
    let mut assembler = InboundAssembler::new();
    let l = link("central-1");

    let oversized = assembler.append(
        &l,
        &[InboundChunk {
            offset: 0,
            bytes: vec![0xAA; 8],
        }],
        InboundLimits {
            cap_bytes: 4,
            stall_reset_ms: 250,
        },
        0,
    );
    match oversized {
        InboundAppend::Oversized { metadata } => {
            assert_eq!(metadata.accumulated_bytes, 8);
            assert_eq!(metadata.appended_bytes, 8);
        }
        other => panic!("expected an oversized rejection, got {other:?}"),
    }

    let bytes = frame(0xDEF);
    let decoded = assembler.append(
        &l,
        &[InboundChunk {
            offset: 0,
            bytes: bytes.clone(),
        }],
        InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        },
        1,
    );
    match decoded {
        InboundAppend::Decoded { packet, .. } => assert_eq!(packet.timestamp, 0xDEF),
        other => panic!("expected the clean frame to decode, got {other:?}"),
    }
}

#[test]
fn a_hostile_offset_cannot_force_an_allocation() {
    let mut assembler = InboundAssembler::new();
    let l = link("hostile");
    let limits = InboundLimits::default();

    // bitchat zero-fills to `offset + len` and only then checks the cap, so a
    // 4-byte write at this offset would ask for terabytes. The bound is checked
    // before anything is resized.
    let huge = assembler.append(
        &l,
        &[InboundChunk {
            offset: 1 << 40,
            bytes: vec![0xAA; 4],
        }],
        limits,
        0,
    );
    assert!(matches!(huge, InboundAppend::Oversized { .. }));
    assert!(assembler.is_empty());

    // And an offset that would overflow the address space at all.
    let overflowing = assembler.append(
        &l,
        &[InboundChunk {
            offset: usize::MAX,
            bytes: vec![0xAA; 4],
        }],
        limits,
        0,
    );
    assert!(matches!(overflowing, InboundAppend::Oversized { .. }));
    assert!(assembler.is_empty());
    assert_eq!(assembler.total_buffered_bytes(), 0);
}

#[test]
fn a_gap_between_chunks_is_zero_filled_and_simply_never_parses() {
    let mut assembler = InboundAssembler::new();
    let l = link("central-1");
    let limits = InboundLimits {
        cap_bytes: 1024,
        stall_reset_ms: 250,
    };

    let result = assembler.append(
        &l,
        &[InboundChunk {
            offset: 64,
            bytes: vec![0xAA; 4],
        }],
        limits,
        0,
    );
    match result {
        InboundAppend::Waiting { metadata } => {
            assert_eq!(metadata.accumulated_bytes, 68);
            assert_eq!(metadata.appended_bytes, 4);
            assert_eq!(metadata.packet_type, Some(0));
        }
        other => panic!("expected a hole to keep waiting, got {other:?}"),
    }
    assert_eq!(assembler.buffered_bytes(&l), 68);
}

#[test]
fn a_partial_frame_is_reset_after_the_stall_window() {
    let mut assembler = InboundAssembler::new();
    let l = link("central-1");
    let limits = InboundLimits {
        cap_bytes: 1024,
        stall_reset_ms: 250,
    };
    let bytes = frame(0x11);
    let split = (bytes.len() / 2).max(1);

    let _ = assembler.append(
        &l,
        &[InboundChunk {
            offset: 0,
            bytes: bytes[..split].to_vec(),
        }],
        limits,
        1_000,
    );
    assert_eq!(assembler.buffered_bytes(&l), split);

    // One millisecond short of the window: the old bytes are still there, so the
    // completing tail decodes.
    let mut early = assembler.clone();
    let completed = early.append(
        &l,
        &[InboundChunk {
            offset: split,
            bytes: bytes[split..].to_vec(),
        }],
        limits,
        1_000 + limits.stall_reset_ms - 1,
    );
    match completed {
        InboundAppend::Decoded { metadata, .. } => {
            assert_eq!(metadata.stall_discarded_bytes, 0);
        }
        other => panic!("expected the frame to still complete, got {other:?}"),
    }

    // Exactly at the window: the stalled prefix is discarded first, so the same
    // tail lands on an empty buffer and cannot decode.
    let late = assembler.append(
        &l,
        &[InboundChunk {
            offset: split,
            bytes: bytes[split..].to_vec(),
        }],
        limits,
        1_000 + limits.stall_reset_ms,
    );
    match late {
        InboundAppend::Waiting { metadata } => {
            assert_eq!(metadata.stall_discarded_bytes, split);
            // Reassembly restarted: only the tail, written at its own offset.
            assert_eq!(metadata.accumulated_bytes, bytes.len());
        }
        other => panic!("expected the stalled prefix to be discarded, got {other:?}"),
    }
}

#[test]
fn reap_stalled_reclaims_partials_from_peers_that_never_come_back() {
    let mut assembler = InboundAssembler::new();
    let limits = InboundLimits {
        cap_bytes: 1024,
        stall_reset_ms: 250,
    };

    // Three peers open a frame and go silent; one keeps writing.
    for i in 0..3 {
        let _ = assembler.append(
            &link(&format!("silent-{i}")),
            &[InboundChunk {
                offset: 0,
                bytes: vec![0xAA; 100],
            }],
            limits,
            1_000,
        );
    }
    let chatty = link("chatty");
    let _ = assembler.append(
        &chatty,
        &[InboundChunk {
            offset: 0,
            bytes: vec![0xBB; 100],
        }],
        limits,
        1_200,
    );

    assert_eq!(assembler.link_count(), 4);
    assert_eq!(assembler.total_buffered_bytes(), 400);

    let reaped = assembler.reap_stalled(limits, 1_300);
    assert_eq!(reaped.len(), 3);
    for entry in &reaped {
        assert_eq!(entry.discarded_bytes, 100);
        assert_eq!(entry.idle_ms, 300);
        assert!(entry.link.as_str().starts_with("silent-"));
    }
    assert_eq!(assembler.link_count(), 1);
    assert_eq!(assembler.buffered_bytes(&chatty), 100);

    // Nothing to reclaim twice.
    assert!(assembler.reap_stalled(limits, 1_300).is_empty());
}

#[test]
fn discard_and_clear_release_partial_writes() {
    let mut assembler = InboundAssembler::new();
    let limits = InboundLimits {
        cap_bytes: 1024,
        stall_reset_ms: 250,
    };
    let a = link("a");
    let b = link("b");
    let bytes = frame(0x22);
    let split = (bytes.len() / 2).max(1);

    for l in [&a, &b] {
        let _ = assembler.append(
            l,
            &[InboundChunk {
                offset: 0,
                bytes: bytes[..split].to_vec(),
            }],
            limits,
            0,
        );
    }

    assert_eq!(assembler.discard(&a), split);
    assert_eq!(assembler.link_count(), 1);

    // The earlier prefix is gone, so the tail alone cannot decode.
    let result = assembler.append(
        &a,
        &[InboundChunk {
            offset: split,
            bytes: bytes[split..].to_vec(),
        }],
        limits,
        1,
    );
    assert!(matches!(result, InboundAppend::Waiting { .. }));

    assembler.clear();
    assert!(assembler.is_empty());
    assert_eq!(assembler.total_buffered_bytes(), 0);
}

#[test]
fn empty_chunks_are_ignored_and_leave_no_entry() {
    let mut assembler = InboundAssembler::new();
    let l = link("central-1");
    let result = assembler.append(
        &l,
        &[InboundChunk {
            offset: 0,
            bytes: Vec::new(),
        }],
        InboundLimits::default(),
        0,
    );
    match result {
        InboundAppend::Waiting { metadata } => {
            assert_eq!(metadata.appended_bytes, 0);
            assert!(metadata.offsets.is_empty());
            assert_eq!(metadata.packet_type, None);
        }
        other => panic!("expected an empty write to be a no-op, got {other:?}"),
    }
    assert!(assembler.is_empty());
}

#[test]
fn inbound_memory_stays_bounded_under_sustained_partial_writes() {
    let mut assembler = InboundAssembler::new();
    let limits = InboundLimits {
        cap_bytes: 4096,
        stall_reset_ms: 250,
    };

    // 200 peers, each dribbling partial frames forever and never completing one.
    // Per-link memory is bounded by the cap; the link count is bounded by the
    // connection budget, and reaping bounds it further.
    let mut now = 0u64;
    for round in 0..50u64 {
        for peer in 0..200u32 {
            let _ = assembler.append(
                &link(&format!("peer-{peer}")),
                &[InboundChunk {
                    offset: (round as usize * 64) % 2048,
                    bytes: vec![0xAA; 64],
                }],
                limits,
                now,
            );
        }
        now += 10;
        assert!(
            assembler.total_buffered_bytes() <= 200 * limits.cap_bytes,
            "inbound memory grew past 200 capped links in round {round}"
        );
    }

    assert_eq!(assembler.link_count(), 200);
    for peer in 0..200u32 {
        assert!(assembler.buffered_bytes(&link(&format!("peer-{peer}"))) <= limits.cap_bytes);
    }

    // Everyone goes quiet: the sweep gives all of it back.
    let reaped = assembler.reap_stalled(limits, now + limits.stall_reset_ms);
    assert_eq!(reaped.len(), 200);
    assert_eq!(assembler.total_buffered_bytes(), 0);
    assert!(assembler.is_empty());
}
