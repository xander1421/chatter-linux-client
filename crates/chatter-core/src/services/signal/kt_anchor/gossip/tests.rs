//! A simulated split operator and an honest one, replaying responses from
//! TestSplitViewVectors in the Go keytransparency service.

use std::collections::VecDeque;
use std::sync::Arc;

use base64::Engine as _;
use chatter_proto::chat_v1::KtSearchResponse;
use serde::Deserialize;

use super::*;
use crate::storage::Database;

const BOB: &[u8] = b"00000000-0000-4000-8000-000000000002";

#[derive(Deserialize)]
struct Vector {
    name: String,
    monitor: Option<String>,
    proof: String,
}

struct Fixture {
    verifier: KeyTransparency,
    vectors: Vec<Vector>,
    now: SystemTime,
}

impl Fixture {
    fn new() -> Self {
        let key = |hex| <[u8; 32]>::try_from(hex::decode(hex).unwrap()).unwrap();
        let verifier = KeyTransparency {
            config: PublicConfig {
                mode: DeploymentMode::ContactMonitoring,
                signature_key: VerifyingKey::from_bytes(&key(
                    "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29",
                ))
                .unwrap(),
                vrf_key: VrfPublicKey::try_from(key(
                    "48af975feaba33ea110d4ab0c8f7a38c5c7122db185ff9499b81e5e74da6ec47",
                ))
                .unwrap(),
            },
        };
        let archive =
            flate2::read::GzDecoder::new(&include_bytes!("split-view-vectors.json.gz")[..]);
        Self {
            verifier,
            vectors: serde_json::from_reader(archive).unwrap(),
            now: SystemTime::UNIX_EPOCH + Duration::from_millis(1_800_000_000_100),
        }
    }

    fn bytes(&self, name: &str, monitor: bool) -> Vec<u8> {
        let vector = self.vectors.iter().find(|vector| vector.name == name).unwrap();
        let encoded = if monitor { vector.monitor.as_deref().unwrap() } else { &vector.proof };
        base64::engine::general_purpose::STANDARD.decode(encoded).unwrap()
    }

    fn search(&self, name: &str) -> KtSearchResponse {
        KtSearchResponse::decode(self.bytes(name, false).as_slice()).unwrap()
    }

    /// The operator's answer to a joint-head request, and the request it
    /// answers, so the transport can check what the device asked for.
    fn joint(&self, name: &str) -> (KtMonitorRequest, Result<KtMonitorResponse, NetworkError>) {
        (
            KtMonitorRequest::decode(self.bytes(name, true).as_slice()).unwrap(),
            Ok(KtMonitorResponse::decode(self.bytes(name, false).as_slice()).unwrap()),
        )
    }

    /// A fresh device whose first lookup of Bob pins the head in `search`.
    async fn device(&self, search: &str) -> Device {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("kt.db"), "gossip tests").unwrap();
        db.run_migrations().unwrap();
        let settings = SettingsRepository::new(Arc::new(db));
        let mut client = Operator {
            searches: [(DISTINGUISHED, self.search("checkpoint")), (BOB, self.search(search))].into(),
            ..Default::default()
        };
        search_contact(&mut client, &settings, PinScope::Account, BOB, &self.verifier, self.now)
            .await
            .unwrap()
            .unwrap();
        assert!(client.searches.is_empty());
        Device { _dir: dir, settings }
    }
}

struct Device {
    _dir: tempfile::TempDir,
    settings: SettingsRepository,
}

impl Device {
    /// The head this device puts on an outgoing message.
    fn send(&self) -> KtHeadGossip {
        let content = Content::default().encode_to_vec();
        received(&attach(&self.settings, KtRoute::Normal, content)).unwrap()
    }

    fn pinned_size(&self) -> u64 {
        load_head(&self.settings, HEAD).unwrap().unwrap().head.0.tree_size
    }

    async fn receive(&self, fixture: &Fixture, operator: &mut Operator, head: &KtHeadGossip) -> Verdict {
        let peer = from_gossip(head).unwrap();
        let verdict = reconcile_at(
            operator,
            &self.settings,
            PinScope::Account,
            &peer,
            &fixture.verifier,
            fixture.now,
        )
        .await
        .unwrap();
        assert!(operator.searches.is_empty() && operator.monitors.is_empty());
        verdict
    }
}

