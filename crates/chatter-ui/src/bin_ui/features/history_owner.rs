//! Ownership of a history request includes its queued UI publication.
use chatter_core::{
    client::{ChatterClient, ConversationGuard, SessionGuard},
    domain::ids::MessageId,
    state::ActiveConversation,
};
use std::sync::atomic::{AtomicU64, Ordering};

static MAIN: AtomicU64 = AtomicU64::new(1);
static OLDER: AtomicU64 = AtomicU64::new(1);
static THREAD: AtomicU64 = AtomicU64::new(1);

/// Account-wide hydration remains valid across conversation changes.
#[derive(Clone)]
pub(crate) struct SessionOwner {
    session: SessionGuard,
}
impl SessionOwner {
    pub fn capture(client: &ChatterClient) -> Self {
        Self {
            session: client.session_guard(),
        }
    }
    pub fn is_current(&self) -> bool {
        self.session.is_current()
    }
}

#[derive(Clone)]
pub(crate) struct HistoryOwner {
    session: SessionGuard,
    selection: ConversationGuard,
    epoch: u64,
    mesh_epoch: Option<u64>,
    thread: Option<(MessageId, u64)>,
    older: Option<u64>,
}
impl HistoryOwner {
    /// Capture synchronously when selecting/reloading a conversation, before spawning.
    pub fn begin(client: &ChatterClient, conversation: ActiveConversation) -> Self {
        let mut owner = Self::capture(client, conversation);
        if owner.is_current() {
            owner.epoch = MAIN.fetch_add(1, Ordering::SeqCst) + 1;
        }
        owner
    }
    /// Older pages and internal loaders share the owning selection/reload epoch.
    pub fn capture(client: &ChatterClient, conversation: ActiveConversation) -> Self {
        Self {
            session: client.session_guard(),
            selection: client.conversation_guard(conversation),
            epoch: MAIN.load(Ordering::SeqCst),
            // Mesh scopes all use ActiveConversation::None. Their independent
            // selection epoch also fences captures and queued publications.
            mesh_epoch: if matches!(conversation, ActiveConversation::None) {
                mesh_epoch()
            } else {
                None
            },
            thread: None,
            older: None,
        }
    }
    /// Called on the serialized UI thread; rejected work cannot revoke accepted work.
    pub fn admit_older(client: &ChatterClient, conversation: ActiveConversation) -> Option<Self> {
        let mut owner = Self::capture(client, conversation);
        let channel = conversation.channel_id()?;
        if !owner.is_current() || client.message_is_loading(&channel) {
            return None;
        }
        client.set_message_loading(channel, true);
        owner.older = Some(OLDER.fetch_add(1, Ordering::SeqCst) + 1);
        Some(owner)
    }
    pub fn thread(client: &ChatterClient, root: MessageId, replace: bool) -> Self {
        let mut owner = Self::capture(client, client.active_conversation());
        let epoch = if replace {
            THREAD.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            THREAD.load(Ordering::SeqCst)
        };
        owner.thread = Some((root, epoch));
        owner
    }
    pub fn is_current(&self) -> bool {
        self.session.is_current()
            && self.selection.is_current()
            && MAIN.load(Ordering::SeqCst) == self.epoch
            && self
                .mesh_epoch
                .is_none_or(|epoch| mesh_epoch() == Some(epoch))
            && self
                .older
                .is_none_or(|epoch| OLDER.load(Ordering::SeqCst) == epoch)
            && self.thread.is_none_or(|(root, epoch)| {
                THREAD.load(Ordering::SeqCst) == epoch && self.selection.shows_thread(root)
            })
    }
}

fn mesh_epoch() -> Option<u64> {
    #[cfg(feature = "mesh")]
    {
        super::mesh::active_session().map(|(_, epoch)| epoch)
    }
    #[cfg(not(feature = "mesh"))]
    {
        None
    }
}

