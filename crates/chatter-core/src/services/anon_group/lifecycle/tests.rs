use super::*;
use crate::client::ChatterClient;
use crate::storage::repositories::settings_repo::keys;
use std::sync::Arc;

struct Fixture {
    client: Arc<ChatterClient>,
    _dir: tempfile::TempDir,
    group: AnonGroup,
    inviter: AnonGroupMember,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let service = client.anon_groups();
        service
            .settings_repo
            .set(keys::CURRENT_USER_ID, &UserId::new().to_string())
            .unwrap();
        service
            .settings_repo
            .set(keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();
        let owner = UserId::new();
        let group = AnonGroup::from_invite(AnonGroupId::new(), "join regression".into(), owner);
        let inviter = AnonGroupMember {
            device_id: Some(2),
            ..AnonGroupMember::with_mailbox(group.id, owner, AnonGroupRole::Member, [7; 32])
        };
        Self {
            client,
            _dir: dir,
            group,
            inviter,
        }
    }

    fn mailbox(&self) -> AnonGroupMailbox {
        let now = Utc::now();
        AnonGroupMailbox {
            id: Uuid::new_v4().to_string(),
            group_id: self.group.id,
            public_key: [8; 32],
            private_key: [9; 64],
            token_id: "prefetched".into(),
            created_at: now,
            expires_at: now + chrono::Duration::days(30),
        }
    }

    async fn prepare(
        &self,
        result: Result<AnonGroupMailbox, AnonGroupError>,
    ) -> Result<(AnonGroup, AnonGroupMailbox), AnonGroupError> {
        self.client
            .anon_groups()
            .prepare_invited_group(self.group.clone(), self.inviter.clone(), async { result })
            .await
    }

