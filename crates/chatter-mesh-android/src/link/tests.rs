use super::*;

/// Handles as the Kotlin side mints them: `c:` for a connection we opened,
/// `p:` for one a remote central opened to us, address uppercased. Distinct
/// by construction, which is the contract the module docs spell out — note
/// that `PEER` and `INBOUND` are the *same device* in both directions.
const PEER: &str = "c:AA:BB:CC:DD:EE:FF";
const OTHER: &str = "c:11:22:33:44:55:66";
const INBOUND: &str = "p:AA:BB:CC:DD:EE:FF";

fn table() -> Connections {
    Connections::default()
}

/// A link with a queue small enough to overflow on purpose.
///
/// Deliberately not registered as the active link: these tests exercise the
/// object, and the global registration is process-wide, so racing over it
/// would make them order-dependent.
fn shared(capacity: usize) -> Arc<Shared> {
    Shared::new(capacity)
}

/// Everything currently queued. Needs no runtime — `try_recv` is
/// synchronous, which is what keeps these tests executor-free.
fn drain_events(shared: &Shared) -> Vec<LinkEvent> {
    let mut queue = shared.queue.lock().expect("queue lock");
    let mut events = Vec::new();
    while let Some(event) = queue.0.pop_front() {
        events.push(event);
    }
    events
}

fn block_on<F: core::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

// -- MTU arithmetic ----------------------------------------------------

#[test]
fn max_write_is_the_negotiated_mtu_minus_the_att_header() {
    assert_eq!(max_write_for_mtu(23), 20);
    assert_eq!(max_write_for_mtu(185), 182);
    assert_eq!(max_write_for_mtu(247), 244);
    assert_eq!(max_write_for_mtu(517), 514);
}

#[test]
fn the_default_is_the_spec_minimum_not_an_optimistic_guess() {
    // Until onMtuChanged lands every link is a 23-byte link. Assuming more
    // means silently truncated packets for the whole negotiation window.
    assert_eq!(DEFAULT_MAX_WRITE, 20);
    assert_eq!(LARGEST_MAX_WRITE, 514);
}

#[test]
fn an_impossible_mtu_is_clamped_into_the_spec_range() {
    // Below the spec minimum, 20 always fits, so clamping up is safe.
    assert_eq!(max_write_for_mtu(0), DEFAULT_MAX_WRITE);
    assert_eq!(max_write_for_mtu(i32::MIN), DEFAULT_MAX_WRITE);
    // Above it, believing the number is what loses packets.
    assert_eq!(max_write_for_mtu(4096), LARGEST_MAX_WRITE);
    assert_eq!(max_write_for_mtu(i32::MAX), LARGEST_MAX_WRITE);
}

#[test]
fn a_size_reported_by_kotlin_is_never_believed_beyond_the_spec() {
    assert_eq!(sanitise_max_write(244), 244);
    assert_eq!(sanitise_max_write(20), 20);
    assert_eq!(sanitise_max_write(0), DEFAULT_MAX_WRITE);
    assert_eq!(sanitise_max_write(-1), DEFAULT_MAX_WRITE);
    assert_eq!(sanitise_max_write(i32::MIN), DEFAULT_MAX_WRITE);
    assert_eq!(sanitise_max_write(100_000), LARGEST_MAX_WRITE);
}

// -- the link id table -------------------------------------------------

#[test]
fn link_ids_name_the_role_the_handle_and_a_sequence() {
    let mut conns = table();
    let opened = conns.open(LinkRole::Central, PEER, DEFAULT_MAX_WRITE, true);
    assert!(opened.displaced.is_none());
    assert_eq!(opened.id.as_str(), "c:AA:BB:CC:DD:EE:FF#1");
}

#[test]
fn the_same_peer_in_both_directions_gets_two_distinct_links() {
    // Two devices running the mesh routinely connect to each other at once.
    // Kotlin gives each direction its own handle, which is what keeps them
    // apart here — see the module docs.
    let mut conns = table();
    let outbound = conns.open(LinkRole::Central, PEER, 244, true);
    let inbound = conns.open(LinkRole::Peripheral, INBOUND, 244, true);
    assert!(
        inbound.displaced.is_none(),
        "the inbound link must not displace the outbound one"
    );
    assert_ne!(outbound.id, inbound.id);
    assert_eq!(conns.len(), 2);
}

#[test]
fn a_reestablished_link_never_reuses_an_id() {
    // Reuse would let a late Disconnected from the old incarnation destroy
    // state belonging to the new one.
    let mut conns = table();
    let first = conns.establish(LinkRole::Central, PEER, 244).id;
    assert_eq!(conns.close(PEER).as_ref(), Some(&first));
    let second = conns.establish(LinkRole::Central, PEER, 244).id;
    assert_ne!(first, second);
}