/// Check immediately before queuing and again inside the actual Slint callback.
/// Clone the owner so each request can protect all of its dependent side effects.
#[macro_export]
macro_rules! run_history_ui {
    ($weak:expr, $owner:expr, |$window:ident| $action:expr) => {{
        let owner = $owner.clone();
        if owner.is_current() {
            let weak = $weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if !owner.is_current() {
                    return;
                }
                if let Some($window) = weak.upgrade() {
                    $action
                }
            });
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MainWindow, MessageAttachmentData, MessageItemData};
    use slint::{ComponentHandle, Model, ModelRc, VecModel};

    fn pump(w: &MainWindow) {
        w.show().unwrap();
        for _ in 0..3 {
            slint::Timer::single_shot(std::time::Duration::from_millis(20), || {
                slint::quit_event_loop().unwrap()
            });
            w.run().unwrap();
        }
    }
    fn current_model(w: &MainWindow, client: &ChatterClient) {
        client.set_pinned_messages(vec![
            "00000000-0000-0000-0000-000000000002".parse().unwrap(),
        ]);
        w.global::<crate::ChatPane>()
            .set_channel_name("newer view".into());
        w.global::<crate::ChatPane>()
            .set_pending_attachments(ModelRc::new(VecModel::from(vec![
                crate::PendingAttachmentData {
                    id: "newer-pending".into(),
                    ..Default::default()
                },
            ])));
        w.global::<crate::ChatPane>()
            .set_composer_text("newer draft".into());
        w.global::<crate::ChatPane>().set_composer_is_replying(true);
        w.global::<crate::ChatPane>()
            .set_pinned_message_preview("newer pin".into());
        w.global::<crate::ChatPane>().set_pinned_total_count(7);
        w.global::<crate::ChatPane>().set_loading_messages(false);
        w.global::<crate::ChatPane>()
            .set_messages(ModelRc::new(VecModel::from(vec![MessageItemData {
                message_id: "newer-row".into(),
                content: "newer message".into(),
                ..Default::default()
            }])));
    }
    fn assert_preserved(w: &MainWindow, client: &ChatterClient) {
        assert_eq!(
            w.global::<crate::ChatPane>()
                .get_pending_attachments()
                .row_data(0)
                .unwrap()
                .id,
            "newer-pending"
        );
        assert_eq!(
            client.pinned_current_message_id(),
            Some("00000000-0000-0000-0000-000000000002".parse().unwrap())
        );
        assert_eq!(
            w.global::<crate::ChatPane>().get_channel_name(),
            "newer view"
        );
        assert_eq!(
            w.global::<crate::ChatPane>().get_composer_text(),
            "newer draft"
        );
        assert!(w.global::<crate::ChatPane>().get_composer_is_replying());
        assert_eq!(
            w.global::<crate::ChatPane>().get_pinned_message_preview(),
            "newer pin"
        );
        assert_eq!(w.global::<crate::ChatPane>().get_pinned_total_count(), 7);
        assert!(!w.global::<crate::ChatPane>().get_loading_messages());
        assert_eq!(w.global::<crate::ChatPane>().get_messages().row_count(), 1);
        assert_eq!(
            w.global::<crate::ChatPane>()
                .get_messages()
                .row_data(0)
                .unwrap()
                .message_id,
            "newer-row"
        );
    }
    fn queue(
        w: &MainWindow,
        owner: &HistoryOwner,
        display: crate::bin_ui::converters::AttachmentDisplayData,
        client: &std::sync::Arc<ChatterClient>,
    ) {
        super::super::sidebar::apply_pinned_ui_owned(
            &w.as_weak(),
            client,
            vec!["00000000-0000-0000-0000-000000000001".parse().unwrap()],
            "old pin".into(),
            owner,
        );
        crate::run_history_ui!(w.as_weak(), owner, |w| {
            w.global::<crate::ChatPane>()
                .set_channel_name("old video view".into());
            w.global::<crate::ChatPane>()
                .set_pending_attachments(ModelRc::new(VecModel::from(Vec::<
                    crate::PendingAttachmentData,
                >::new())));
            w.global::<crate::ChatPane>().set_composer_text("".into());
            w.global::<crate::ChatPane>()
                .set_composer_is_replying(false);
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview("old pin".into());
            w.global::<crate::ChatPane>().set_pinned_total_count(1);
            w.global::<crate::ChatPane>().set_loading_messages(true);
            w.global::<crate::ChatPane>()
                .set_messages(ModelRc::new(VecModel::from(vec![MessageItemData {
                    message_id: "old-row".into(),
                    attachments: ModelRc::new(VecModel::<MessageAttachmentData>::from(vec![
                        display.to_slint(),
                    ])),
                    ..Default::default()
                }])));
        });
    }
    /// Pause the production attachment conversion/decode boundary with an owned frame.
    fn derive(
        started: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> crate::bin_ui::converters::AttachmentDisplayData {
        use chatter_core::domain::{
            attachment::{Attachment, FileMetadata},
            ids::AttachmentId,
        };
        let mut attachment = Attachment::new(
            AttachmentId::new(),
            FileMetadata::new("owned.mp4".into(), "video/mp4".into(), 32),
        );
        started.send(()).unwrap();
        resume.recv().unwrap();
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 4)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        attachment.metadata.thumbnail = Some(bytes.into_inner());
        let mut display = crate::bin_ui::converters::attachment_to_display_data(&attachment);
        display.decode_thumbnail();
        assert!(display.thumbnail_pixels.is_some());
        display
    }
    #[test]
    fn history_thumbnail_owner_rejects_superseded_older_pages_and_reopened_threads() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _runtime = runtime.enter();
        let fixture = tempfile::tempdir().unwrap();
        // Off the real keyring: the marker must exist before the client opens.
        let db_path =
            chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
                .unwrap()
                .database_path();
        std::fs::write(
            db_path
                .parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(fixture.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let channel = chatter_core::domain::ids::ChannelId::new();
        let conversation = ActiveConversation::Channel(channel);
        client.select_conversation(conversation);
        HistoryOwner::begin(&client, conversation);
        let first = HistoryOwner::admit_older(&client, conversation).unwrap();
        assert!(HistoryOwner::admit_older(&client, conversation).is_none());
        assert!(
            first.is_current(),
            "admitted page survives rejected duplicate"
        );
        client.set_message_loading(channel, false);
        let second = HistoryOwner::admit_older(&client, conversation).unwrap();
        client.set_message_loading(channel, false);
        assert!(!first.is_current());
        assert!(second.is_current());
        HistoryOwner::begin(&client, conversation);
        assert!(!second.is_current());
        let root = MessageId::new();
        client.open_thread(channel, root);
        let old = HistoryOwner::thread(&client, root, true);
        assert!(old.is_current());
        client.close_thread(root);
        assert!(!old.is_current());
        client.open_thread(channel, root);
        let current = HistoryOwner::thread(&client, root, true);
        assert!(!old.is_current());
        assert!(current.is_current());
        client.testing_invalidate_session();
        assert!(!current.is_current());
    }
    #[test]
    fn history_thumbnail_publication_preserves_new_view_draft_and_model_after_delay_switch_logout_and_reload()
     {
        const MARKER: &str = "CHATTER_HISTORY_THUMBNAIL_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let test = format!(
                "{}::history_thumbnail_publication_preserves_new_view_draft_and_model_after_delay_switch_logout_and_reload",
                module_path!().split_once("::").unwrap().1
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture", "--test-threads=1"])
                .env(MARKER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("HISTORY_THUMBNAIL_COMPLETED")
            );
            return;
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _runtime = runtime.enter();
        let w = MainWindow::new().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        // Off the real keyring: the marker must exist before the client opens.
        let db_path =
            chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
                .unwrap()
                .database_path();
        std::fs::write(
            db_path
                .parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(fixture.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let a = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        let b = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        // Delayed conversion completes only after switch or logout: reject before queueing.
        for logout in [false, true] {
            client.select_conversation(a);
            let owner = HistoryOwner::begin(&client, a);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let work = std::thread::spawn(move || derive(started_tx, resume_rx));
            started_rx.recv().unwrap();
            if logout {
                client.testing_invalidate_session();
            } else {
                client.select_conversation(b);
                HistoryOwner::begin(&client, b);
            }
            current_model(&w, &client);
            resume_tx.send(()).unwrap();
            queue(&w, &owner, work.join().unwrap(), &client);
            pump(&w);
            assert_preserved(&w, &client);
        }
        // Fully decoded bytes have already queued a callback: recheck at publication.
        for case in 0..5 {
            client.select_conversation(a);
            let owner = HistoryOwner::begin(&client, a);
            let (started_tx, _started_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            resume_tx.send(()).unwrap();
            queue(&w, &owner, derive(started_tx, resume_rx), &client);
            match case {
                0 => {
                    client.select_conversation(b);
                    HistoryOwner::begin(&client, b);
                }
                1 => {
                    client.testing_invalidate_session();
                }
                2 => {
                    client.testing_invalidate_session();
                    client.select_conversation(a);
                    HistoryOwner::begin(&client, a);
                }
                3 => {
                    client.select_conversation(b);
                    HistoryOwner::begin(&client, b);
                    client.select_conversation(a);
                    HistoryOwner::begin(&client, a);
                }
                _ => {
                    HistoryOwner::begin(&client, a);
                } // same owner/conversation, newer reload
            }
            current_model(&w, &client);
            pump(&w);
            assert_preserved(&w, &client);
        }
        // Current-owner control really publishes its decoded frame.
        client.select_conversation(a);
        let owner = HistoryOwner::begin(&client, a);
        let (started_tx, _started_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        resume_tx.send(()).unwrap();
        assert!(owner.is_current(), "current control before queue");
        queue(&w, &owner, derive(started_tx, resume_rx), &client);
        pump(&w);
        assert!(owner.is_current(), "current control after queue");
        assert_eq!(
            w.global::<crate::ChatPane>().get_channel_name(),
            "old video view"
        );
        assert!(
            w.global::<crate::ChatPane>()
                .get_messages()
                .row_data(0)
                .unwrap()
                .attachments
                .row_data(0)
                .unwrap()
                .has_thumbnail
        );
        // Global account hydration accepts chat switches but rejects logout/new account,
        // including a change after the publication has already been queued.
        for stale in [false, true] {
            let session = SessionOwner::capture(&client);
            crate::run_history_ui!(w.as_weak(), session, |w| {
                w.global::<crate::SidebarPane>()
                    .set_user_display_name("captured account".into());
            });
            client.select_conversation(b);
            HistoryOwner::begin(&client, b);
            if stale {
                client.testing_invalidate_session();
            }
            w.global::<crate::SidebarPane>()
                .set_user_display_name("newer account".into());
            pump(&w);
            assert_eq!(
                w.global::<crate::SidebarPane>().get_user_display_name(),
                if stale {
                    "newer account"
                } else {
                    "captured account"
                }
            );
        }
        eprintln!("HISTORY_THUMBNAIL_COMPLETED");
    }
}