    fn assert_empty(&self) {
        let repo = &self.client.anon_groups().anon_group_repo;
        assert!(repo.get_group(&self.group.id).unwrap().is_none());
        assert!(repo.get_mailbox(&self.group.id).unwrap().is_none());
        assert!(repo.get_members(&self.group.id).unwrap().is_empty());
        let db = &self.client.messages().database;
        let count: i64 = db
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM anon_group_member_order WHERE group_id=?1",
                [self.group.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn token_empty_and_remote_failure_leave_no_partial_group_and_can_retry() {
    let f = Fixture::new();
    let service = f.client.anon_groups();
    let result = service
        .prepare_invited_group(f.group.clone(), f.inviter.clone(), async {
            service.claim_token()?;
            panic!("empty batch must fail before remote registration")
        })
        .await;
    assert!(matches!(result, Err(AnonGroupError::CryptoError(_))));
    f.assert_empty();
    assert!(
        f.prepare(Err(AnonGroupError::MessageError(
            "remote registration failed".into()
        )))
        .await
        .is_err()
    );
    f.assert_empty();
    let (_, mailbox) = f.prepare(Ok(f.mailbox())).await.unwrap();
    assert_eq!(
        service
            .anon_group_repo
            .get_members(&f.group.id)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        service
            .anon_group_repo
            .get_mailbox(&f.group.id)
            .unwrap()
            .unwrap()
            .public_key,
        mailbox.public_key
    );
    assert!(
        service
            .anon_group_repo
            .awaiting_join_roster(&f.group.id, &service.current_user_id().unwrap())
            .unwrap()
    );
}

#[tokio::test]
async fn historical_bare_group_is_repaired_but_membership_history_is_not() {
    let f = Fixture::new();
    let repo = &f.client.anon_groups().anon_group_repo;
    repo.insert_group(&f.group).unwrap();
    assert!(
        f.prepare(Err(AnonGroupError::MessageError(
            "remote unavailable".into()
        )))
        .await
        .is_err()
    );
    assert!(repo.get_members(&f.group.id).unwrap().is_empty());
    f.prepare(Ok(f.mailbox())).await.unwrap();
    let other = Fixture::new();
    let repo = &other.client.anon_groups().anon_group_repo;
    repo.insert_group(&other.group).unwrap();
    // An inactive member proves this was a real membership, not a bare failed join.
    repo.add_member(&other.inviter).unwrap();
    repo.remove_member(&other.group.id, &other.inviter.user_id)
        .unwrap();
    assert!(other.prepare(Ok(other.mailbox())).await.is_err());
    assert!(repo.get_mailbox(&other.group.id).unwrap().is_none());
}

#[tokio::test]
async fn local_write_failure_rolls_back_group_mailbox_members_and_admission() {
    let f = Fixture::new();
    let db = &f.client.messages().database;
    db.connection().unwrap().execute_batch(
        "CREATE TRIGGER reject_second_member BEFORE INSERT ON anon_group_members
         WHEN (SELECT count(*) FROM anon_group_members)=1 BEGIN SELECT RAISE(ABORT,'injected write failure'); END;"
    ).unwrap();
    assert!(matches!(
        f.prepare(Ok(f.mailbox())).await,
        Err(AnonGroupError::Storage(_))
    ));
    f.assert_empty();
    db.connection()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_second_member")
        .unwrap();
    f.prepare(Ok(f.mailbox())).await.unwrap();
}

#[tokio::test]
async fn join_ack_failure_retries_same_mailbox_and_one_join_notice() {
    let f = Fixture::new();
    let service = f.client.anon_groups();
    let (_, mailbox) = f.prepare(Ok(f.mailbox())).await.unwrap();
    service
        .save_pending_invite(
            &crate::storage::repositories::anon_group_repo::PendingAnonGroupInvite {
                group_id: f.group.id,
                invite_epoch: 1,
                group_name: f.group.name.clone(),
                inviter_id: f.inviter.user_id,
                inviter_device_id: 2,
                inviter_mailbox_pk: [7; 32],
                profile_key: None,
                inviter_display_name: None,
                inviter_avatar: None,
            },
        )
        .unwrap();
    for _ in 0..2 {
        // No secret channel: actual JoinAck fails closed, preserving acceptance.
        assert!(service.accept_pending_invite(f.group.id).await.is_err());
        assert_eq!(service.list_pending_invites().unwrap().len(), 1);
        assert_eq!(
            service
                .anon_group_repo
                .get_mailbox(&f.group.id)
                .unwrap()
                .unwrap()
                .id,
            mailbox.id
        );
        assert_eq!(service.get_message_count(&f.group.id).unwrap(), 1);
    }
    let (_, retried) = service
        .prepare_invited_group(f.group.clone(), f.inviter.clone(), async {
            panic!("repeat acceptance must not poll mailbox creation or spend a token")
        })
        .await
        .unwrap();
    assert_eq!(retried.id, mailbox.id);
    let mut forged = f.inviter.clone();
    forged.device_id = Some(3);
    assert!(
        service
            .prepare_invited_group(f.group.clone(), forged, async {
                panic!("mismatched device must fail before token use")
            })
            .await
            .is_err()
    );
}

/// Opt-in public artifact verification, using the build's unchanged KT pins.
#[test]
#[ignore = "requires an external public manifest and its provisioned build pins"]
fn verify_external_public_mailbox_manifest() {
    use prost::Message;
    let path = std::env::var("CHATTER_PUBLIC_MANIFEST_FILE").expect("public manifest path");
    let bytes = std::fs::read(path).expect("read public manifest");
    let manifest =
        crate::proto::mailbox_v1::GetBlindSignaturePublicKeyResponse::decode(bytes.as_slice())
            .unwrap();
    assert!(
        crate::mailbox::blind_signature::ServerPublicKey::from_manifest(&manifest).is_ok(),
        "public manifest failed normal pinned signature/schedule/KT proof validation"
    );
}

impl Fixture {
    fn invite(&self, epoch: u64) -> super::super::PendingAnonGroupInvite {
        super::super::PendingAnonGroupInvite {
            group_id: self.group.id,
            invite_epoch: epoch,
            group_name: self.group.name.clone(),
            inviter_id: self.inviter.user_id,
            inviter_device_id: 2,
            inviter_mailbox_pk: [7; 32],
            profile_key: None,
            inviter_display_name: None,
            inviter_avatar: None,
        }
    }
}

#[tokio::test]
async fn replacement_or_decline_during_registration_cannot_publish() {
    for decline in [false, true] {
        let f = Fixture::new();
        let service = f.client.anon_groups();
        let invite = f.invite(1);
        service.save_pending_invite(&invite).unwrap();
        let context = service
            .capture_invite_context(Some(&invite), None)
            .await
            .unwrap();
        let (release, register) = tokio::sync::oneshot::channel();
        let accept = service.finish_invite(
            f.group.clone(),
            f.inviter.clone(),
            async {
                register.await.unwrap();
                Ok(f.mailbox())
            },
            &context,
            |_| async { panic!("obsolete acceptance sent JoinAck") },
        );
        tokio::pin!(accept);
        assert!(futures::poll!(&mut accept).is_pending());
        if decline {
            service
                .anon_group_repo
                .decline_exact_pending_invite(&invite)
                .unwrap();
        } else {
            service.save_pending_invite(&f.invite(2)).unwrap();
        }
        release.send(()).unwrap();
        assert!(accept.await.is_err());
        f.assert_empty();
        let pending = service
            .anon_group_repo
            .get_pending_invite(&f.group.id)
            .unwrap();
        assert_eq!(
            pending.map(|i| i.invite_epoch),
            if decline { None } else { Some(2) }
        );
    }
}

#[tokio::test]
async fn replacement_during_join_ack_survives_exact_completion() {
    let f = Fixture::new();
    let service = f.client.anon_groups();
    let invite = f.invite(1);
    service.save_pending_invite(&invite).unwrap();
    let context = service
        .capture_invite_context(Some(&invite), None)
        .await
        .unwrap();
    let (release, ack) = tokio::sync::oneshot::channel();
    let accept = service.finish_invite(
        f.group.clone(),
        f.inviter.clone(),
        async { Ok(f.mailbox()) },
        &context,
        |_| async {
            ack.await.unwrap();
            Ok(())
        },
    );
    tokio::pin!(accept);
    assert!(futures::poll!(&mut accept).is_pending());
    assert!(
        service
            .anon_group_repo
            .decline_exact_pending_invite(&invite)
            .is_err()
    );
    service.save_pending_invite(&f.invite(2)).unwrap();
    release.send(()).unwrap();
    accept.await.unwrap();
    assert_eq!(
        service
            .anon_group_repo
            .get_pending_invite(&f.group.id)
            .unwrap()
            .unwrap()
            .invite_epoch,
        2
    );
}

#[tokio::test]
async fn retired_registration_and_initialized_retry_have_no_publication() {
    for initialized in [false, true] {
        for same_account in [false, true] {
            let f = Fixture::new();
            let service = f.client.anon_groups();
            let me = service.current_user_id().unwrap();
            f.client
                .auth_state()
                .set_logged_in(me, 1, "old session".into());
            if initialized {
                f.prepare(Ok(f.mailbox())).await.unwrap();
            }
            let invite = f.invite(1);
            service.save_pending_invite(&invite).unwrap();
            let session = f.client.anon_group_invite_session().unwrap();
            let context = service
                .capture_invite_context(Some(&invite), Some(&session))
                .await
                .unwrap();
            let (release, register) = tokio::sync::oneshot::channel();
            let blocker = if initialized {
                Some(service.invite_acceptance.lock().await)
            } else {
                None
            };
            let accept = service.finish_invite(
                f.group.clone(),
                f.inviter.clone(),
                async {
                    assert!(!initialized, "initialized retry polled registration");
                    register.await.unwrap();
                    Ok(f.mailbox())
                },
                &context,
                |_| async { panic!("retired acceptance sent JoinAck") },
            );
            tokio::pin!(accept);
            assert!(futures::poll!(&mut accept).is_pending());
            let _ = f.client.logout().await;
            if same_account {
                service
                    .settings_repo
                    .set(keys::CURRENT_USER_ID, &me.to_string())
                    .unwrap();
                f.client
                    .auth_state()
                    .set_logged_in(me, 1, "replacement".into());
            } else {
                service.settings_repo.delete(keys::CURRENT_USER_ID).unwrap();
            }
            drop(blocker);
            let _ = release.send(());
            assert!(accept.await.is_err());
            if !initialized {
                f.assert_empty();
            }
            assert_eq!(service.get_message_count(&f.group.id).unwrap(), 0);
            assert!(
                service
                    .anon_group_repo
                    .get_pending_invite(&f.group.id)
                    .unwrap()
                    .is_some()
            );
        }
    }
}

#[tokio::test]
async fn retired_join_ack_does_not_consume_pending_invitation() {
    let f = Fixture::new();
    let service = f.client.anon_groups();
    let me = service.current_user_id().unwrap();
    f.client.auth_state().set_logged_in(me, 1, "old".into());
    let invite = f.invite(1);
    service.save_pending_invite(&invite).unwrap();
    let session = f.client.anon_group_invite_session().unwrap();
    let context = service
        .capture_invite_context(Some(&invite), Some(&session))
        .await
        .unwrap();
    let (release, ack) = tokio::sync::oneshot::channel();
    let accept = service.finish_invite(
        f.group.clone(),
        f.inviter.clone(),
        async { Ok(f.mailbox()) },
        &context,
        |_| async {
            ack.await.unwrap();
            Ok(())
        },
    );
    tokio::pin!(accept);
    assert!(futures::poll!(&mut accept).is_pending());
    let _ = f.client.logout().await;
    release.send(()).unwrap();
    assert!(accept.await.is_err());
    assert!(
        service
            .anon_group_repo
            .get_pending_invite(&f.group.id)
            .unwrap()
            .is_some()
    );
    assert_eq!(service.get_message_count(&f.group.id).unwrap(), 1);
}

#[tokio::test]
async fn authenticated_device_and_mailbox_replacement_are_distinct_incarnations() {
    for device_change in [false, true] {
        let f = Fixture::new();
        let service = f.client.anon_groups();
        let invite = f.invite(1);
        service.save_pending_invite(&invite).unwrap();
        let context = service
            .capture_invite_context(Some(&invite), None)
            .await
            .unwrap();
        let (release, register) = tokio::sync::oneshot::channel();
        let accept = service.finish_invite(
            f.group.clone(),
            f.inviter.clone(),
            async {
                register.await.unwrap();
                Ok(f.mailbox())
            },
            &context,
            |_| async { panic!("replacement sent JoinAck") },
        );
        tokio::pin!(accept);
        assert!(futures::poll!(&mut accept).is_pending());
        let mut replacement = f.invite(1);
        if device_change {
            replacement.inviter_device_id = 3;
        } else {
            replacement.inviter_mailbox_pk = [6; 32];
        }
        service.save_pending_invite(&replacement).unwrap();
        assert!(
            !service
                .anon_group_repo
                .remove_exact_pending_invite(&invite)
                .unwrap()
        );
        release.send(()).unwrap();
        assert!(accept.await.is_err());
        f.assert_empty();
        assert!(
            service
                .anon_group_repo
                .get_pending_invite(&f.group.id)
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn acceptance_uses_profile_captured_before_registration_suspends() {
    let f = Fixture::new();
    let service = f.client.anon_groups();
    let me = service.current_user_id().unwrap();
    f.client.auth_state().set_logged_in(me, 1, "old".into());
    service
        .settings_repo
        .set_string(keys::DISPLAY_NAME, "captured")
        .unwrap();
    let session = f.client.anon_group_invite_session().unwrap();
    let context = service
        .capture_invite_context(None, Some(&session))
        .await
        .unwrap();
    let (release, register) = tokio::sync::oneshot::channel();
    let accept = service.finish_invite(
        f.group.clone(),
        f.inviter.clone(),
        async {
            register.await.unwrap();
            Ok(f.mailbox())
        },
        &context,
        |_| async { Ok(()) },
    );
    tokio::pin!(accept);
    assert!(futures::poll!(&mut accept).is_pending());
    service
        .settings_repo
        .set_string(keys::DISPLAY_NAME, "later")
        .unwrap();
    release.send(()).unwrap();
    accept.await.unwrap();
    let own = service
        .anon_group_repo
        .get_member(&f.group.id, &me)
        .unwrap()
        .unwrap();
    assert_eq!(own.display_name.as_deref(), Some("captured"));
}
