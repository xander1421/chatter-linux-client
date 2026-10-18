use super::mock::{BrokenSinkRelay, MockRelay, Script, fast_config};
use super::protocol::NO_REASON_GIVEN;
use super::*;
use serde_json::Value;
use tokio::sync::mpsc;

fn event(id: &str) -> NostrEvent {
    NostrEvent {
        id: id.to_string(),
        pubkey: "ab".repeat(32),
        created_at: 1_700_000_000,
        kind: 20000,
        tags: vec![vec!["g".to_string(), "u4pruy".to_string()]],
        content: "hello".to_string(),
        sig: Some("cd".repeat(64)),
    }
}

fn event_frame(sub: &str, id: &str) -> String {
    let value = serde_json::to_value(event(id)).expect("event serialises");
    Value::Array(vec![Value::from("EVENT"), Value::from(sub), value]).to_string()
}

// --- URL normalisation ---

#[test]
fn normalisation_collapses_schemes_and_default_ports() {
    let cases = [
        ("wss://relay.damus.io", "wss://relay.damus.io"),
        ("https://relay.damus.io", "wss://relay.damus.io"),
        ("wss://relay.damus.io:443", "wss://relay.damus.io"),
        ("wss://relay.damus.io:8443", "wss://relay.damus.io:8443"),
        ("ws://localhost:8080", "ws://localhost:8080"),
        ("http://localhost", "ws://localhost"),
        ("ws://localhost:80", "ws://localhost"),
        // wss keeps :80 — only *its* default port drops.
        ("https://relay.example:80", "wss://relay.example:80"),
        ("WSS://RELAY.Example.COM/", "wss://relay.example.com"),
        ("  wss://relay.example  ", "wss://relay.example"),
        ("wss://relay.example/path", "wss://relay.example/path"),
        // A bare `/` path drops, so what is left is the query.
        ("wss://relay.example/?a=1", "wss://relay.example?a=1"),
        ("wss://relay.example/x?a=1", "wss://relay.example/x?a=1"),
        ("wss://relay.example#frag", "wss://relay.example"),
        ("wss://[::1]:443", "wss://[::1]"),
        ("wss://[::1]:9000", "wss://[::1]:9000"),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            normalize_relay_url(raw, None).as_deref(),
            Some(expected),
            "{raw}"
        );
    }
}

#[test]
fn normalisation_rejects_what_is_not_a_relay_url() {
    for raw in [
        "",
        "   ",
        "relay.damus.io",          // no scheme and no default offered
        "ftp://relay.damus.io",    // wrong scheme
        "wss://",                  // no host
        "wss:///path",             // no host
        "wss://user:pass@relay.x", // userinfo
        "wss://relay.x:notaport",
        "wss://relay.x:99999", // out of u16 range
        "wss://relä.example",  // non-ASCII host
        "wss://[::1",          // unterminated bracket
    ] {
        assert_eq!(normalize_relay_url(raw, None), None, "{raw}");
    }
}

#[test]
fn a_default_scheme_is_only_applied_when_none_is_present() {
    assert_eq!(
        normalize_relay_url("relay.damus.io", Some("wss")).as_deref(),
        Some("wss://relay.damus.io")
    );
    assert_eq!(
        normalize_relay_url("ws://relay.damus.io", Some("wss")).as_deref(),
        Some("ws://relay.damus.io")
    );
}

#[test]
fn directory_addresses_drop_the_scheme() {
    assert_eq!(
        directory_address("relay.paulstephenborile.com:443").as_deref(),
        Some("relay.paulstephenborile.com")
    );
    assert_eq!(
        directory_address("relay.lab.rytswd.com").as_deref(),
        Some("relay.lab.rytswd.com")
    );
    assert_eq!(
        directory_address("ws://relay.example:8080").as_deref(),
        Some("relay.example:8080")
    );
    assert_eq!(directory_address("not a host"), None);
}

// --- Filters ---

#[test]
fn an_empty_filter_is_an_empty_object() {
    assert_eq!(RelayFilter::default().to_json(), serde_json::json!({}));
}

