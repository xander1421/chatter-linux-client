use super::*;

#[test]
fn writes_leave_in_priority_order() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");

    let _ = buffer.enqueue(&l, vec![0x01; 4], WritePriority::FILE_TRANSFER, 64);
    let _ = buffer.enqueue(&l, vec![0x02; 4], WritePriority::HIGH, 64);
    let _ = buffer.enqueue(&l, vec![0x03; 4], WritePriority::fragment(2), 64);

    let taken = buffer.take_all(&l);
    let firsts: Vec<u8> = taken
        .iter()
        .filter_map(|w| w.bytes.first().copied())
        .collect();
    assert_eq!(firsts, vec![0x02, 0x03, 0x01]);
    assert!(buffer.is_empty());
}

#[test]
fn equal_priorities_keep_fifo_order() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");
    for tag in 1u8..=4 {
        let _ = buffer.enqueue(&l, vec![tag; 2], WritePriority::fragment(8), 64);
    }
    let taken = buffer.take_all(&l);
    let firsts: Vec<u8> = taken
        .iter()
        .filter_map(|w| w.bytes.first().copied())
        .collect();
    assert_eq!(firsts, vec![1, 2, 3, 4]);
}

#[test]
fn smaller_transfers_outrank_larger_ones() {
    assert!(WritePriority::HIGH < WritePriority::fragment(2));
    assert!(WritePriority::fragment(2) < WritePriority::fragment(500));
    assert!(WritePriority::fragment(500) < WritePriority::FILE_TRANSFER);
    assert!(WritePriority::FILE_TRANSFER < WritePriority::LOW);
}

#[test]
fn fragment_priority_clamps_absurd_totals() {
    assert_eq!(WritePriority::fragment(0), WritePriority::fragment(1));
    assert_eq!(
        WritePriority::fragment(usize::MAX),
        WritePriority::fragment(u16::MAX as usize)
    );
    // Even a clamped fragment must still outrank bulk traffic.
    assert!(WritePriority::fragment(usize::MAX) < WritePriority::FILE_TRANSFER);
}

#[test]
fn byte_cap_admits_exactly_at_the_boundary_and_rejects_one_past_it() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");

    let at_cap = buffer.enqueue(&l, vec![0xAA; 16], WritePriority::HIGH, 16);
    assert_eq!(
        at_cap,
        WriteEnqueue::Enqueued {
            trimmed_bytes: 0,
            remaining_bytes: 16
        }
    );
    assert_eq!(buffer.queued_bytes(&l), 16);

    let mut fresh = OutboundWriteBuffer::new();
    let past_cap = fresh.enqueue(&l, vec![0xAA; 17], WritePriority::HIGH, 16);
    assert_eq!(past_cap, WriteEnqueue::Oversized { offered_bytes: 17 });
    // Nothing was allocated for the link at all.
    assert!(fresh.is_empty());
    assert_eq!(fresh.queued_bytes(&l), 0);
}

#[test]
fn trimming_evicts_the_lowest_priority_tail_first() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");

    let _ = buffer.enqueue(&l, vec![0x01; 8], WritePriority::LOW, 16);
    let _ = buffer.enqueue(&l, vec![0x02; 8], WritePriority::FILE_TRANSFER, 16);
    let result = buffer.enqueue(&l, vec![0x03; 8], WritePriority::HIGH, 16);

    assert_eq!(
        result,
        WriteEnqueue::Enqueued {
            trimmed_bytes: 8,
            remaining_bytes: 16
        }
    );
    let taken = buffer.take_all(&l);
    let firsts: Vec<u8> = taken
        .iter()
        .filter_map(|w| w.bytes.first().copied())
        .collect();
    assert_eq!(firsts, vec![0x03, 0x02]);
}

#[test]
fn rejection_of_the_offered_write_is_reported_not_swallowed() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");
    let _ = buffer.enqueue(&l, vec![0x01; 8], WritePriority::HIGH, 8);

    // The queue is exactly full of higher-priority traffic, so this write sorts
    // to the tail and is trimmed straight back off. `Enqueued` alone would be a
    // lie here; `accepted` is what a courier commit reads.
    let admission = buffer.enqueue_reporting_acceptance(&l, vec![0x02; 8], WritePriority::LOW, 8);
    assert!(!admission.accepted);
    assert_eq!(
        admission.result,
        WriteEnqueue::Enqueued {
            trimmed_bytes: 8,
            remaining_bytes: 8
        }
    );

    let taken = buffer.take_all(&l);
    let firsts: Vec<u8> = taken
        .iter()
        .filter_map(|w| w.bytes.first().copied())
        .collect();
    assert_eq!(firsts, vec![0x01]);
}