#[test]
fn reusing_a_handle_displaces_the_stale_link() {
    let mut conns = table();
    let first = conns.establish(LinkRole::Peripheral, INBOUND, 244).id;
    let second = conns.establish(LinkRole::Peripheral, INBOUND, 185);
    assert_eq!(second.displaced, Some(first));
    assert_eq!(conns.len(), 1);
    assert_eq!(conns.max_write(&second.id), Some(185));
}

#[test]
fn an_outbound_attempt_is_announced_under_the_id_the_caller_already_holds() {
    // connect() hands the id back before the link is writable. Minting a
    // second id at Connected would leave the caller holding a link that
    // never comes up and never dies.
    let mut conns = table();
    let handed_out = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
    let established = conns.establish(LinkRole::Central, PEER, 244);
    assert_eq!(established.id, handed_out);
    assert!(established.displaced.is_none());
    assert_eq!(conns.len(), 1);
    assert_eq!(conns.max_write(&handed_out), Some(244));
}

#[test]
fn connecting_twice_to_the_same_peer_returns_the_link_that_already_exists() {
    // Kotlin's connect() is idempotent and hands back the same id. Minting a
    // second one here would displace a live link and leave the caller
    // holding an id nothing will ever establish.
    let mut conns = table();
    let first = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
    conns.establish(LinkRole::Central, PEER, 244);

    let again = conns.open_outbound(PEER, DEFAULT_MAX_WRITE);
    assert_eq!(again.id, first);
    assert!(
        again.displaced.is_none(),
        "the live link must survive a second connect"
    );
    assert_eq!(conns.len(), 1);
    assert_eq!(
        conns.max_write(&first),
        Some(244),
        "and must keep its negotiated size"
    );
}

#[test]
fn an_inbound_connection_on_a_handle_awaiting_an_outbound_one_does_not_steal_its_id() {
    // Kotlin's `c:`/`p:` prefixes make this impossible today. If that ever
    // stops being true, the two roles must still not be merged into one
    // link: they have two independent send paths.
    let mut conns = table();
    let outbound = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
    let established = conns.establish(LinkRole::Peripheral, PEER, 244);
    assert_ne!(established.id, outbound);
    assert_eq!(
        established.displaced,
        Some(outbound),
        "the outbound id was visible, so it dies"
    );
}

#[test]
fn a_link_the_caller_never_saw_produces_no_disconnected() {
    let mut conns = table();
    conns.open(LinkRole::Peripheral, INBOUND, 244, false);
    assert_eq!(conns.close(INBOUND), None);
    assert_eq!(conns.len(), 0);
    assert!(conns.drain().is_empty());
}

#[test]
fn an_outbound_attempt_that_never_becomes_writable_still_produces_disconnected() {
    // The whole reason visibility is tracked separately from writability.
    // The caller is holding this id; if it dies silently, the state keyed on
    // it is never released.
    let mut conns = table();
    let id = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
    assert_eq!(conns.close(PEER), Some(id));
}

#[test]
fn closing_a_handle_that_is_not_there_is_harmless() {
    let mut conns = table();
    assert_eq!(conns.close(PEER), None);
}

#[test]
fn inbound_data_before_the_link_is_writable_is_held_not_dropped() {
    // A central may write on the characteristic before it subscribes.
    // Nothing below us retransmits, so those bytes have to survive the gap.
    let mut conns = table();
    assert_eq!(conns.accept_inbound(INBOUND, b"early".to_vec()), None);

    conns.open(LinkRole::Peripheral, INBOUND, 244, false);
    // Still buffered: a link in the table is not a writable link.
    assert_eq!(conns.accept_inbound(INBOUND, b"later".to_vec()), None);

    let established = conns.establish(LinkRole::Peripheral, INBOUND, 244);
    assert_eq!(
        established.flushed,
        vec![b"early".to_vec(), b"later".to_vec()],
        "order must be preserved"
    );

    // From here it flows straight through.
    assert_eq!(
        conns.accept_inbound(INBOUND, b"live".to_vec()),
        Some((established.id, b"live".to_vec()))
    );
}

#[test]
fn data_on_a_visible_but_unwritable_link_is_still_held_back() {
    // The caller holds this id, but has not been told the link is up. A
    // Data event before Connected would name a link it has no state for.
    let mut conns = table();
    conns.open_outbound(PEER, DEFAULT_MAX_WRITE);
    assert_eq!(conns.accept_inbound(PEER, b"early".to_vec()), None);
    assert_eq!(
        conns.establish(LinkRole::Central, PEER, 244).flushed,
        vec![b"early".to_vec()]
    );
}