#[test]
fn filters_serialise_tags_with_a_hash_prefix() {
    let filter = RelayFilter {
        kinds: vec![20000, 20001],
        authors: vec!["aa".to_string()],
        tags: vec![("g".to_string(), vec!["u4pruy".to_string()])],
        since: Some(1_700_000_000),
        until: Some(1_700_003_600),
        limit: Some(200),
    };
    assert_eq!(
        filter.to_json(),
        serde_json::json!({
            "kinds": [20000, 20001],
            "authors": ["aa"],
            "#g": ["u4pruy"],
            "since": 1_700_000_000,
            "until": 1_700_003_600,
            "limit": 200,
        })
    );
}

#[test]
fn a_tag_with_no_values_stays_a_matches_nothing_filter() {
    let filter = RelayFilter {
        tags: vec![("g".to_string(), Vec::new())],
        ..RelayFilter::default()
    };
    assert_eq!(filter.to_json(), serde_json::json!({ "#g": [] }));
}

#[test]
fn the_ported_filter_constructors_match_bitchat() {
    assert_eq!(
        RelayFilter::gift_wraps_for("abcd", Some(100)).to_json(),
        serde_json::json!({ "kinds": [1059], "#p": ["abcd"], "since": 100, "limit": 100 })
    );
    assert_eq!(
        RelayFilter::geohash_ephemeral("u4pruy", None, GEOHASH_INITIAL_LIMIT).to_json(),
        serde_json::json!({ "kinds": [20000, 20001], "#g": ["u4pruy"], "limit": 200 })
    );
    assert_eq!(
        RelayFilter::courier_drops(&["ff".to_string()], None, 100).to_json(),
        serde_json::json!({ "kinds": [1401], "#x": ["ff"], "limit": 100 })
    );
}

// --- Outbound framing ---

#[test]
fn subscription_requests_are_req_id_then_filters() {
    let frame = subscription_request(
        "sub-1",
        &[
            RelayFilter {
                kinds: vec![1],
                ..RelayFilter::default()
            },
            RelayFilter {
                kinds: vec![2],
                ..RelayFilter::default()
            },
        ],
    );
    let parsed: Value = serde_json::from_str(&frame).expect("valid JSON");
    assert_eq!(
        parsed,
        serde_json::json!(["REQ", "sub-1", { "kinds": [1] }, { "kinds": [2] }])
    );
}

#[test]
fn publish_and_close_requests_have_the_right_shape() {
    let frame = publish_request(&event("ff")).expect("serialises");
    let parsed: Value = serde_json::from_str(&frame).expect("valid JSON");
    let array = parsed.as_array().expect("array");
    assert_eq!(array.first().and_then(Value::as_str), Some("EVENT"));
    assert_eq!(
        array
            .get(1)
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str),
        Some("ff")
    );

    let parsed: Value = serde_json::from_str(&close_request("sub-1")).expect("valid JSON");
    assert_eq!(parsed, serde_json::json!(["CLOSE", "sub-1"]));
}

#[test]
fn subscription_ids_that_need_escaping_survive_the_round_trip() {
    let id = "sub\"1\\/\n";
    let frame = subscription_request(id, &[RelayFilter::default()]);
    let parsed: Value = serde_json::from_str(&frame).expect("valid JSON");
    assert_eq!(
        parsed
            .as_array()
            .and_then(|a| a.get(1))
            .and_then(Value::as_str),
        Some(id)
    );
}

// --- Inbound parsing ---