#[test]
fn oversized_write_is_reported_as_not_accepted() {
    let mut buffer = OutboundWriteBuffer::new();
    let admission = buffer.enqueue_reporting_acceptance(
        &link("peer-1"),
        vec![0x01; 32],
        WritePriority::HIGH,
        16,
    );
    assert!(!admission.accepted);
    assert_eq!(
        admission.result,
        WriteEnqueue::Oversized { offered_bytes: 32 }
    );
}

#[test]
fn a_surviving_write_reports_accepted() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");
    let _ = buffer.enqueue(&l, vec![0x01; 8], WritePriority::LOW, 8);

    let admission = buffer.enqueue_reporting_acceptance(&l, vec![0x02; 8], WritePriority::HIGH, 8);
    assert!(admission.accepted);
    let taken = buffer.take_all(&l);
    let firsts: Vec<u8> = taken
        .iter()
        .filter_map(|w| w.bytes.first().copied())
        .collect();
    assert_eq!(firsts, vec![0x02]);
}

#[test]
fn discard_all_removes_only_the_named_link() {
    let mut buffer = OutboundWriteBuffer::new();
    let gone = link("gone");
    let live = link("live");
    let _ = buffer.enqueue(&gone, vec![1], WritePriority::HIGH, 100);
    let _ = buffer.enqueue(&live, vec![2], WritePriority::HIGH, 100);

    buffer.discard_all(&gone);

    assert!(buffer.take_all(&gone).is_empty());
    let survivors: Vec<Vec<u8>> = buffer
        .take_all(&live)
        .into_iter()
        .map(|w| w.bytes)
        .collect();
    assert_eq!(survivors, vec![vec![2u8]]);
}

#[test]
fn prepend_restores_undrained_writes_ahead_of_newer_ones() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");
    let _ = buffer.enqueue(&l, vec![0x02], WritePriority::HIGH, 64);

    buffer.prepend(
        &l,
        vec![PendingWrite {
            priority: WritePriority::HIGH,
            bytes: vec![0x01],
        }],
    );

    let taken = buffer.take_all(&l);
    let firsts: Vec<u8> = taken
        .iter()
        .filter_map(|w| w.bytes.first().copied())
        .collect();
    assert_eq!(firsts, vec![0x01, 0x02]);
}

#[test]
fn prepend_of_nothing_leaves_the_queue_untouched() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("peer-1");
    buffer.prepend(&l, Vec::new());
    assert!(buffer.is_empty());
}

#[test]
fn one_link_cannot_grow_past_its_cap_under_sustained_backpressure() {
    let mut buffer = OutboundWriteBuffer::new();
    let l = link("greedy");
    let cap = 64 * 1024;

    // A peer that never drains: 10k writes offered, nothing taken.
    for i in 0..10_000u32 {
        let priority = if i % 3 == 0 {
            WritePriority::HIGH
        } else {
            WritePriority::LOW
        };
        let _ = buffer.enqueue(&l, vec![(i % 251) as u8; 1024], priority, cap);
        assert!(
            buffer.queued_bytes(&l) <= cap,
            "queue exceeded its cap at iteration {i}"
        );
    }

    assert_eq!(buffer.link_count(), 1);
    assert!(buffer.queued_bytes(&l) <= cap);
    assert_eq!(buffer.total_queued_bytes(), buffer.queued_bytes(&l));
}

#[test]
fn rotating_link_identities_do_not_each_mint_a_fresh_cap() {
    let mut buffer = OutboundWriteBuffer::new();
    let cap = 4096;

    // BlueZ hands out a new object path per connection. Without discarding on
    // disconnect, 500 reconnects would hold 500 * cap bytes forever.
    for i in 0..500u32 {
        let l = link(&format!("/org/bluez/hci0/dev_{i}"));
        let _ = buffer.enqueue(&l, vec![0xAB; 4096], WritePriority::HIGH, cap);
        buffer.discard_all(&l);
    }

    assert!(buffer.is_empty());
    assert_eq!(buffer.total_queued_bytes(), 0);
}

#[test]
fn clear_drops_every_link() {
    let mut buffer = OutboundWriteBuffer::new();
    let _ = buffer.enqueue(&link("a"), vec![1], WritePriority::HIGH, 64);
    let _ = buffer.enqueue(&link("b"), vec![2], WritePriority::HIGH, 64);
    assert_eq!(buffer.link_count(), 2);
    buffer.clear();
    assert!(buffer.is_empty());
}

#[test]
fn link_ids_lists_only_links_holding_writes() {
    let mut buffer = OutboundWriteBuffer::new();
    let a = link("a");
    let _ = buffer.enqueue(&a, vec![1], WritePriority::HIGH, 64);
    let ids: Vec<&LinkId> = buffer.link_ids().collect();
    assert_eq!(ids, vec![&a]);

    let _ = buffer.take_all(&a);
    assert_eq!(buffer.link_ids().count(), 0);
}
