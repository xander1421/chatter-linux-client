use super::*;
use crate::storage::repositories::settings_repo::keys;

/// We are a plain member of a group owned by `owner`, with a second member
/// `mallory` who has a known mailbox.
struct Fixture {
    _client: std::sync::Arc<crate::client::ChatterClient>,
    _dir: tempfile::TempDir,
    group_id: AnonGroupId,
    owner: UserId,
    mallory: UserId,
    victim: UserId,
}

const VICTIM_PK: [u8; 32] = [7u8; 32];

fn fixture() -> Fixture {
    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = crate::client::ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");
    let me = UserId::new();
    let s = &client.messages().settings_repo;
    s.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
    s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();

    let (owner, mallory, victim) = (UserId::new(), UserId::new(), UserId::new());
    let group_id = AnonGroupId::new();
    let repo = &client.anon_groups().anon_group_repo;
    repo.insert_group(&AnonGroup::from_invite(group_id, "g".into(), owner))
        .unwrap();
    repo.add_member(&AnonGroupMember {
        device_id: Some(2),
        ..AnonGroupMember::with_mailbox(group_id, owner, AnonGroupRole::Owner, [1u8; 32])
    })
    .unwrap();
    repo.add_member(&AnonGroupMember {
        device_id: Some(2),
        ..AnonGroupMember::with_mailbox(group_id, mallory, AnonGroupRole::Member, [2u8; 32])
    })
    .unwrap();
    repo.add_member(&AnonGroupMember {
        device_id: Some(2),
        ..AnonGroupMember::with_mailbox(group_id, victim, AnonGroupRole::Member, VICTIM_PK)
    })
    .unwrap();
    Fixture {
        _client: client,
        _dir: dir,
        group_id,
        owner,
        mallory,
        victim,
    }
}

fn info(user: UserId, role: AnonGroupRole, pk: [u8; 32]) -> AnonGroupMemberInfo {
    AnonGroupMemberInfo {
        user_id: user.as_bytes().to_vec(),
        mailbox_pk: pk.to_vec(),
        role: role.to_i32(),
        profile_key: None,
        encrypted_display_name: None,
        avatar: None,
        admission_timestamp: 0,
        device_id: 0,
    }
}

/// The forged list: mallory promotes herself to Owner and re-points the
/// victim's mailbox at a key she holds.
fn forged(f: &Fixture) -> Vec<AnonGroupMemberInfo> {
    vec![
        info(f.mallory, AnonGroupRole::Owner, [2u8; 32]),
        info(f.victim, AnonGroupRole::Member, [9u8; 32]),
    ]
}