#[test]
fn parses_every_frame_type() {
    let sub = "sub-1";
    match parse_relay_message(&event_frame(sub, "ff")).expect("EVENT parses") {
        RelayMessage::Event {
            subscription_id,
            event,
        } => {
            assert_eq!(subscription_id, sub);
            assert_eq!(event.id, "ff");
            assert_eq!(event.kind, 20000);
            assert_eq!(event.tag_value("g"), Some("u4pruy"));
        }
        other => panic!("expected EVENT, got {other:?}"),
    }

    assert_eq!(
        parse_relay_message(r#"["EOSE","sub-1"]"#).expect("EOSE parses"),
        RelayMessage::Eose {
            subscription_id: "sub-1".to_string()
        }
    );
    assert_eq!(
        parse_relay_message(r#"["OK","ff",true,"stored"]"#).expect("OK parses"),
        RelayMessage::Ok {
            event_id: "ff".to_string(),
            accepted: true,
            message: "stored".to_string(),
        }
    );
    assert_eq!(
        parse_relay_message(r#"["NOTICE","slow down"]"#).expect("NOTICE parses"),
        RelayMessage::Notice("slow down".to_string())
    );
    assert_eq!(
        parse_relay_message(r#"["CLOSED","sub-1","auth-required"]"#).expect("CLOSED parses"),
        RelayMessage::Closed {
            subscription_id: "sub-1".to_string(),
            message: "auth-required".to_string(),
        }
    );
}

#[test]
fn a_rejecting_ok_parses_as_a_rejection() {
    // The case that matters: a socket write succeeded and the event was still
    // dropped. Nothing may read this as success.
    let parsed =
        parse_relay_message(r#"["OK","ff",false,"blocked: pow required"]"#).expect("OK parses");
    assert_eq!(
        parsed,
        RelayMessage::Ok {
            event_id: "ff".to_string(),
            accepted: false,
            message: "blocked: pow required".to_string(),
        }
    );
}

#[test]
fn an_ok_without_a_reason_gets_the_placeholder() {
    assert_eq!(
        parse_relay_message(r#"["OK","ff",false]"#).expect("OK parses"),
        RelayMessage::Ok {
            event_id: "ff".to_string(),
            accepted: false,
            message: NO_REASON_GIVEN.to_string(),
        }
    );
}

#[test]
fn malformed_frames_are_errors_never_panics() {
    let bad = [
        "",
        "not json",
        "{}",
        "[]",
        "[1,2,3]",
        r#"["EVENT"]"#,
        r#"["EVENT","sub"]"#,
        r#"["EVENT",5,{}]"#,
        r#"["EVENT","sub",{"id":"ff"}]"#, // event missing required fields
        r#"["EVENT","sub","not an object"]"#,
        r#"["EOSE"]"#,
        r#"["EOSE",null]"#,
        r#"["OK","ff"]"#,
        r#"["OK","ff","yes"]"#, // accepted must be a bool
        r#"["OK",5,true]"#,
        r#"["NOTICE"]"#,
        r#"["CLOSED"]"#,
        r#"["UNKNOWN","x"]"#,
        r#"["REQ","sub",{}]"#, // a client frame, not a relay frame
        "[\"EVENT\",\"sub\",{\"id\":\"ff\",\"pubkey\":\"aa\",\"created_at\":\"soon\",\"kind\":1,\"tags\":[],\"content\":\"\"}]",
    ];
    for frame in bad {
        assert!(parse_relay_message(frame).is_err(), "{frame}");
    }
}

#[test]
fn an_event_without_a_signature_still_parses() {
    // Signature verification belongs to the crypto layer, and the pool's
    // verifier hook is where it plugs in. Parsing must not be the gate.
    let frame = r#"["EVENT","s",{"id":"ff","pubkey":"aa","created_at":1,"kind":1,"tags":[],"content":"x"}]"#;
    match parse_relay_message(frame).expect("parses") {
        RelayMessage::Event { event, .. } => assert_eq!(event.sig, None),
        other => panic!("expected EVENT, got {other:?}"),
    }
}

#[test]
fn an_absurd_frame_type_is_truncated_in_the_error() {
    let kind = "A".repeat(5000);
    let frame = Value::Array(vec![Value::from(kind), Value::from("x")]).to_string();
    let error = parse_relay_message(&frame).expect_err("unknown type");
    assert!(error.to_string().len() < 128, "{error}");
}

// --- Backoff ---

#[test]
fn backoff_doubles_and_saturates_at_the_ceiling() {
    let config = RelayClientConfig {
        backoff_jitter_ratio: 0.0,
        ..RelayClientConfig::default()
    };
    let secs = |attempt| backoff_delay(attempt, 0.5, &config).as_secs_f64();
    assert!((secs(1) - 1.0).abs() < 1e-9);
    assert!((secs(2) - 2.0).abs() < 1e-9);
    assert!((secs(3) - 4.0).abs() < 1e-9);
    assert!((secs(9) - 256.0).abs() < 1e-9);
    // 512 would exceed the 300 s ceiling.
    assert!((secs(10) - 300.0).abs() < 1e-9);
    assert!((secs(100) - 300.0).abs() < 1e-9);
}

#[test]
fn backoff_jitter_stays_within_twenty_percent() {
    let config = RelayClientConfig::default();
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        let base = (INITIAL_BACKOFF.as_secs_f64() * 2f64.powi(attempt as i32 - 1))
            .min(MAX_BACKOFF.as_secs_f64());
        for unit in [0.0, 0.25, 0.5, 0.75, 0.999] {
            let delay = backoff_delay(attempt, unit, &config).as_secs_f64();
            assert!(delay >= base * 0.8 - 1e-9, "attempt {attempt} unit {unit}");
            assert!(delay <= base * 1.2 + 1e-9, "attempt {attempt} unit {unit}");
        }
    }
}

#[test]
fn jitter_actually_spreads_reconnects() {
    // Synchronised reconnects are how a relay outage becomes a stampede, so a
    // constant delay here would be a bug, not a simplification.
    let config = RelayClientConfig::default();
    let low = backoff_delay(5, 0.0, &config);
    let high = backoff_delay(5, 1.0, &config);
    assert!(high > low);
    assert!(high.as_secs_f64() / low.as_secs_f64() > 1.4);
}

#[test]
fn backoff_survives_hostile_inputs() {
    let config = RelayClientConfig {
        backoff_multiplier: f64::INFINITY,
        ..RelayClientConfig::default()
    };
    assert!(backoff_delay(3, f64::NAN, &config) <= MAX_BACKOFF);
    assert!(backoff_delay(u32::MAX, -5.0, &config) <= MAX_BACKOFF);

    let negative = RelayClientConfig {
        backoff_jitter_ratio: -1.0,
        ..RelayClientConfig::default()
    };
    assert!(backoff_delay(1, 0.0, &negative) >= Duration::ZERO);
}

#[test]
fn entropy_jitter_is_in_range_and_not_constant() {
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..64 {
        let unit = entropy_jitter();
        assert!((0.0..1.0).contains(&unit), "{unit}");
        seen.insert(unit.to_bits());
    }
    assert!(
        seen.len() > 32,
        "jitter must vary, got {} values",
        seen.len()
    );
}

// --- Client lifecycle, all against the mock ---

fn client<C: RelayConnector + 'static>(
    relay: Arc<C>,
) -> (Arc<RelayClient>, mpsc::UnboundedReceiver<RelayEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let client = RelayClient::new("wss://relay.test", relay, tx, fast_config()).expect("valid URL");
    (Arc::new(client), rx)
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<RelayEvent>) -> RelayEvent {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("an event arrives")
        .expect("channel open")
}

#[tokio::test]
async fn rejects_a_url_it_cannot_normalise() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let relay: Arc<dyn RelayConnector> = MockRelay::new(vec![]);
    assert!(RelayClient::new("not a url", relay, tx, fast_config()).is_err());
}

#[tokio::test]
async fn connects_replays_subscriptions_and_forwards_events() {
    let relay = MockRelay::new(vec![Script::Serve(vec![])]);
    let (client, mut rx) = client(Arc::clone(&relay));

    client
        .subscribe(
            "sub-1",
            &[RelayFilter::geohash_ephemeral("u4pruy", None, 200)],
        )
        .await
        .expect("subscribes");
    // Remembered while disconnected, not sent — and specifically *not* parked in
    // the pending-send queue, or connecting would emit the REQ twice.
    assert!(relay.sent_frames().is_empty());
    assert_eq!(client.pending_send_count().await, 0);
    assert_eq!(client.subscription_ids().await, vec!["sub-1".to_string()]);

    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    // The REQ was replayed on connect.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let sent = relay.sent_frames();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent.first()
            .is_some_and(|f| f.starts_with(r#"["REQ","sub-1""#)),
        "{sent:?}"
    );

    relay.push(&event_frame("sub-1", "ff"));
    match next_event(&mut rx).await {
        RelayEvent::Event {
            subscription_id,
            event,
            ..
        } => {
            assert_eq!(subscription_id, "sub-1");
            assert_eq!(event.id, "ff");
        }
        other => panic!("expected an event, got {other:?}"),
    }

    relay.push(r#"["EOSE","sub-1"]"#);
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Eose {
            url: "wss://relay.test".to_string(),
            subscription_id: "sub-1".to_string()
        }
    );

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn publishes_queue_while_disconnected_and_flush_on_connect() {
    let relay = MockRelay::new(vec![Script::Serve(vec![])]);
    let (client, mut rx) = client(Arc::clone(&relay));

    client.publish(&event("aa")).await.expect("queues");
    client.publish(&event("bb")).await.expect("queues");
    assert_eq!(client.pending_send_count().await, 2);

    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert_eq!(client.pending_send_count().await, 0);
    let sent = relay.sent_frames();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent.iter().all(|f| f.starts_with(r#"["EVENT""#)),
        "{sent:?}"
    );

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn the_pending_queue_is_bounded_and_drops_the_oldest() {
    let relay = MockRelay::new(vec![Script::Serve(vec![])]);
    let (client, _rx) = client(Arc::clone(&relay));

    for index in 0..PENDING_SEND_QUEUE_CAP + 25 {
        client
            .publish(&event(&format!("{index:04}")))
            .await
            .expect("queues");
    }
    assert_eq!(client.pending_send_count().await, PENDING_SEND_QUEUE_CAP);
    assert_eq!(client.dropped_send_count().await, 25);

    // The survivors are the newest ones: a stale presence beacon is worthless.
    let handle = tokio::spawn(Arc::clone(&client).run());
    tokio::time::sleep(Duration::from_millis(30)).await;
    let sent = relay.sent_frames();
    assert_eq!(sent.len(), PENDING_SEND_QUEUE_CAP);
    assert!(
        sent.first().is_some_and(|f| f.contains("\"0025\"")),
        "{:?}",
        sent.first()
    );
    assert!(
        sent.last().is_some_and(|f| f.contains("\"0224\"")),
        "{:?}",
        sent.last()
    );

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn a_failed_write_requeues_and_marks_the_socket_dead() {
    let (client, mut rx) = client(Arc::new(BrokenSinkRelay));
    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    client.publish(&event("aa")).await.expect("queues");
    assert_eq!(client.pending_send_count().await, 1);
    assert!(!client.is_connected().await);

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn reconnects_after_a_drop_and_replays_the_subscription() {
    let relay = MockRelay::new(vec![Script::ServeThenClose(vec![]), Script::Serve(vec![])]);
    let (client, mut rx) = client(Arc::clone(&relay));
    client
        .subscribe("sub-1", &[RelayFilter::default()])
        .await
        .expect("subscribes");

    let handle = tokio::spawn(Arc::clone(&client).run());

    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );
    match next_event(&mut rx).await {
        RelayEvent::Disconnected { .. } => {}
        other => panic!("expected a disconnect, got {other:?}"),
    }
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(relay.connects(), 2);
    // Replayed once per connection: a relay forgets subscriptions with the socket.
    assert_eq!(relay.sent_frames().len(), 2);
    assert_eq!(
        client.reconnect_attempts().await,
        0,
        "a success resets the counter"
    );

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn gives_up_after_the_maximum_attempts() {
    let relay = MockRelay::new((0..40).map(|_| Script::Fail("refused")).collect());
    let (client, mut rx) = client(Arc::clone(&relay));
    let handle = tokio::spawn(Arc::clone(&client).run());

    let mut disconnects = 0;
    loop {
        match next_event(&mut rx).await {
            RelayEvent::Disconnected { reason, .. } => {
                assert!(reason.contains("connect failed"), "{reason}");
                disconnects += 1;
            }
            RelayEvent::GaveUp { url } => {
                assert_eq!(url, "wss://relay.test");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(disconnects, MAX_RECONNECT_ATTEMPTS as usize);
    assert_eq!(relay.connects(), MAX_RECONNECT_ATTEMPTS as usize);

    // The loop is over: the task ends on its own.
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("run returns")
        .expect("no panic");
}

#[tokio::test]
async fn malformed_frames_are_counted_and_the_connection_survives() {
    let relay = MockRelay::new(vec![Script::Serve(vec![
        "not json".to_string(),
        r#"["EVENT","sub"]"#.to_string(),
        r#"["GARBAGE"]"#.to_string(),
    ])]);
    let (client, mut rx) = client(Arc::clone(&relay));
    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(client.malformed_frame_count().await, 3);
    assert!(
        client.is_connected().await,
        "one bad frame must not kill the socket"
    );

    // Still working afterwards.
    relay.push(r#"["EOSE","sub-1"]"#);
    match next_event(&mut rx).await {
        RelayEvent::Eose { .. } => {}
        other => panic!("expected EOSE, got {other:?}"),
    }

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn a_relay_closing_a_subscription_stops_us_replaying_it() {
    let relay = MockRelay::new(vec![Script::Serve(vec![])]);
    let (client, mut rx) = client(Arc::clone(&relay));
    client
        .subscribe("sub-1", &[RelayFilter::default()])
        .await
        .expect("subscribes");
    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    relay.push(r#"["CLOSED","sub-1","auth-required: restricted"]"#);
    match next_event(&mut rx).await {
        RelayEvent::Closed {
            subscription_id, ..
        } => assert_eq!(subscription_id, "sub-1"),
        other => panic!("expected CLOSED, got {other:?}"),
    }
    assert!(client.subscription_ids().await.is_empty());

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn unsubscribing_sends_close_and_forgets_the_subscription() {
    let relay = MockRelay::new(vec![Script::Serve(vec![])]);
    let (client, mut rx) = client(Arc::clone(&relay));
    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    client
        .subscribe("sub-1", &[RelayFilter::default()])
        .await
        .expect("subscribes");
    client.unsubscribe("sub-1").await;
    assert!(client.subscription_ids().await.is_empty());

    tokio::time::sleep(Duration::from_millis(20)).await;
    let sent = relay.sent_frames();
    assert!(sent.iter().any(|f| f == r#"["CLOSE","sub-1"]"#), "{sent:?}");

    // Unsubscribing twice is a no-op, not a second CLOSE.
    client.unsubscribe("sub-1").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        relay
            .sent_frames()
            .iter()
            .filter(|f| f.starts_with(r#"["CLOSE""#))
            .count(),
        1
    );

    client.shutdown().await;
    handle.abort();
}

#[tokio::test]
async fn a_subscription_needs_at_least_one_filter() {
    let relay = MockRelay::new(vec![]);
    let (client, _rx) = client(Arc::clone(&relay));
    assert!(client.subscribe("sub-1", &[]).await.is_err());
    assert!(client.subscription_ids().await.is_empty());
}

#[tokio::test]
async fn shutdown_stops_the_loop() {
    let relay = MockRelay::new(vec![Script::Serve(vec![])]);
    let (client, mut rx) = client(Arc::clone(&relay));
    let handle = tokio::spawn(Arc::clone(&client).run());
    assert_eq!(
        next_event(&mut rx).await,
        RelayEvent::Connected {
            url: "wss://relay.test".to_string()
        }
    );

    client.shutdown().await;
    relay.drop_connection();

    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("run returns after shutdown")
        .expect("no panic");
    assert_eq!(relay.connects(), 1, "no reconnect after shutdown");
}