#[test]
fn the_pre_connected_buffer_is_bounded() {
    // A peer that writes forever without subscribing must not be able to
    // grow this without limit.
    let mut conns = table();
    for n in 0..(PENDING_PACKETS_PER_LINK + 5) {
        conns.accept_inbound(INBOUND, vec![n as u8]);
    }
    let flushed = conns.establish(LinkRole::Peripheral, INBOUND, 244).flushed;
    assert_eq!(flushed.len(), PENDING_PACKETS_PER_LINK);
    // The oldest were dropped, so the newest survive.
    assert_eq!(
        flushed.last(),
        Some(&vec![(PENDING_PACKETS_PER_LINK + 4) as u8])
    );
}

#[test]
fn buffered_data_is_discarded_when_the_connection_goes_away() {
    let mut conns = table();
    conns.accept_inbound(INBOUND, b"orphan".to_vec());
    conns.close(INBOUND);
    assert!(conns
        .establish(LinkRole::Peripheral, INBOUND, 244)
        .flushed
        .is_empty());
}

#[test]
fn buffers_are_kept_per_connection() {
    let mut conns = table();
    conns.accept_inbound(INBOUND, b"inbound".to_vec());
    conns.accept_inbound(PEER, b"outbound".to_vec());
    conns.accept_inbound(OTHER, b"other peer".to_vec());
    assert_eq!(
        conns.establish(LinkRole::Peripheral, INBOUND, 244).flushed,
        vec![b"inbound".to_vec()]
    );
}

#[test]
fn max_write_is_per_link_and_never_a_constant() {
    // Two links negotiated at different times routinely disagree.
    let mut conns = table();
    let small = conns.establish(LinkRole::Central, PEER, 20).id;
    let large = conns.establish(LinkRole::Central, OTHER, 512).id;
    assert_eq!(conns.max_write(&small), Some(20));
    assert_eq!(conns.max_write(&large), Some(512));

    conns.set_max_write(&small, 244);
    assert_eq!(conns.max_write(&small), Some(244));
}

#[test]
fn max_write_is_none_for_a_link_that_is_gone() {
    let mut conns = table();
    let id = conns.establish(LinkRole::Central, PEER, 244).id;
    conns.remove(&id);
    assert_eq!(conns.max_write(&id), None);
    assert_eq!(conns.handle(&id), None);
    assert!(conns.target(&id).is_none());
    // Recording a size for a link that no longer exists must not revive it.
    conns.set_max_write(&id, 512);
    assert_eq!(conns.max_write(&id), None);
}

#[test]
fn removing_by_id_also_forgets_what_was_buffered_for_it() {
    let mut conns = table();
    let id = conns.open(LinkRole::Peripheral, INBOUND, 244, true).id;
    conns.accept_inbound(INBOUND, b"orphan".to_vec());
    conns.remove(&id);
    assert!(conns
        .establish(LinkRole::Peripheral, INBOUND, 244)
        .flushed
        .is_empty());
}

#[test]
fn draining_returns_every_visible_link_exactly_once() {
    let mut conns = table();
    let a = conns.establish(LinkRole::Central, PEER, 244).id;
    let b = conns.establish(LinkRole::Peripheral, INBOUND, 244).id;
    conns.open(LinkRole::Central, OTHER, 244, false);

    let mut drained = conns.drain();
    drained.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(drained, expected);
    assert_eq!(conns.len(), 0);
    assert!(conns.drain().is_empty());
}

// -- event marshalling -------------------------------------------------

#[test]
fn establishing_a_link_announces_it_with_the_negotiated_size() {
    let shared = shared(8);
    shared.establish(LinkRole::Peripheral, INBOUND, 185);
    let events = drain_events(&shared);
    let link = match &events[..] {
        [LinkEvent::Connected {
            link,
            role,
            max_write,
        }] => {
            assert_eq!(*role, LinkRole::Peripheral);
            assert_eq!(*max_write, 185);
            link.clone()
        }
        other => panic!("expected one Connected, got {other:?}"),
    };

    shared.close_link(INBOUND);
    assert_eq!(
        drain_events(&shared),
        vec![LinkEvent::Disconnected { link }]
    );
}