#[tokio::test]
async fn real_group_fanout_refuses_replaced_session_after_recipient_preparation() {
    use crate::services::gateway::submission::{SEND_SESSION, SendSession, track_submission};
    for earlier_submission in [false, true] {
        let f = fixture();
        let state = f._client.auth_state();
        let me = f._client.anon_groups().current_user_id().unwrap();
        // This test exercises an established sender, not a non-member denial.
        // Without self membership send_message returns before the hook.
        f._client
            .anon_groups()
            .anon_group_repo
            .add_member(&AnonGroupMember {
                device_id: Some(1),
                ..AnonGroupMember::with_mailbox(f.group_id, me, AnonGroupRole::Member, [3; 32])
            })
            .unwrap();
        state.set_logged_in(me, 1, "old".into());
        let captured = SendSession::capture(state);
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let resume = std::sync::Arc::new(tokio::sync::Notify::new());
        let client = f._client.clone();
        let group = f.group_id;
        let gates = (ready.clone(), resume.clone());
        let mut task = tokio::spawn(SEND_SESSION.scope(
            captured,
            RECIPIENT_PREPARED.scope(
                gates,
                track_submission(
                    async move {
                        if earlier_submission {
                            crate::services::gateway::submission::mark_existing_submission();
                        }
                        client
                            .anon_groups()
                            .send_message(
                                &group,
                                crate::services::build_text_content("old content", 41),
                            )
                            .await
                    },
                    None,
                ),
            ),
        ));
        tokio::select! {
            _ = ready.notified() => {},
            result = &mut task => panic!("fanout ended before recipient preparation: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                task.abort();
                panic!("recipient preparation hook was not reached within five seconds");
            }
        }
        // Same-account login replaces the captured session, too.
        state.set_logged_in(me, 1, "replacement".into());
        resume.notify_one();
        let (result, crossed, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                .await
                .unwrap_or_else(|_| {
                    task.abort();
                    panic!("fanout did not finish after recipient preparation resumed");
                })
                .unwrap();
        assert!(matches!(result, Err(AnonGroupError::Storage(_))));
        assert_eq!(crossed, earlier_submission);
        let db = f._client.messages().database.clone();
        let conn = db.connection().unwrap();
        let lower_claims: i64 = conn
            .query_row(
                "SELECT count(*) FROM settings WHERE key LIKE 'send.anonymous.v1:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            lower_claims, 0,
            "pre-encryption lower claim must be released"
        );
        drop(conn);
        // The expired send must not become durable parked work.
        assert!(
            f._client
                .anon_groups()
                .anon_group_repo
                .get_pending_messages_for_member(&group, &f.owner)
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn existing_unknown_recipient_claim_stays_uncertain_after_session_replacement() {
    use crate::services::gateway::submission::{self, SEND_SESSION, SendSession};
    let f = fixture();
    let svc = f._client.anon_groups();
    let me = svc.current_user_id().unwrap();
    let state = f._client.auth_state();
    state.set_logged_in(me, 1, "old".into());
    let session = SendSession::capture(state);
    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(
            content_v1::AnonGroupMessage {
                group_id: f.group_id.to_vec(),
                inner_content: Some(Box::new(crate::services::build_text_content(
                    "retained", 42,
                ))),
            },
        ))),
        ephemeral_reply_to: None,
        timestamp: 42,
        padding: vec![],
    };
    let bytes = content.encode_to_vec();
    let db = svc.settings_repo.database();
    let key = match submission::prepare(db, &me.to_string(), &f.owner.to_string(), &bytes).unwrap()
    {
        submission::Admission::First(key) => key,
        _ => panic!("new claim"),
    };
    submission::ciphertext(db, &key, b"original ciphertext").unwrap();
    state.set_logged_in(me, 1, "replacement".into());
    let (result, crossed, _) = SEND_SESSION
        .scope(
            session,
            submission::track_submission(
                svc.send_to_anon_mailbox(&f.owner, Some(1), &[1; 32], content),
                None,
            ),
        )
        .await;
    assert!(matches!(result, Err(AnonGroupError::DeliveryUnconfirmed)));
    assert!(crossed);
    assert!(matches!(
        submission::prepare(db, &me.to_string(), &f.owner.to_string(), &bytes).unwrap(),
        submission::Admission::Unconfirmed
    ));
}

/// With no Tor route an invite fails closed; it never falls back to the
/// friend DM, which would show the server who is invited where.
#[tokio::test]
async fn invite_without_tor_fails_closed() {
    let f = fixture();
    let svc = f._client.anon_groups();
    let me = svc.current_user_id().unwrap();
    let group = AnonGroup::new_as_owner("mine".into(), me);
    let repo = &svc.anon_group_repo;
    repo.insert_group(&group).unwrap();
    repo.add_member(&AnonGroupMember::with_mailbox(
        group.id,
        me,
        AnonGroupRole::Owner,
        [1u8; 32],
    ))
    .unwrap();

    // Reach the routing check with a real pending admission. Without this,
    // invitation validation returns early and this test never exercises Tor.
    assert!(
        repo.invite_ordered_member(
            &AnonGroupMember::new(group.id, f.victim, AnonGroupRole::Member),
            &me,
            crate::util::time::TimestampNanos::now().as_u64(),
        )
        .unwrap()
    );

    let res = svc
        .send_invite(f.victim, group.id, "mine", &[1u8; 32])
        .await;
    assert!(
        matches!(&res, Err(AnonGroupError::MessageError(e)) if e.contains("Tor")),
        "{res:?}"
    );
}

#[tokio::test]
async fn non_owner_member_list_changes_nothing() {
    let f = fixture();
    let svc = f._client.anon_groups();
    let repo = &svc.anon_group_repo;

    let res = svc.handle_member_list(f.group_id, f.mallory, forged(&f));
    assert!(
        matches!(res, Err(AnonGroupError::InsufficientPermissions(_))),
        "{res:?}"
    );

    assert_eq!(
        repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
        Some(AnonGroupRole::Member)
    );
    let victim = repo.get_member(&f.group_id, &f.victim).unwrap().unwrap();
    assert_eq!(
        victim.mailbox_pk,
        Some(VICTIM_PK),
        "a non-owner must not re-point another member's mailbox"
    );

    // Nor can a stranger who merely knows the group_id.
    let stranger = UserId::new();
    assert!(
        svc.handle_member_list(
            f.group_id,
            stranger,
            vec![info(stranger, AnonGroupRole::Admin, [3u8; 32])]
        )
        .is_err()
    );
    assert!(!repo.is_member(&f.group_id, &stranger).unwrap());
}

#[tokio::test]
async fn owner_member_list_is_applied() {
    let f = fixture();
    let svc = f._client.anon_groups();
    let repo = &svc.anon_group_repo;

    // Even the owner's list cannot crown a second Owner: refused whole.
    svc.handle_member_list(f.group_id, f.owner, forged(&f))
        .unwrap();
    assert_eq!(
        repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
        Some(AnonGroupRole::Member)
    );

    let list = vec![
        info(f.owner, AnonGroupRole::Owner, [1u8; 32]),
        info(f.mallory, AnonGroupRole::Admin, [2u8; 32]),
        info(f.victim, AnonGroupRole::Member, [9u8; 32]),
    ];
    svc.handle_member_list_ordered(f.group_id, f.owner, list, 1)
        .unwrap();
    assert_eq!(
        repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
        Some(AnonGroupRole::Admin)
    );
    let victim = repo.get_member(&f.group_id, &f.victim).unwrap().unwrap();
    assert_eq!(victim.mailbox_pk, Some([9u8; 32]));
}

/// RC107: a member's avatar whose fetch fails (here: no route) is
/// reported back for a retry pass, not dropped; an entry with no usable
/// pointer is not.
#[tokio::test]
async fn failed_roster_avatar_fetch_is_kept_for_a_retry() {
    let f = fixture();
    let svc = f._client.anon_groups();
    let mut fetchable = info(f.victim, AnonGroupRole::Member, [9u8; 32]);
    fetchable.avatar = Some(crate::proto::content_v1::AttachmentPointer {
        cdn_key: "k".into(),
        plaintext_size: 3,
        plaintext_hash: "00".into(),
        ..Default::default()
    });
    let mut unusable = info(f.mallory, AnonGroupRole::Member, [2u8; 32]);
    unusable.avatar = Some(Default::default());
    let (changed, failed) = svc
        .refresh_roster_avatars(
            &f.group_id,
            &[fetchable.clone(), unusable],
            &[std::time::Duration::ZERO],
        )
        .await
        .unwrap();
    assert!(!changed);
    assert_eq!(failed, vec![fetchable]);
}
