//! A simulated split operator, a restored log and an honest operator,
//! replaying responses from TestSplitViewVectors in the Go keytransparency
//! service.

use std::collections::VecDeque;
use std::sync::Arc;

use base64::Engine as _;
use chatter_proto::chat_v1::KtSearchResponse;
use serde::Deserialize;

use super::*;
use crate::storage::Database;

const BOB: &[u8] = b"00000000-0000-4000-8000-000000000002";
const STEP: Duration = Duration::from_secs(10);

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

fn decode(encoded: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(encoded).unwrap()
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

    fn search(&self, name: &str) -> KtSearchResponse {
        let vector = self.vectors.iter().find(|vector| vector.name == name).unwrap();
        KtSearchResponse::decode(decode(&vector.proof).as_slice()).unwrap()
    }

    /// The operator's answers to one monitor request, one per entry position
    /// the device may hold for Bob's label.
    fn monitor(&self, name: &str) -> Answer {
        let prefix = format!("{name} @");
        let answers: Vec<_> = self
            .vectors
            .iter()
            .filter(|vector| vector.name.starts_with(&prefix))
            .map(|vector| {
                (
                    KtMonitorRequest::decode(decode(vector.monitor.as_deref().unwrap()).as_slice())
                        .unwrap(),
                    KtMonitorResponse::decode(decode(&vector.proof).as_slice()).unwrap(),
                )
            })
            .collect();
        assert!(!answers.is_empty(), "no vectors named {name}");
        Answer::Vectors(answers)
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

    async fn check(
        &self,
        fixture: &Fixture,
        operator: &mut Operator,
        head: &KtHeadGossip,
        own_label: &[u8],
    ) -> Verdict {
        let peer = from_gossip(head).unwrap();
        let verdict = reconcile_at(
            operator,
            &self.settings,
            PinScope::Account,
            &peer,
            own_label,
            &fixture.verifier,
            fixture.now,
            STEP,
        )
        .await
        .unwrap();
        assert!(operator.searches.is_empty() && operator.monitors.is_empty());
        verdict
    }

    async fn receive(&self, fixture: &Fixture, answers: Vec<Answer>, head: &KtHeadGossip) -> Verdict {
        let mut operator = Operator {
            monitors: answers.into(),
            ..Default::default()
        };
        self.check(fixture, &mut operator, head, b"").await
    }
}

enum Answer {
    Vectors(Vec<(KtMonitorRequest, KtMonitorResponse)>),
    Fail(NetworkError),
    Stall,
}

/// Replays operator responses in order and checks each request.
#[derive(Default)]
struct Operator {
    searches: VecDeque<(&'static [u8], KtSearchResponse)>,
    monitors: VecDeque<Answer>,
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
        match self.monitors.pop_front().expect("unexpected monitor") {
            Answer::Vectors(answers) => Ok(answers
                .into_iter()
                .find(|(expected, _)| *expected == request)
                .unwrap_or_else(|| panic!("no vector answers {request:?}"))
                .1),
            Answer::Fail(error) => Err(error),
            Answer::Stall => std::future::pending().await,
        }
    }
}

fn beyond_log() -> Answer {
    Answer::Fail(NetworkError::InvalidRequest("baseline beyond log".into()))
}

#[tokio::test]
async fn split_operator_withholds_the_check_on_both_branches() {
    let fixture = Fixture::new();
    // The operator shows Alice its own key for Bob and shows Bob a clean log.
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 5").await;
    // Neither side holds signed evidence, so each check is withheld between
    // two answered routine requests. Three of them fail closed (storage).
    for _ in 0..crate::storage::peer_kt_heads::WITHHELD_LIMIT {
        // Alice -> Bob. Bob's operator can only prove its own branch.
        let answers = vec![
            fixture.monitor("B control 5/1"),
            fixture.monitor("B check 5/4"),
            fixture.monitor("B control 5/1"),
        ];
        assert_eq!(bob.receive(&fixture, answers, &alice.send()).await, Verdict::Withheld);
        // Bob -> Alice. Alice's branch has no size 5 to prove.
        let answers = vec![
            fixture.monitor("A control 4/1"),
            beyond_log(),
            fixture.monitor("A control 4/1"),
        ];
        assert_eq!(alice.receive(&fixture, answers, &bob.send()).await, Verdict::Withheld);
    }
    assert_eq!((alice.pinned_size(), bob.pinned_size()), (4, 5));
}

#[tokio::test(start_paused = true)]
async fn a_stalled_check_on_an_answering_connection_is_withheld() {
    let fixture = Fixture::new();
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 5").await;
    let answers = vec![
        fixture.monitor("B control 5/1"),
        Answer::Stall,
        fixture.monitor("B control 5/1"),
    ];
    assert_eq!(bob.receive(&fixture, answers, &alice.send()).await, Verdict::Withheld);
}

#[tokio::test(start_paused = true)]
async fn connection_setup_and_dropped_connections_are_never_withheld() {
    let fixture = Fixture::new();
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 5").await;
    // The opening routine request fails: nothing is learned.
    for opening in [Answer::Stall, Answer::Fail(NetworkError::Unavailable)] {
        assert_eq!(bob.receive(&fixture, vec![opening], &alice.send()).await, Verdict::Undecided);
    }
    // The connection dies with the check: the routine request after it fails too.
    let answers = vec![fixture.monitor("B control 5/1"), Answer::Stall, Answer::Stall];
    assert_eq!(bob.receive(&fixture, answers, &alice.send()).await, Verdict::Undecided);
}

#[tokio::test]
async fn same_size_heads_from_two_branches_are_a_split_without_a_request() {
    let fixture = Fixture::new();
    let alice = fixture.device("A bob at 4").await;
    let bob = fixture.device("B bob at 4").await;
    for (from, to) in [(&alice, &bob), (&bob, &alice)] {
        assert_eq!(to.receive(&fixture, Vec::new(), &from.send()).await, Verdict::Split);
    }
}

#[tokio::test]
async fn a_later_signed_smaller_head_is_a_signed_rollback() {
    let fixture = Fixture::new();
    let peer = fixture.device("H bob at 5").await;
    let restored = fixture.device("R bob at 4").await;
    let answers = vec![
        fixture.monitor("R control 4/1"),
        beyond_log(),
        fixture.monitor("R control 4/1"),
    ];
    assert_eq!(restored.receive(&fixture, answers, &peer.send()).await, Verdict::Split);
}

#[tokio::test]
async fn a_restored_log_beyond_our_own_pin_is_no_split() {
    let fixture = Fixture::new();
    // Our own pin is past the restored log, so the routine request is refused
    // too, and an error code is not evidence against the peer.
    let device = fixture.device("H bob at 5").await;
    let peer = fixture.device("A bob at 4").await;
    assert_eq!(device.receive(&fixture, vec![beyond_log()], &peer.send()).await, Verdict::Undecided);
    assert_eq!(device.pinned_size(), 5);
}

#[tokio::test]
async fn honest_operator_raises_no_alarm_over_a_conversation() {
    let fixture = Fixture::new();
    let alice = fixture.device("H bob at 4").await;
    let bob = fixture.device("H bob at 5").await;

    let answers = vec![fixture.monitor("H control 5/1"), fixture.monitor("H check 5/4")];
    assert_eq!(bob.receive(&fixture, answers, &alice.send()).await, Verdict::Consistent);
    assert_eq!(bob.pinned_size(), 5);

    // Alice advances to the joint head, so both now share one pin.
    let answers = vec![fixture.monitor("H control 4/1"), fixture.monitor("H check 4/5")];
    assert_eq!(alice.receive(&fixture, answers, &bob.send()).await, Verdict::Consistent);
    assert_eq!(alice.pinned_size(), 5);

    // Every later message carries the shared head and needs no request.
    for _ in 0..4 {
        for (from, to) in [(&alice, &bob), (&bob, &alice)] {
            assert_eq!(to.receive(&fixture, Vec::new(), &from.send()).await, Verdict::Consistent);
        }
    }
}

#[tokio::test]
async fn the_check_never_monitors_our_own_label() {
    let fixture = Fixture::new();
    let alice = fixture.device("H bob at 4").await;
    let bob = fixture.device("H bob at 5").await;
    // Bob's only observed label is his own, so there is nothing to blend in.
    let mut operator = Operator::default();
    assert_eq!(bob.check(&fixture, &mut operator, &alice.send(), BOB).await, Verdict::Undecided);
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
        assert_eq!(bob.receive(&fixture, Vec::new(), &head).await, Verdict::Invalid);
    }
    let mut malformed = alice.send();
    malformed.root.pop();
    assert!(from_gossip(&malformed).is_err());
}

