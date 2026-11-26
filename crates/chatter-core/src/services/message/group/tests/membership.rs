    /// Give `client` a usable session with `(user, device)` whose remote
    /// registration id is `registration_id`, as a bundle fetch would.
    async fn store_session(
        client: &crate::client::ChatterClient,
        user: UserId,
        device: u32,
        registration_id: u32,
    ) {
        use libsignal_protocol::{
            DeviceId, IdentityKeyPair, InMemSignalProtocolStore, KeyPair, PreKeyBundle,
            ProtocolAddress, SessionStore, kem, initiate_session,
        };
        use rand::SeedableRng;
        let mut rng =
            rand::rngs::StdRng::seed_from_u64(u64::from(registration_id) << 8 | u64::from(device));
        let mut ours =
            InMemSignalProtocolStore::new(IdentityKeyPair::generate(&mut rng), 1).unwrap();
        let theirs = IdentityKeyPair::generate(&mut rng);
        let signed = KeyPair::generate(&mut rng);
        let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rng);
        let sign = |key: &[u8], rng: &mut rand::rngs::StdRng| {
            theirs
                .private_key()
                .calculate_signature(key, rng)
                .unwrap()
                .to_vec()
        };
        let device_id = DeviceId::try_from(device).unwrap();
        let bundle = PreKeyBundle::new(
            registration_id,
            device_id,
            None,
            1u32.into(),
            signed.public_key,
            sign(&signed.public_key.serialize(), &mut rng),
            1u32.into(),
            kyber.public_key.clone(),
            sign(&kyber.public_key.serialize(), &mut rng),
            *theirs.identity_key(),
        )
        .unwrap();
        let address = ProtocolAddress::new(user.to_string(), device_id);
        let local = ProtocolAddress::new("me".into(), DeviceId::try_from(1u32).unwrap());
        let mut identities = ours.clone();
        initiate_session(
            &address,
            &local,
            &mut ours,
            &mut identities,
            &bundle,
            std::time::SystemTime::now(),
            &mut rng,
        )
        .await
        .unwrap();
        let record = ours.load_session(&address).await.unwrap().unwrap();
        client
            .signal()
            .key_store
            .store_session(&format!("{user}:{device}"), &record.serialize().unwrap())
            .unwrap();
    }

    /// The outcome of an SKDM deposit that reached `devices`, each
    /// `(device, registration id of the session that encrypted to it)`.
    fn reached(devices: &[(u32, u32)]) -> super::super::server_dm::DepositOutcome {
        super::super::server_dm::DepositOutcome {
            delivered: devices.len(),
            deposits: devices.iter().map(|&(d, _)| (d, 0)).collect(),
            registration_ids: devices.iter().copied().collect(),
            ..Default::default()
        }
    }

    #[cfg(feature = "dev-trust-root")]
    #[tokio::test]
    async fn private_submit_fetches_bundles_anonymously_only_when_needed() {
        use chatter_proto::chat_v1 as pb;
        use crate::services::message::dm_bootstrap::tests::{
            cache_device, cache_fixture_sender_certificate, seed_tokens,
        };
        use rand::{SeedableRng, rngs::StdRng};
        use prost::Message as _;
        use std::{
            collections::VecDeque, convert::Infallible, future::Ready, pin::Pin,
            sync::{Arc, Mutex}, task::{Context, Poll},
        };
        use tokio::net::TcpListener;
        use tonic::{body::Body, server::NamedService, transport::Server};

        struct GrpcFrames(VecDeque<http_body::Frame<bytes::Bytes>>);
        impl http_body::Body for GrpcFrames {
            type Data = bytes::Bytes;
            type Error = Infallible;

            fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) ->
                Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>>
            {
                Poll::Ready(self.0.pop_front().map(Ok))
            }
        }

        #[derive(Clone, Default)]
        struct Probe {
            calls: Arc<Mutex<Vec<(String, bool, bool)>>>,
            mismatch: Arc<Mutex<Option<(String, Vec<u32>)>>>,
        }

        #[derive(Clone)]
        struct RpcProbe<const SERVICE: u8>(Probe);
        impl<const SERVICE: u8> NamedService for RpcProbe<SERVICE> {
            const NAME: &'static str = match SERVICE {
                0 => "chat.v1.AuthService",
                1 => "chat.v1.MessageService",
                _ => "chat.v1.ChannelService",
            };
        }
        impl<const SERVICE: u8> tower::Service<http::Request<Body>> for RpcProbe<SERVICE> {
            type Response = http::Response<Body>;
            type Error = Infallible;
            type Future = Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, request: http::Request<Body>) -> Self::Future {
                let path = request.uri().path();
                self.0.calls.lock().unwrap().push((
                    path.to_owned(),
                    request.headers().contains_key(http::header::AUTHORIZATION),
                    request.headers().contains_key("x-anonymous-credential"),
                ));
                if path.ends_with("/SendGroupMessage") {
                    if let Some((user_id, missing_devices)) = self.0.mismatch.lock().unwrap().take() {
                        let response = pb::SendGroupMessageResponse {
                            mismatched: vec![pb::AccountMismatchedDevices {
                                user_id,
                                devices: Some(pb::MismatchedDevices {
                                    missing_devices,
                                    ..Default::default()
                                }),
                            }],
                            ..Default::default()
                        };
                        let mut body = vec![0];
                        body.extend_from_slice(&(response.encoded_len() as u32).to_be_bytes());
                        response.encode(&mut body).unwrap();
                        let mut trailers = http::HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        return std::future::ready(Ok(http::Response::builder()
                            .header("content-type", "application/grpc")
                            .body(Body::new(GrpcFrames(VecDeque::from([
                                http_body::Frame::data(body.into()),
                                http_body::Frame::trailers(trailers),
                            ]))))
                            .unwrap()));
                    }
                }
                std::future::ready(Ok(tonic::Status::aborted("fixture reached").into_http()))
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let incoming = futures::stream::unfold(listener, |listener| async {
            Some((listener.accept().await.map(|(stream, _)| stream), listener))
        });
        let probe = Probe::default();
        let server = tokio::spawn(Server::builder()
            .add_service(RpcProbe::<0>(probe.clone()))
            .add_service(RpcProbe::<1>(probe.clone()))
            .add_service(RpcProbe::<2>(probe.clone()))
            .serve_with_incoming(incoming));

        let t = two_members_at(&url);
        let alice = t.alice.messages();
        alice.grpc_pool.auth().set_basic(format!("{}.1", t.alice_id), "fixture".into());
        let mut rng = StdRng::seed_from_u64(1844);
        let identity = libsignal_protocol::IdentityKeyPair::generate(&mut rng);
        let store = &alice.signal_service.key_store;
        store.set_local_identity(42, &identity.serialize()).unwrap();
        // SSv2 includes Alice's sending device, which needs her own prekeys
        // to construct its ephemeral self-session.
        alice.signal_service.actor.rotate_signed_prekey().await.unwrap();
        cache_device(alice, &t.bob_id, 1, &mut rng).await;
        cache_fixture_sender_certificate(&t.alice, t.alice_id);
        let bob = t.bob_id.to_string();
        let registration = alice.signal_service.session_registration_id(&bob, 1).unwrap();
        let gate = alice.group_gate().for_distribution(&t.channel_id).unwrap();
        let distribution = alice.create_sender_key_for_channel(&t.channel_id).await.unwrap();
        let (content, _) = alice.prepare_encrypted_content(
            &t.channel_id,
            &chatter_proto::protovalidate::test_plaintext("group post"),
            pb::message::Content::default(),
            true,
            1,
        ).await.unwrap();
        let request = pb::SendMessageRequest {
            channel_id: t.channel_id.to_string(),
            content: Some(content),
            client_request_id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        };

        // A cache miss waits for maintenance and never starts identified work.
        assert!(matches!(
            alice.submit_group_request(&request, &distribution).await,
            Err(MessageError::Network(crate::error::NetworkError::Unavailable))
        ));
        assert!(probe.calls.lock().unwrap().is_empty());

        // The recorder refuses every send. Repeated rejection leaves the
        // cached token in place and cannot request identified endorsements.
        alice.group_send_tokens.put(&t.channel_id, gate.state_hash, u64::MAX, vec![1]);
        for path in ["SendSealedMessage", "SendGroupMessage", "SendGroupMessage"] {
            let result = alice.submit_group_request(&request, &distribution).await;
            assert!(matches!(
                result,
                Err(MessageError::Network(crate::error::NetworkError::ServerError(ref detail)))
                    if detail.contains("fixture reached")
            ), "{result:?}");
            assert_eq!(
                std::mem::take(&mut *probe.calls.lock().unwrap()),
                [(format!("/chat.v1.MessageService/{path}"), false, false)]
            );
            // On the second submit Bob already holds our sender key.
            alice.record_sender_key_holders(&t.channel_id, &bob, &reached(&[(1, registration)]));
        }

        alice.database.connection().unwrap().execute(
            "UPDATE peer_key_transparency SET state='unverified-inbound' WHERE account=?1",
            [&bob],
        ).unwrap();
        assert!(matches!(
            alice.submit_group_request(&request, &distribution).await,
            Err(MessageError::RecipientKeyVerifying { peer_id }) if peer_id == bob
        ));
        assert!(probe.calls.lock().unwrap().is_empty());

        // A mismatch on an otherwise warm post triggers anonymous discovery.
        alice.database.connection().unwrap().execute(
            "UPDATE peer_key_transparency SET state='verified' WHERE account=?1",
            [&bob],
        ).unwrap();
        seed_tokens(alice, 7);
        alice.group_send_tokens.put(&t.channel_id, gate.state_hash, u64::MAX, vec![1]);
        *probe.mismatch.lock().unwrap() = Some((bob.clone(), (7..=15).collect()));
        assert!(matches!(
            alice.submit_group_request(&request, &distribution).await,
            Err(MessageError::Network(crate::error::NetworkError::InvalidRequest(_)))
        ));
        assert_eq!(alice.token_manager.available_token_count().unwrap(), 7);
        assert_eq!(
            std::mem::take(&mut *probe.calls.lock().unwrap()),
            [("/chat.v1.MessageService/SendGroupMessage".into(), false, false)]
        );
        *probe.mismatch.lock().unwrap() = Some((bob.clone(), vec![7]));
        assert!(alice.submit_group_request(&request, &distribution).await.is_err());
        assert_eq!(
            std::mem::take(&mut *probe.calls.lock().unwrap()),
            [
                ("/chat.v1.MessageService/SendGroupMessage".into(), false, false),
                ("/chat.v1.AuthService/GetPreKeyBundle".into(), false, true),
            ]
        );

        // A parked session cannot be rebuilt through identified discovery,
        // either for a private post or for a mailbox bootstrap.
        assert_eq!(alice.signal_service.archive_peer_sessions(&bob).await.unwrap(), 1);
        assert!(alice.submit_group_request(&request, &distribution).await.is_err());
        assert!(alice.bootstrap_devices(&t.bob_id).await.is_err());
        assert_eq!(
            std::mem::take(&mut *probe.calls.lock().unwrap()),
            [
                ("/chat.v1.AuthService/GetPreKeyBundle".into(), false, true),
                ("/chat.v1.AuthService/GetPreKeyBundle".into(), false, true),
            ]
        );

        // Exhaustion waits for an independent refill and sends no RPC.
        assert!(matches!(
            alice.submit_group_request(&request, &distribution).await,
            Err(MessageError::Network(crate::error::NetworkError::AnonymousCredentialsUnavailable))
        ));
        assert!(probe.calls.lock().unwrap().is_empty());
        assert_eq!(alice.token_manager.available_token_count().unwrap(), 4);

        // A private post with linked devices saves its receipt transcript and
        // can be flushed only after the independent delay.
        store_session(&t.alice, t.alice_id, 2, 1200).await;
        let mut auth = alice.auth_client();
        alice.sync_channel_receipt_targets(t.channel_id, 42, &mut auth, true).await;
        let conn = alice.database.read_connection().unwrap();
        let due: i64 = conn.query_row(
            "SELECT json_extract(value,'$.due') FROM settings WHERE key LIKE 'private.receipt-sync:%'",
            [],
            |row| row.get(0),
        ).unwrap();
        drop(conn);
        assert!((299..=900).contains(&(due - chrono::Utc::now().timestamp())));
        alice.flush_private_receipt_syncs(t.alice_id).await.unwrap();
        assert!(probe.calls.lock().unwrap().is_empty());
        server.abort();
    }

    /// The registration id comes from the session itself — the bundle's,
    /// for a session we initiated — and a device with no session has none,
    /// never 0 (0 would switch off the server's stale-device check).
    #[tokio::test]
    async fn session_registration_id_is_the_sessions_and_absent_without_one() {
        let t = two_members();
        let signal = &t.alice.messages().signal_service;
        let bob = t.bob_id.to_string();
        assert_eq!(signal.session_registration_id(&bob, 1), None);
        store_session(&t.alice, t.bob_id, 1, 4321).await;
        assert_eq!(signal.session_registration_id(&bob, 1), Some(4321));
        assert_eq!(signal.session_registration_id(&bob, 2), None);
    }

    /// A device whose registration id is unknown lacks our key, and an
    /// SKDM deposit without one is not recorded as held.
    #[tokio::test]
    async fn device_without_registration_id_is_never_a_holder() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let holders = [(bob.clone(), 1, 101)].into();
        assert!(lacks_sender_key(&holders, &bob, &[(1, None)], false));
        assert!(!lacks_sender_key(&holders, &bob, &[(1, Some(101))], false));

        let mut outcome = reached(&[(1, 101)]);
        outcome.registration_ids.clear();
        alice.record_sender_key_holders(&channel, &bob, &outcome);
        assert!(
            alice
                .signal_service
                .sender_key_holders(&channel)
                .unwrap()
                .is_empty()
        );
    }

    /// Our own device list, as `own_device_ids` would have fetched it.
    async fn set_own_devices(client: &crate::client::ChatterClient, ids: &[u32]) {
        *client.messages().own_devices.lock().await = Some((
            TimestampNanos::now().as_u64(),
            ids.iter().copied().collect(),
        ));
    }

    /// Per-device holders, Signal's `memberDevices`: a member's device that
    /// does not hold our current key puts the member up for the pre-send
    /// SKDM; one whose every device holds it is left alone. Our own other
    /// devices are tracked the same way.
    #[tokio::test]
    async fn member_device_without_our_key_gets_the_skdm_and_holders_do_not() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let me = t.alice_id.to_string();
        set_own_devices(&t.alice, &[1]).await;
        let lacking = || async {
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
        };

        // Never reached (no session): the fan-out establishes Bob's sessions.
        assert_eq!(lacking().await, vec![t.bob_id]);

        store_session(&t.alice, t.bob_id, 1, 101).await;
        store_session(&t.alice, t.bob_id, 2, 202).await;
        // The SKDM reached device 1 only: device 2 still lacks the key.
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 101)]));
        assert_eq!(
            alice.signal_service.sender_key_holders(&channel).unwrap(),
            [(bob.clone(), 1, 101)].into()
        );
        assert_eq!(
            lacking().await,
            vec![t.bob_id],
            "Bob's device 2 gets the SKDM on the next send"
        );
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(2, 202)]));
        assert!(
            lacking().await.is_empty(),
            "every device of Bob's holds it: no SKDM"
        );

        // §125: an added device of Bob's whose session could not be opened
        // is retried on the next send, and reaching it clears it.
        alice.unreached_peer_devices.lock().insert((bob.clone(), 4));
        assert_eq!(lacking().await, vec![t.bob_id]);
        store_session(&t.alice, t.bob_id, 4, 404).await;
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(4, 404)]));
        assert!(alice.unreached_peer_devices.lock().is_empty());
        assert!(lacking().await.is_empty());

        // Our own newly linked device 3 (no session yet), then reached.
        set_own_devices(&t.alice, &[1, 3]).await;
        assert_eq!(lacking().await, vec![t.alice_id]);
        store_session(&t.alice, t.alice_id, 3, 303).await;
        alice.record_sender_key_holders(&channel, &me, &reached(&[(3, 303)]));
        assert!(lacking().await.is_empty());
    }

    /// Signal compares devices on (id, registration id): a device that
    /// re-registered under the same id is a new device and gets the SKDM;
    /// its delivery replaces the old install's entry.
    #[tokio::test]
    async fn re_registered_device_counts_as_new() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let me = t.alice_id.to_string();
        set_own_devices(&t.alice, &[1]).await;
        store_session(&t.alice, t.bob_id, 1, 101).await;
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 101)]));
        assert!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
                .is_empty()
        );

        store_session(&t.alice, t.bob_id, 1, 909).await;
        assert_eq!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap(),
            vec![t.bob_id]
        );
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 909)]));
        assert_eq!(
            alice.signal_service.sender_key_holders(&channel).unwrap(),
            [(bob, 1, 909)].into()
        );
        assert!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A new key has reached nobody: rotation clears every holder. The old
    /// per-user entries are not recognized.
    #[tokio::test]
    async fn rotation_clears_the_holder_set() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let me = t.alice_id.to_string();
        set_own_devices(&t.alice, &[1]).await;
        store_session(&t.alice, t.bob_id, 1, 101).await;
        t.alice.signal().create_sender_key(&channel).await.unwrap();
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 101)]));
        assert!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
                .is_empty()
        );

        t.alice.signal().rotate_sender_key(&channel).await.unwrap();
        assert!(
            alice
                .signal_service
                .sender_key_holders(&channel)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap(),
            vec![t.bob_id]
        );

        alice
            .settings_repo
            .set_string(
                &format!("sender_key_delivered:{channel}"),
                &format!("{bob},{me}.3"),
            )
            .unwrap();
        assert!(
            alice
                .signal_service
                .sender_key_holders(&channel)
                .unwrap()
                .is_empty()
        );
    }

    // ---------------------------------------------------- verified key gating

    fn bob_gateway(t: &TwoMembers) -> crate::services::gateway::MessageGateway {
        let m = t.bob.messages();
        crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        )
    }

    /// Alice's SKDM exactly as it leaves for Bob: stamped with her verified state.
    async fn stamped_skdm(t: &TwoMembers) -> chatter_proto::content_v1::SenderKeyDistribution {
        use chatter_proto::content_v1;
        use prost::Message as _;
        let alice = t.alice.messages();
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        let verified = alice.group_gate().for_distribution(&t.channel_id).unwrap();
        let bytes = alice.skdm_plaintext(&t.channel_id, &verified, &skdm, &t.bob_id.to_string());
        match content_v1::Content::decode(bytes.as_slice())
            .unwrap()
            .content
        {
            Some(content_v1::content::Content::SenderKey(s)) => s,
            _ => panic!("not an SKDM"),
        }
    }

    /// The ghost acceptance test (Design B): a user the server lists as a
    /// member, but who is not in the verified state, gets no SKDM, its
    /// SenderKeyRequest is not answered, and no profile key reaches it.
    #[tokio::test]
    async fn server_only_member_gets_no_sender_key() {
        use crate::domain::channel::{MemberRole, Membership, NotificationPreference};
        let t = two_members();
        let alice = t.alice.messages();
        let ghost = UserId::new();
        alice
            .channel_repo
            .insert_membership(&Membership {
                channel_id: t.channel_id,
                user_id: ghost,
                role: MemberRole::Admin,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::default(),
            })
            .unwrap();
        assert!(
            alice
                .channel_repo
                .get_membership(&t.channel_id, &ghost)
                .unwrap()
                .is_some(),
            "the server's view has the ghost"
        );

        set_own_devices(&t.alice, &[1]).await;
        let me = t.alice_id.to_string();
        assert_eq!(
            alice
                .members_lacking_sender_key(&t.channel_id, &me, 1)
                .await
                .unwrap(),
            vec![t.bob_id],
            "the pre-send top-up names verified members only"
        );
        let mut fanout = alice
            .group_gate()
            .for_distribution(&t.channel_id)
            .unwrap()
            .members();
        fanout.sort();
        let mut expected = vec![t.alice_id.to_string(), t.bob_id.to_string()];
        expected.sort();
        assert_eq!(fanout, expected, "the SKDM fan-out set excludes the ghost");

        assert!(matches!(
            alice
                .answer_sender_key_request(&t.channel_id, &ghost, &[])
                .await,
            Ok(false)
        ));
        assert!(
            !t.alice
                .signal()
                .has_sender_key(&t.channel_id)
                .await
                .unwrap(),
            "the ghost's request must not even create our key"
        );
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        assert!(
            matches!(alice.send_skdm_to_member(&t.channel_id, &ghost, &skdm).await,
            Err(MessageError::EncryptionFailed(ref e)) if e.contains("not a verified member"))
        );
        let verified = alice.group_gate().for_distribution(&t.channel_id).unwrap();
        alice
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &[9; 32])
            .unwrap();
        assert!(
            alice
                .skdm_profile_key(&verified, &ghost.to_string())
                .is_none()
        );
        assert!(
            alice
                .skdm_profile_key(&verified, &t.bob_id.to_string())
                .is_some()
        );
    }

    /// No verified state yet: nothing is distributed and nothing may be sent.
    #[tokio::test]
    async fn channel_without_verified_state_is_send_blocked() {
        let t = two_members();
        let alice = t.alice.messages();
        let other = ChannelId::new();
        let err = alice.group_gate().for_distribution(&other).unwrap_err();
        assert!(matches!(
            err,
            MessageError::MembershipNotVerified { status: None, .. }
        ));
        assert_eq!(
            err.permanent_send_failure_reason(),
            Some("Membership not verified")
        );
        assert!(
            alice
                .members_lacking_sender_key(&other, &t.alice_id.to_string(), 1)
                .await
                .is_err()
        );
        assert!(matches!(
            alice
                .answer_sender_key_request(&other, &t.bob_id, &[])
                .await,
            Ok(false)
        ));
    }

    /// An SKDM from a sender outside the verified state is not stored; a
    /// verified member's is.
    #[tokio::test]
    async fn skdm_from_a_non_member_is_not_stored() {
        use chatter_proto::content_v1;
        use prost::Message as _;
        let t = two_members();
        let skdm = stamped_skdm(&t).await;
        let ghost = UserId::new();
        let gateway = bob_gateway(&t);
        let plaintext = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage {
                    timestamp: 1,
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
        .encode_to_vec();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, &plaintext)
            .await
            .unwrap();

        assert_eq!(
            gateway
                .accept_skdm(&ghost, 1, &t.channel_id, &skdm)
                .await
                .unwrap(),
            None
        );
        assert!(
            t.bob
                .signal()
                .decrypt_from_group(&ghost, 1, &ge)
                .await
                .is_err(),
            "nothing stored for the ghost"
        );

        assert!(
            gateway
                .accept_skdm(&t.alice_id, 1, &t.channel_id, &skdm)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            t.bob
                .signal()
                .decrypt_from_group(&t.alice_id, 1, &ge)
                .await
                .is_ok()
        );
    }

    /// Same revision, different hash from a verified member: Fork. The
    /// channel stops distributing and sending until acknowledged.
    #[tokio::test]
    async fn fork_from_a_member_blocks_distribution_and_send() {
        use crate::domain::verified_state::VerificationStatus;
        let t = two_members();
        // Alice and Bob diverge at revision 2 (the server showed them different changes).
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id, t.bob_id]);
        seed_verified(
            &t.bob,
            t.channel_id,
            2,
            &[t.alice_id, t.bob_id, UserId::new()],
        );
        let skdm = stamped_skdm(&t).await;
        let gateway = bob_gateway(&t);
        let bob = t.bob.messages();

        // A non-member claiming a fork changes nothing.
        assert_eq!(
            gateway
                .accept_skdm(&UserId::new(), 1, &t.channel_id, &skdm)
                .await
                .unwrap(),
            None
        );
        assert!(bob.group_gate().for_distribution(&t.channel_id).is_ok());

        assert!(
            gateway
                .accept_skdm(&t.alice_id, 1, &t.channel_id, &skdm)
                .await
                .unwrap()
                .is_some(),
            "a member's key is still installed: receiving is not distributing"
        );
        assert!(matches!(
            bob.group_gate().for_distribution(&t.channel_id),
            Err(MessageError::MembershipNotVerified {
                status: Some(VerificationStatus::Fork),
                ..
            })
        ));
        assert!(matches!(
            bob.answer_sender_key_request(&t.channel_id, &t.alice_id, &[])
                .await,
            Ok(false)
        ));
        assert!(
            bob.members_lacking_sender_key(&t.channel_id, &t.bob_id.to_string(), 1)
                .await
                .is_err(),
            "the pre-send top-up (and with it the send) is refused"
        );
        let skdm = t
            .bob
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        assert!(matches!(
            bob.send_skdm_to_member(&t.channel_id, &t.alice_id, &skdm)
                .await,
            Err(MessageError::MembershipNotVerified { .. })
        ));
    }

    struct RecordingSync(tokio::sync::mpsc::UnboundedSender<(ChannelId, u64)>);

    #[async_trait::async_trait]
    impl ChannelStateSync for RecordingSync {
        async fn fetch_channel_changes(&self, channel_id: ChannelId, peer_revision: u64) {
            let _ = self.0.send((channel_id, peer_revision));
        }
    }

    /// Stamps round-trip: an SKDM and a group DataMessage carry the sender's
    /// verified (revision, state_hash); equal state is in sync, and a peer
    /// ahead of us has the channel service fetch the changes.
    #[tokio::test]
    async fn stamps_round_trip_and_a_peer_ahead_triggers_a_fetch() {
        use crate::domain::verified_state::PeerStateCheck;
        let t = two_members();
        let alice_state = t
            .alice
            .messages()
            .group_gate()
            .for_distribution(&t.channel_id)
            .unwrap();
        let skdm = stamped_skdm(&t).await;
        assert_eq!(
            (skdm.state_revision, skdm.state_hash.clone()),
            (1, alice_state.state_hash().to_vec())
        );

        let mut dm = chatter_proto::content_v1::DataMessage::default();
        stamp_group_data_message(&mut dm, &alice_state);
        let dm = <chatter_proto::content_v1::DataMessage as prost::Message>::decode(
            prost::Message::encode_to_vec(&dm).as_slice(),
        )
        .unwrap();
        assert_eq!(
            (dm.group_state_revision, dm.group_state_hash.as_slice()),
            (1, alice_state.state_hash().as_slice())
        );

        let bob = t.bob.messages();
        let gate = bob.group_gate();
        assert_eq!(
            gate.cross_check(
                &t.channel_id,
                true,
                dm.group_state_revision,
                &dm.group_state_hash
            ),
            Some(PeerStateCheck::InSync)
        );
        assert!(gate.take_peer_ahead(&t.channel_id).is_none());

        // Alice moves to revision 3; Bob is still at 1.
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id, t.bob_id]);
        seed_verified(&t.alice, t.channel_id, 3, &[t.alice_id, t.bob_id]);
        let skdm = stamped_skdm(&t).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bob.set_channel_state_sync(Arc::new(RecordingSync(tx)));
        bob_gateway(&t)
            .accept_skdm(&t.alice_id, 1, &t.channel_id, &skdm)
            .await
            .unwrap();
        // The SKDM's SenderKeyReceived drains the parked queue, and the record.
        bob.retry_pending_group_decrypts(t.channel_id, t.alice_id, 1)
            .await;
        assert_eq!(rx.recv().await, Some((t.channel_id, 3)));
        assert!(
            gate.take_peer_ahead(&t.channel_id).is_none(),
            "drained once"
        );
        assert!(
            bob.group_gate().for_distribution(&t.channel_id).is_ok(),
            "ahead is not a fork"
        );

        // A peer behind with a hash we recorded for that revision: in sync
        // with its past, no alert.
        let back = t.alice.messages().group_gate();
        assert_eq!(
            back.cross_check(&t.channel_id, true, 1, &alice_state.state_hash()),
            Some(PeerStateCheck::PeerBehind)
        );
    }

    /// A group message from a sender outside the verified state is dropped
    /// before decrypting (no SenderKeyRequest, no resend request).
    #[tokio::test]
    async fn group_message_from_a_non_member_is_dropped() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        let t = two_members();
        let plaintext =
            &chatter_proto::protovalidate::test_plaintext("from the server's ghost");
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, plaintext)
            .await
            .unwrap();
        let mut copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let ghost = UserId::new();
        copy.author_id = ghost.to_string();
        let inbound = ChannelInbound::new(t.bob.messages().clone(), t.bob.attachments().clone(), t.bob.users().clone());
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::Dropped
        );
        assert!(
            t.bob.messages().pending_group_decrypts.lock().is_empty(),
            "not parked for an SKDM"
        );
    }