/// Replays operator responses in order and checks each request.
#[derive(Default)]
struct Operator {
    searches: VecDeque<(&'static [u8], KtSearchResponse)>,
    monitors: VecDeque<(KtMonitorRequest, Result<KtMonitorResponse, NetworkError>)>,
}

impl KtTransport for Operator {
    async fn search(
        &mut self,
        request: KtSearchRequest,
    ) -> Result<Option<KtSearchResponse>, NetworkError> {
        let (label, response) = self.searches.pop_front().expect("unexpected search");
        assert_eq!(request.search_key, label);
        Ok(Some(response))
    }

    async fn monitor(
        &mut self,
        request: KtMonitorRequest,
    ) -> Result<KtMonitorResponse, NetworkError> {
        let (expected, response) = self.monitors.pop_front().expect("unexpected monitor");
        assert_eq!(request, expected);
        response
    }
}

#[tokio::test]
async fn split_operator_is_detected_on_the_first_message_each_way() {
    let fixture = Fixture::new();
    // The operator shows Alice its own key for Bob and shows Bob a clean log.
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 5").await;

    // Alice -> Bob. Bob's operator can only prove its own branch from size 4.
    let mut operator = Operator {
        monitors: [fixture.joint("B joint 5 from 4")].into(),
        ..Default::default()
    };
    assert_eq!(bob.receive(&fixture, &mut operator, &alice.send()).await, Verdict::Split);
    assert_eq!(bob.pinned_size(), 5);

    // Bob -> Alice. Alice's branch has no size 5 to prove.
    let mut operator = Operator {
        monitors: [(
            KtMonitorRequest {
                keys: Vec::new(),
                consistency: Some(KtConsistency { last: Some(4), distinguished: Some(5) }),
            },
            Err(NetworkError::InvalidRequest("baseline beyond log".into())),
        )]
        .into(),
        ..Default::default()
    };
    assert_eq!(alice.receive(&fixture, &mut operator, &bob.send()).await, Verdict::Split);
    assert_eq!(alice.pinned_size(), 4);
}

#[tokio::test]
async fn same_size_heads_from_two_branches_are_a_split_without_a_request() {
    let fixture = Fixture::new();
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 4").await;
    for (from, to) in [(&alice, &bob), (&bob, &alice)] {
        let mut operator = Operator::default();
        assert_eq!(to.receive(&fixture, &mut operator, &from.send()).await, Verdict::Split);
    }
}

#[tokio::test]
async fn honest_operator_raises_no_alarm_over_a_conversation() {
    let fixture = Fixture::new();
    let alice = fixture.device("H bob at 4").await;
    let bob = fixture.device("H bob at 5").await;

    let mut operator = Operator {
        monitors: [fixture.joint("H joint 5 from 4")].into(),
        ..Default::default()
    };
    assert_eq!(bob.receive(&fixture, &mut operator, &alice.send()).await, Verdict::Consistent);
    assert_eq!(bob.pinned_size(), 5);

    // Alice advances to the joint head, so both now share one pin.
    let mut operator = Operator {
        monitors: [fixture.joint("H joint 5 for pin 4")].into(),
        ..Default::default()
    };
    assert_eq!(alice.receive(&fixture, &mut operator, &bob.send()).await, Verdict::Consistent);
    assert_eq!(alice.pinned_size(), 5);

    // Every later message carries the shared head and needs no request.
    for _ in 0..4 {
        for (from, to) in [(&alice, &bob), (&bob, &alice)] {
            let mut operator = Operator::default();
            assert_eq!(to.receive(&fixture, &mut operator, &from.send()).await, Verdict::Consistent);
        }
    }
}

#[tokio::test]
async fn a_head_without_the_operator_signature_is_ignored() {
    let fixture = Fixture::new();
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 5").await;
    for tamper in [
        (|head: &mut KtHeadGossip| head.signature[0] ^= 1) as fn(&mut KtHeadGossip),
        |head| head.root[0] ^= 1,
        |head| head.tree_size += 1,
        |head| head.timestamp += 1,
    ] {
        let mut head = alice.send();
        tamper(&mut head);
        let mut operator = Operator::default();
        assert_eq!(bob.receive(&fixture, &mut operator, &head).await, Verdict::Invalid);
    }
    let mut malformed = alice.send();
    malformed.root.pop();
    assert!(from_gossip(&malformed).is_err());
}

#[test]
fn only_a_content_with_a_pin_carries_a_head() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("kt.db"), "gossip tests").unwrap();
    db.run_migrations().unwrap();
    let settings = SettingsRepository::new(Arc::new(db));
    let content = Content::default().encode_to_vec();
    assert_eq!(attach(&settings, KtRoute::Normal, content.clone()), content);
    assert_eq!(attach(&settings, KtRoute::Normal, vec![0xff]), vec![0xff]);
    assert!(received(&content).is_none());
}