#[tokio::test]
async fn a_padded_content_stays_on_an_exact_bucket_and_names_its_route() {
    use chatter_proto::content_v1::{DataMessage, content::Content as Body};
    let fixture = Fixture::new();
    let device = fixture.device("H bob at 5").await;
    let padded = |body: usize| {
        let mut content = Content {
            content: Some(Body::DataMessage(DataMessage {
                body: "x".repeat(body),
                ..Default::default()
            })),
            ..Default::default()
        };
        crate::services::message::MessageService::pad_content_to_bucket(&mut content);
        content.encode_to_vec()
    };
    // The head fits the 256 bucket, or moves the message to the next bucket.
    for (body, bucket) in [(10, 256), (200, 512), (420, 1024)] {
        let before = padded(body);
        let sent = attach(&device.settings, KtRoute::Normal, before.clone());
        assert_eq!(sent.len(), bucket, "body {body}, {} bytes before the head", before.len());
    }
    let head = received(&attach(&device.settings, KtRoute::Normal, padded(10))).unwrap();
    assert_eq!(route(&head), KtRoute::Normal);
    // This device has no anonymous pin, so a Tor send carries nothing.
    assert_eq!(attach(&device.settings, KtRoute::AnonymousTor, padded(10)), padded(10));
    let anonymous = KtHeadGossip {
        anonymous: true,
        ..head
    };
    assert_eq!(route(&anonymous), KtRoute::AnonymousTor);
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