#[test]
fn data_never_arrives_before_the_link_it_belongs_to() {
    // The ordering the Connected contract exists to guarantee.
    let shared = shared(8);
    shared.inbound(INBOUND, b"early".to_vec());
    assert!(
        drain_events(&shared).is_empty(),
        "nothing may be emitted for an unknown link"
    );

    shared.establish(LinkRole::Peripheral, INBOUND, 185);
    let events = drain_events(&shared);
    match &events[..] {
        [LinkEvent::Connected { link, .. }, LinkEvent::Data { link: on, bytes }] => {
            assert_eq!(link, on);
            assert_eq!(bytes, b"early");
        }
        other => panic!("expected Connected then Data, got {other:?}"),
    }

    shared.inbound(INBOUND, b"live".to_vec());
    assert!(matches!(
        drain_events(&shared)[..],
        [LinkEvent::Data { .. }]
    ));
}

#[test]
fn an_outbound_attempt_emits_nothing_until_it_is_writable() {
    let shared = shared(8);
    let id = shared.open_central(PEER);
    assert!(
        drain_events(&shared).is_empty(),
        "connect() must not announce a link it cannot write to"
    );

    shared.establish(LinkRole::Central, PEER, 244);
    assert_eq!(
        drain_events(&shared),
        vec![LinkEvent::Connected {
            link: id,
            role: LinkRole::Central,
            max_write: 244
        }]
    );
}

#[test]
fn losing_the_adapter_disconnects_every_link() {
    let shared = shared(16);
    shared.establish(LinkRole::Central, PEER, 244);
    shared.establish(LinkRole::Peripheral, INBOUND, 244);
    drain_events(&shared);

    shared.emit(LinkEvent::AdapterPowerChanged { powered: false });
    shared.drop_all();

    let events = drain_events(&shared);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], LinkEvent::AdapterPowerChanged { powered: false });
    assert!(events[1..]
        .iter()
        .all(|event| matches!(event, LinkEvent::Disconnected { .. })));
    assert_eq!(shared.conns().len(), 0);
}

// -- the bounded queue -------------------------------------------------

#[test]
fn queue_pressure_keeps_every_lifecycle_transition_in_order() {
    let shared = shared(2);
    let link = LinkId::new("central:test#1");
    let connected = LinkEvent::Connected {
        link: link.clone(),
        role: LinkRole::Central,
        max_write: 244,
    };
    let disconnected = LinkEvent::Disconnected { link: link.clone() };
    shared.emit(connected.clone());
    for _ in 0..8 {
        shared.emit(LinkEvent::Data {
            link: link.clone(),
            bytes: vec![1],
        });
    }
    shared.emit(disconnected.clone());
    shared.emit(LinkEvent::Data {
        link: link.clone(),
        bytes: vec![2],
    });
    shared.emit(connected.clone()); // Lifecycle-only overflow is retained too.
    assert_eq!(
        drain_events(&shared),
        vec![connected.clone(), disconnected, connected]
    );
}

#[test]
fn a_full_queue_drops_rather_than_blocking_the_binder_thread() {
    // Blocking here would stall the entire Android Bluetooth stack, and
    // buffering without limit off a radio anyone in range can talk to is an
    // out-of-memory kill waiting to happen.
    let shared = shared(2);
    for rssi in [-40, -50, -60, -70] {
        shared.emit(LinkEvent::Discovered {
            address: PEER.to_string(),
            rssi,
            peer: None,
        });
    }

    let events = drain_events(&shared);
    assert_eq!(events.len(), 2, "the queue must not grow past its bound");
    // Oldest non-lifecycle events are evicted.
    assert_eq!(
        events[0],
        LinkEvent::Discovered {
            address: PEER.to_string(),
            rssi: -60,
            peer: None
        }
    );
    assert_eq!(
        events[1],
        LinkEvent::Discovered {
            address: PEER.to_string(),
            rssi: -70,
            peer: None
        }
    );
}

#[test]
fn events_emitted_after_shutdown_are_dropped_not_queued() {
    let shared = shared(4);
    shared.close_queue();
    assert!(shared.stopped());
    shared.emit(LinkEvent::AdapterPowerChanged { powered: true });
    assert!(drain_events(&shared).is_empty());
}

#[test]
fn a_log_line_for_a_data_event_never_carries_the_payload() {
    // Mesh traffic is other people's ciphertext and logcat is not private.
    let event = LinkEvent::Data {
        link: LinkId::new("central:x#1"),
        bytes: b"secret".to_vec(),
    };
    let text = describe(&event);
    assert!(text.contains("6 bytes"), "{text}");
    assert!(!text.contains("secret"), "{text}");
}

#[test]
fn every_event_describes_itself_by_name() {
    let link = LinkId::new("central:x#1");
    for event in [
        LinkEvent::Discovered {
            address: PEER.to_string(),
            rssi: -70,
            peer: None,
        },
        LinkEvent::Connected {
            link: link.clone(),
            role: LinkRole::Central,
            max_write: 244,
        },
        LinkEvent::Disconnected { link: link.clone() },
        LinkEvent::Data {
            link,
            bytes: vec![0; 3],
        },
        LinkEvent::AdapterPowerChanged { powered: true },
    ] {
        assert!(!describe(&event).is_empty());
    }
}

// -- the MeshLink surface ---------------------------------------------

#[test]
fn events_come_back_in_order() {
    let link = AndroidLink::new();
    link.inner.emit(LinkEvent::Discovered {
        address: PEER.to_string(),
        rssi: -70,
        peer: None,
    });
    link.inner
        .emit(LinkEvent::AdapterPowerChanged { powered: true });

    block_on(async {
        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::Discovered {
                address: PEER.to_string(),
                rssi: -70,
                peer: None
            })
        );
        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::AdapterPowerChanged { powered: true })
        );
    });
}

#[test]
fn stop_drains_the_queue_and_then_ends_it() {
    let link = AndroidLink::new();
    link.inner
        .emit(LinkEvent::AdapterPowerChanged { powered: false });

    block_on(async {
        link.stop().await.expect("stop");
        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::AdapterPowerChanged { powered: false })
        );
        assert_eq!(link.next_event().await, None);
        assert_eq!(link.next_event().await, None, "the end is permanent");
    });
}

#[test]
fn start_after_stop_is_refused_rather_than_silently_dead() {
    let link = AndroidLink::new();
    block_on(async {
        link.stop().await.expect("stop");
        assert!(matches!(link.start().await, Err(LinkError::Backend(_))));
        assert!(matches!(
            link.connect(PEER).await,
            Err(LinkError::Backend(_))
        ));
        assert!(matches!(
            link.set_scanning(true).await,
            Err(LinkError::Backend(_))
        ));
        // The duty cycle calls this unconditionally, including after stop.
        assert!(link.set_scanning(false).await.is_ok());
    });
}

#[test]
fn operations_on_an_unknown_link_say_the_link_is_gone() {
    let link = AndroidLink::new();
    let unknown = LinkId::new("central:AA:BB:CC:DD:EE:FF#99");
    block_on(async {
        assert_eq!(
            link.send(&unknown, b"hello").await,
            Err(error::link_gone(&unknown))
        );
        assert_eq!(
            link.disconnect(&unknown).await,
            Err(error::link_gone(&unknown))
        );
    });
    assert_eq!(link.max_write(&unknown), None);
}

#[test]
fn an_oversized_payload_is_rejected_before_it_reaches_the_controller() {
    // Oversized writes are dropped silently by the controller, so this has
    // to be caught here or the packet vanishes with no error anywhere.
    let link = AndroidLink::new();
    link.inner.establish(LinkRole::Peripheral, INBOUND, 20);
    let id = link
        .inner
        .conns()
        .find(INBOUND)
        .expect("the link was just established");

    block_on(async {
        assert_eq!(
            link.send(&id, &[0u8; 21]).await,
            Err(LinkError::PayloadTooLarge {
                actual: 21,
                max: 20
            })
        );
        // Exactly at the limit gets past the size check and on to the
        // bridge, which is not installed in a test, so it fails there.
        assert!(!matches!(
            link.send(&id, &[0u8; 20]).await,
            Err(LinkError::PayloadTooLarge { .. })
        ));
    });
}

#[test]
fn max_write_falls_back_to_the_cached_size_when_kotlin_cannot_be_asked() {
    // No bridge in a unit test, so the table is the only source — and it
    // must not report None for a link that exists.
    let link = AndroidLink::new();
    link.inner.establish(LinkRole::Central, PEER, 244);
    let id = link
        .inner
        .conns()
        .find(PEER)
        .expect("the link was just established");
    assert_eq!(link.max_write(&id), Some(244));
}

#[test]
fn disconnect_announces_the_link_and_forgets_it() {
    let link = AndroidLink::new();
    link.inner.establish(LinkRole::Central, PEER, 244);
    let id = link
        .inner
        .conns()
        .find(PEER)
        .expect("the link was just established");
    drain_events(&link.inner);

    block_on(async {
        // The bridge call fails with no JVM; the local teardown still has
        // to happen, which is what this asserts.
        let _ = link.disconnect(&id).await;
    });
    assert_eq!(
        drain_events(&link.inner),
        vec![LinkEvent::Disconnected { link: id.clone() }]
    );
    assert_eq!(link.max_write(&id), None);
    assert_eq!(link.link_count(), 0);
}

#[test]
fn the_link_is_usable_from_several_tasks_at_once() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AndroidLink>();
    assert_send_sync::<Shared>();
}
