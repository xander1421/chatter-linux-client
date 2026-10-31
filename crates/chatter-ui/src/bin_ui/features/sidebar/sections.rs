//! Custom section callbacks for the sidebar.
//!
//! Wires section management (create, delete, rename, toggle, move, reorder,
//! sort mode, pin) and loads section data on startup.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::SectionId;
use chatter_core::domain::section::SectionSortMode;

use chatter_core::ui::sidebar_data::ChatRowSendData;

use crate::bin_ui::converters::ChatRowExt;
use crate::{ChatRowData, CustomSectionData, MainWindow, run_ui};

// Retained core ordering metadata, indexed independently of Slint row contents.
// Structural snapshots invalidate in-flight member reads; per-key revisions
// prevent a slower read from overwriting a newer arrival.
#[derive(Default)]
struct MemberIndex {
    epoch: u64,
    snapshot_pending: bool,
    refresh_queued: bool,
    snapshot_dirty: std::collections::HashSet<String>,
    in_flight: std::collections::HashSet<String>,
    dirty: std::collections::HashSet<String>,
    revision: u64,
    members: std::collections::HashMap<String, (usize, usize, u64)>,
    contributions: std::collections::HashMap<String, i32>,
    sections: Vec<Vec<chatter_core::domain::section::SectionConversation>>,
}
impl MemberIndex {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.snapshot_pending = false;
        self.snapshot_dirty.clear();
        self.in_flight.clear();
        self.dirty.clear();
    }

    fn begin_snapshot(&mut self) -> u64 {
        self.invalidate();
        self.snapshot_pending = true;
        self.epoch
    }

    fn finish_snapshot(&mut self, epoch: u64) -> Option<std::collections::HashSet<String>> {
        if self.epoch != epoch {
            return None;
        }
        self.snapshot_pending = false;
        Some(std::mem::take(&mut self.snapshot_dirty))
    }

    fn begin(
        &mut self,
        key: &str,
    ) -> Option<(u64, u64, chatter_core::domain::section::SectionConversation)> {
        if self.snapshot_pending {
            self.snapshot_dirty.insert(key.to_owned());
            return None;
        }
        let &(section, row, _) = self.members.get(key)?;
        if !self.in_flight.insert(key.to_owned()) {
            self.dirty.insert(key.to_owned());
            return None;
        }
        self.revision = self.revision.wrapping_add(1);
        let revision = self.revision;
        self.members.get_mut(key)?.2 = revision;
        Some((self.epoch, revision, self.sections[section][row].clone()))
    }

    fn finish(&mut self, epoch: u64, key: &str) -> (bool, bool) {
        if self.epoch != epoch {
            return (false, false);
        }
        self.in_flight.remove(key);
        (true, self.dirty.remove(key))
    }
}
struct WindowMembers {
    window: slint::Weak<MainWindow>,
    client: std::sync::Weak<ChatterClient>,
    session: chatter_core::client::SessionGuard,
    index: MemberIndex,
}
thread_local! {
    static MEMBERS: std::cell::RefCell<Vec<WindowMembers>> = Default::default();
}

// This registry is used only on the UI thread. Window identity and the session
// revision own both queued publications and retained member metadata.
fn with_members<R>(
    window: &MainWindow,
    client: &Arc<ChatterClient>,
    f: impl FnOnce(&mut MemberIndex) -> R,
) -> R {
    MEMBERS.with(|states| {
        let mut states = states.borrow_mut();
        states.retain(|s| s.window.upgrade().is_some());
        let position = states.iter().position(|s| {
            s.window
                .upgrade()
                .is_some_and(|w| std::ptr::eq(w.window(), window.window()))
        });
        let state = if let Some(position) = position {
            &mut states[position]
        } else {
            states.push(WindowMembers {
                window: window.as_weak(),
                client: Arc::downgrade(client),
                session: client.session_guard(),
                index: MemberIndex::default(),
            });
            states.last_mut().unwrap()
        };
        if !state.session.is_current() || !state.client.ptr_eq(&Arc::downgrade(client)) {
            // Preserve monotonic generations: a retired completion cannot collide
            // with the first request in a replacement session for the same account.
            state.index.invalidate();
            let epoch = state.index.epoch;
            state.index = MemberIndex {
                epoch,
                ..Default::default()
            };
            state.session = client.session_guard();
            state.client = Arc::downgrade(client);
        }
        f(&mut state.index)
    })
}

// Completion is not admission: an old client's callback must never reset a
// replacement client's retained state merely by attempting publication.
fn owns_members(window: &MainWindow, client: &Arc<ChatterClient>) -> bool {
    MEMBERS.with(|states| {
        states.borrow().iter().any(|s| {
            s.window
                .upgrade()
                .is_some_and(|w| std::ptr::eq(w.window(), window.window()))
                && s.client.ptr_eq(&Arc::downgrade(client))
                && s.session.is_current()
        })
    })
}

fn invalidate_structure(window: &MainWindow, client: &Arc<ChatterClient>) {
    with_members(window, client, MemberIndex::invalidate);
}

/// Coalesce badge snapshot fallbacks per window and session, rather than
/// allowing another window's queued timer to suppress this refresh.
pub(super) fn queue_refresh(window: &MainWindow, client: &Arc<ChatterClient>) {
    if with_members(window, client, |state| {
        std::mem::replace(&mut state.refresh_queued, true)
    }) {
        return;
    }
    let weak = window.as_weak();
    let client = client.clone();
    let session = client.session_guard();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        if !session.is_current() {
            return;
        }
        if let Some(w) = weak.upgrade() {
            if !owns_members(&w, &client) {
                return;
            }
            with_members(&w, &client, |state| state.refresh_queued = false);
            refresh_sections_ui(&weak, &client);
        }
    });
}

fn index_members(
    state: &mut MemberIndex,
    sections: &[chatter_core::domain::section::CustomSection],
    contributions: std::collections::HashMap<String, i32>,
) {
    {
        state.members.clear();
        state.contributions = contributions;
        state.in_flight.clear();
        state.dirty.clear();
        state.sections = sections.iter().map(|s| s.conversations.clone()).collect();
        // The service moves memberships exclusively, but schema v48 only
        // enforces (section_id, conversation_id). Preserve duplicate historical
        // or concurrently-written memberships through the snapshot fallback.
        let mut duplicates = std::collections::HashSet::new();
        for (i, section) in sections.iter().enumerate() {
            for (j, row) in section.conversations.iter().enumerate() {
                if duplicates.contains(&row.conversation_id) {
                    continue;
                }
                if state
                    .members
                    .insert(row.conversation_id.clone(), (i, j, 0))
                    .is_some()
                {
                    duplicates.insert(row.conversation_id.clone());
                    state.members.remove(&row.conversation_id);
                }
            }
        }
    }
}

/// Read and project exactly one folder member off the event loop.
pub(super) fn refresh_member(window: &MainWindow, client: &Arc<ChatterClient>, key: &str) {
    let request = with_members(window, client, |state| state.begin(key));
    let Some((epoch, revision, conversation)) = request else {
        // A pending read will drain the dirty key; only a missing structural
        // index needs the exceptional full snapshot fallback.
        if !with_members(window, client, |state| {
            state.snapshot_pending || state.members.contains_key(key)
        }) {
            super::refresh_section_badges(window, client);
        }
        return;
    };
    let weak = window.as_weak();
    let session = client.session_guard();
    let client = client.clone();
    tokio::spawn(async move {
        let key = conversation.conversation_id.clone();
        let result = client.refresh_section_row_async(conversation).await;
        run_ui!(weak, |w| {
            if !session.is_current() || !owns_members(&w, &client) {
                return;
            }
            let (valid, dirty) = with_members(&w, &client, |state| state.finish(epoch, &key));
            if !valid {
                return;
            }
            // Reads are serialized per member, so this result is newer than
            // its last publication. Paint it even during sustained traffic,
            // then drain the latest dirty state without starving the preview.
            if let Ok((metadata, row, contribution)) = result {
                apply_member(
                    &w,
                    &client,
                    epoch,
                    revision,
                    metadata,
                    row,
                    contribution as i32,
                );
            }
            if dirty {
                refresh_member(&w, &client, &key);
            }
        });
    });
}

fn apply_member(
    window: &MainWindow,
    client: &Arc<ChatterClient>,
    epoch: u64,
    revision: u64,
    metadata: chatter_core::domain::section::SectionConversation,
    data: ChatRowSendData,
    contribution: i32,
) {
    with_members(window, client, |state| {
        if state.epoch != epoch {
            return;
        }
        let Some(&(section_index, from, current)) = state.members.get(&metadata.conversation_id)
        else {
            return;
        };
        if current != revision {
            return;
        }
        let pane = window.global::<crate::SidebarPane>();
        let sections = pane.get_custom_sections();
        let Some(mut section) = sections.row_data(section_index) else {
            return;
        };
        let rows = section.conversations.clone();
        let Some(vec) = rows.as_any().downcast_ref::<VecModel<ChatRowData>>() else {
            return;
        };
        let Some(old) = vec.row_data(from) else {
            return;
        };
        let mut row = data.to_slint();
        row.is_selected = super::row_is_active(
            &client.active_conversation(),
            row.conversation_type,
            row.conversation_id.as_str(),
        );
        row.arrival_serial = old.arrival_serial;
        let mode = SectionSortMode::from_str_value(section.sort_mode.as_str());
        let MemberIndex {
            sections: ordering,
            members,
            ..
        } = &mut *state;
        let ordered = &mut ordering[section_index];
        let to = chatter_core::services::section_service::replace_section_conversation(
            ordered, from, metadata, mode,
        );
        // Vec storage shifts this range; update only its integer positions.
        for i in from.min(to)..=from.max(to) {
            if let Some(member) = members.get_mut(&ordered[i].conversation_id) {
                member.1 = i;
            }
        }
        let previous = state
            .contributions
            .insert(row.conversation_id.to_string(), contribution)
            .unwrap_or(0);
        section.unread_count = section.unread_count - previous + contribution;
        if from == to {
            vec.set_row_data(to, row.clone());
        } else {
            vec.remove(from);
            vec.insert(to, row.clone());
        }
        sections.set_row_data(section_index, section.clone());
        if pane.get_archive_open() {
            return;
        }
        let header = super::SIDEBAR_INDEX.with(|index| {
            index
                .borrow()
                .section_headers
                .get(section.section_id.as_str())
                .copied()
        });
        let Some(header) = header else { return };
        let flat = pane.get_flat_rows();
        let Some(flat_vec) = flat
            .as_any()
            .downcast_ref::<VecModel<crate::SidebarRowData>>()
        else {
            return;
        };
        patch_member_projection(flat_vec, header, section, from, to, row);
    });
}

/// Patch only the folder header and the changed visible child. Child entries
/// share the retained conversation model; their section metadata is structural.
fn patch_member_projection(
    flat_vec: &VecModel<crate::SidebarRowData>,
    header: usize,
    section: CustomSectionData,
    from: usize,
    to: usize,
    row: ChatRowData,
) {
    if let Some(mut entry) = flat_vec.row_data(header) {
        entry.section = section.clone();
        flat_vec.set_row_data(header, entry);
    }
    if section.is_expanded {
        let source = header + 1 + from;
        let target = header + 1 + to;
        if let Some(mut entry) = flat_vec.row_data(source) {
            entry.chat = row;
            entry.section = section;
            if source == target {
                flat_vec.set_row_data(target, entry);
            } else {
                flat_vec.remove(source);
                flat_vec.insert(target, entry);
            }
        }
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;

    struct Changes(std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>);
    impl i_slint_core::model::ModelChangeListener for Changes {
        fn row_changed(self: std::pin::Pin<&Self>, _: usize) {
            self.0.borrow_mut().push("change");
        }
        fn row_added(self: std::pin::Pin<&Self>, _: usize, _: usize) {
            self.0.borrow_mut().push("add");
        }
        fn row_removed(self: std::pin::Pin<&Self>, _: usize, _: usize) {
            self.0.borrow_mut().push("remove");
        }
        fn reset(self: std::pin::Pin<&Self>) {
            self.0.borrow_mut().push("reset");
        }
    }

    fn entry(key: &str) -> crate::SidebarRowData {
        crate::SidebarRowData {
            key: key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn bursts_have_one_in_flight_read_and_structural_changes_reject_old_results() {
        let metadata = chatter_core::domain::section::SectionConversation {
            conversation_id: "peer".into(),
            conversation_type: chatter_core::domain::message::ConversationType::ServerDm,
            display_name: "Peer".into(),
            unread_count: 0,
            last_message_at: None,
            is_pinned: false,
        };
        let mut index = MemberIndex::default();
        index.sections.push(vec![metadata]);
        index.members.insert("peer".into(), (0, 0, 0));
        let (epoch, _, _) = index.begin("peer").unwrap();
        for _ in 0..1000 {
            assert!(index.begin("peer").is_none());
        }
        assert_eq!(index.in_flight.len(), 1);
        assert_eq!(index.dirty.len(), 1);
        assert_eq!(index.finish(epoch, "peer"), (true, true));
        assert!(index.begin("peer").is_some());
        index.epoch += 1;
        assert_eq!(index.finish(epoch, "peer"), (false, false));
        assert!(index.in_flight.contains("peer"));
        assert!(index.begin("missing").is_none());
    }

    #[test]
    fn snapshot_admission_retires_members_and_old_completion_cannot_clear_new_work() {
        let mut index = MemberIndex::default();
        index.members.insert("peer".into(), (0, 0, 0));
        index
            .sections
            .push(vec![chatter_core::domain::section::SectionConversation {
                conversation_id: "peer".into(),
                conversation_type: chatter_core::domain::message::ConversationType::ServerDm,
                display_name: "Peer".into(),
                unread_count: 0,
                last_message_at: None,
                is_pinned: false,
            }]);
        let (member, _, _) = index.begin("peer").unwrap();
        let old = index.begin_snapshot();
        let newer = index.begin_snapshot();
        assert_eq!(index.finish_snapshot(old), None);
        assert!(index.snapshot_pending);
        assert_eq!(index.finish(member, "peer"), (false, false));
        assert!(index.in_flight.is_empty());
        assert_eq!(index.finish_snapshot(newer), Some(Default::default()));
        // Also used for a failed current read: pending ownership is released,
        // so incremental updates and a later full request can proceed.
        assert!(index.begin("peer").is_some());
        let retry = index.begin_snapshot();
        assert_eq!(index.finish_snapshot(retry), Some(Default::default()));
        assert!(index.begin("peer").is_some());
    }

    #[test]
    fn sustained_members_do_not_starve_structural_publication() {
        let mut index = MemberIndex::default();
        let epoch = index.begin_snapshot();
        for _ in 0..1000 {
            assert!(index.begin("peer").is_none());
        }
        assert_eq!(index.snapshot_dirty.len(), 1);
        let dirty = index.finish_snapshot(epoch).unwrap();
        assert_eq!(dirty.into_iter().collect::<Vec<_>>(), ["peer"]);
        assert!(!index.snapshot_pending);
        assert!(index.snapshot_dirty.is_empty());
    }

    #[test]
    fn duplicate_memberships_fall_back_without_hiding_either_folder() {
        let member = chatter_core::domain::section::SectionConversation {
            conversation_id: "duplicate".into(),
            conversation_type: chatter_core::domain::message::ConversationType::ServerDm,
            display_name: "Peer".into(),
            unread_count: 0,
            last_message_at: None,
            is_pinned: false,
        };
        let section = || chatter_core::domain::section::CustomSection {
            id: SectionId::new(),
            name: "Folder".into(),
            icon_emoji: "".into(),
            sort_order: 0,
            is_expanded: true,
            sort_mode: SectionSortMode::Recent,
            conversations: vec![member.clone()],
            unread_count: 0,
        };
        let mut state = MemberIndex::default();
        index_members(&mut state, &[section(), section()], Default::default());
        {
            assert!(state.begin("duplicate").is_none());
            assert_eq!(state.sections.len(), 2);
            assert_eq!(state.sections[0].len(), 1);
            assert_eq!(state.sections[1].len(), 1);
        }
    }

    #[test]
    fn paused_structural_snapshots_reject_old_structure_and_drain_dirty_members() {
        const MARKER: &str = "CHATTER_FOLDER_SNAPSHOT_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let name = format!(
                "{}::paused_structural_snapshots_reject_old_structure_and_drain_dirty_members",
                module_path!().split_once("::").unwrap().1
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture", "--test-threads=1"])
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
            return;
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _entered = rt.enter();
        let fixture = tempfile::tempdir().unwrap();
        let db = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
            .unwrap()
            .database_path();
        std::fs::write(
            db.parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(fixture.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let w = MainWindow::new().unwrap();
        let other = MainWindow::new().unwrap();
        let section = chatter_core::domain::section::CustomSection {
            id: SectionId::new(),
            name: "Before".into(),
            icon_emoji: "".into(),
            sort_order: 0,
            is_expanded: true,
            sort_mode: SectionSortMode::Recent,
            conversations: vec![],
            unread_count: 0,
        };
        let publish =
            |window: &MainWindow,
             epoch,
             sections: Vec<chatter_core::domain::section::CustomSection>| {
                let data = build_section_data(&sections, &client);
                let contributions = sections
                    .iter()
                    .flat_map(|s| &s.conversations)
                    .map(|c| {
                        (
                            c.conversation_id.clone(),
                            client.section_member_unread_badge(c) as i32,
                        )
                    })
                    .collect();
                publish_snapshot(window, &client, epoch, Ok((sections, data, contributions)));
            };
        // S has read expanded=true; pause its completion while collapse T publishes.
        let old = with_members(&w, &client, MemberIndex::begin_snapshot);
        let (resume, paused) = std::sync::mpsc::channel();
        let stale = section.clone();
        let worker = std::thread::spawn(move || {
            paused.recv().unwrap();
            vec![stale]
        });
        invalidate_structure(&w, &client);
        let newer = with_members(&w, &client, MemberIndex::begin_snapshot);
        let mut collapsed = section.clone();
        collapsed.is_expanded = false;
        collapsed.name = "After".into();
        publish(&w, newer, vec![collapsed]);
        resume.send(()).unwrap();
        publish(&w, old, worker.join().unwrap());
        let shown = w
            .global::<crate::SidebarPane>()
            .get_custom_sections()
            .row_data(0)
            .unwrap();
        assert!(!shown.is_expanded);
        assert_eq!(shown.name, "After");
        // A separate window's admission cannot revoke this window's pending read.
        let current = with_members(&w, &client, MemberIndex::begin_snapshot);
        with_members(&other, &client, MemberIndex::begin_snapshot);
        publish(&w, current, vec![section.clone()]);
        assert!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_data(0)
                .unwrap()
                .is_expanded
        );
        // Incoming members during a paused read are retained without starving
        // publication, including unknown/newly moved members.
        let pending = with_members(&w, &client, MemberIndex::begin_snapshot);
        assert!(with_members(&w, &client, |s| s.begin("new-member")).is_none());
        publish(&w, pending, vec![section.clone()]);
        assert_eq!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_count(),
            1
        );
        let latest = with_members(&w, &client, MemberIndex::begin_snapshot);
        let mut unread = section.clone();
        unread
            .conversations
            .push(chatter_core::domain::section::SectionConversation {
                conversation_id: "new-member".into(),
                conversation_type: chatter_core::domain::message::ConversationType::ServerDm,
                display_name: "Peer".into(),
                unread_count: 9,
                last_message_at: None,
                is_pinned: false,
            });
        publish(&w, latest, vec![unread]);
        assert_eq!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_data(0)
                .unwrap()
                .unread_count,
            9
        );
        // Continuous arrivals still permit the structural result to publish,
        // and exactly one member read drains their retained key afterward.
        let burst = with_members(&w, &client, MemberIndex::begin_snapshot);
        for _ in 0..1000 {
            refresh_member(&w, &client, "new-member");
        }
        let badge_sections = with_members(&w, &client, |s| s.sections.clone());
        let mut burst_section = section.clone();
        burst_section.conversations = badge_sections[0].clone();
        publish(&w, burst, vec![burst_section]);
        assert!(with_members(&w, &client, |s| !s.snapshot_pending
            && s.in_flight.contains("new-member")));
        assert_eq!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_data(0)
                .unwrap()
                .unread_count,
            9
        );
        let member_epoch = with_members(&w, &client, |s| s.epoch);
        let structural = with_members(&w, &client, MemberIndex::begin_snapshot);
        assert_eq!(
            with_members(&w, &client, |s| s.finish(member_epoch, "new-member")),
            (false, false)
        );
        // A newer reorder/removal survives the paused predecessor as well.
        let mut moved = section.clone();
        moved.id = SectionId::new();
        moved.name = "Moved".into();
        let newest = with_members(&w, &client, MemberIndex::begin_snapshot);
        publish(&w, newest, vec![moved.clone(), section.clone()]);
        publish(&w, structural, vec![section.clone()]);
        assert_eq!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_data(0)
                .unwrap()
                .section_id,
            moved.id.to_string()
        );
        let removal = with_members(&w, &client, MemberIndex::begin_snapshot);
        publish(&w, removal, vec![]);
        publish(&w, newest, vec![moved]);
        assert_eq!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_count(),
            0
        );
        // Same account/session replacement resets the retained index without
        // recycling the generation, so old completion cannot block new work.
        let retired = with_members(&w, &client, MemberIndex::begin_snapshot);
        // Queue production admission under the old session, then retire it
        // before the event loop executes that request.
        refresh_sections_ui(&w.as_weak(), &client);
        client.testing_invalidate_session();
        let replacement = with_members(&w, &client, MemberIndex::begin_snapshot);
        assert_ne!(retired, replacement);
        w.show().unwrap();
        slint::invoke_from_event_loop(|| slint::quit_event_loop().unwrap()).unwrap();
        slint::run_event_loop_until_quit().unwrap();
        assert_eq!(with_members(&w, &client, |s| s.epoch), replacement);
        assert!(with_members(&w, &client, |s| s.snapshot_pending));
        publish(&w, retired, vec![]);
        publish(&w, replacement, vec![]);
        assert_eq!(
            w.global::<crate::SidebarPane>()
                .get_custom_sections()
                .row_count(),
            0
        );
        queue_refresh(&w, &client);
        queue_refresh(&other, &client);
        assert!(with_members(&w, &client, |s| s.refresh_queued));
        assert!(with_members(&other, &client, |s| s.refresh_queued));
        assert!(with_members(&w, &client, |s| !s.snapshot_pending));
    }

    #[test]
    fn expanded_folder_patch_moves_only_its_child_and_updates_header() {
        let child_rows = ModelRc::new(VecModel::from(vec![ChatRowData::default(); 3]));
        let section = CustomSectionData {
            section_id: "folder".into(),
            is_expanded: true,
            unread_count: 7,
            conversations: child_rows.clone(),
            ..Default::default()
        };
        let flat = VecModel::from(vec![
            entry("loose"),
            entry("header"),
            entry("a"),
            entry("b"),
            entry("c"),
            entry("other-folder"),
        ]);
        let changes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observer = Box::pin(i_slint_core::model::ModelChangeListenerContainer::new(
            Changes(changes.clone()),
        ));
        flat.model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        let row = ChatRowData {
            conversation_id: "c".into(),
            unread_count: 7,
            is_pinned: true,
            is_selected: true,
            ..Default::default()
        };
        patch_member_projection(&flat, 1, section, 2, 0, row.clone());
        assert_eq!(
            flat.iter().map(|r| r.key.to_string()).collect::<Vec<_>>(),
            ["loose", "header", "c", "a", "b", "other-folder"]
        );
        // Slint Image(None) does not compare equal even to its clone; assert
        // the user-visible member state rather than opaque image equality.
        let moved = flat.row_data(2).unwrap().chat;
        assert_eq!(moved.conversation_id, row.conversation_id);
        assert_eq!(moved.unread_count, 7);
        assert!(moved.is_pinned && moved.is_selected);
        assert_eq!(&*changes.borrow(), &["change", "remove", "add"]);
        let header = flat.row_data(1).unwrap();
        assert_eq!(header.section.unread_count, 7);
        assert!(header.section.conversations == child_rows);
        // A badge-only follow-up keeps ordering, selection and pin metadata.
        patch_member_projection(&flat, 1, header.section, 0, 0, row);
        assert_eq!(flat.row_count(), 6);
        assert_eq!(flat.row_data(2).unwrap().key, "c");
        assert_eq!(flat.row_data(5).unwrap().key, "other-folder");
        assert_eq!(
            &*changes.borrow(),
            &["change", "remove", "add", "change", "change"]
        );
    }

    #[test]
    fn collapsed_folder_patch_never_materializes_hidden_children() {
        let flat = VecModel::from(vec![entry("header"), entry("next-folder")]);
        let section = CustomSectionData {
            unread_count: 4,
            is_expanded: false,
            ..Default::default()
        };
        patch_member_projection(&flat, 0, section, 99, 0, ChatRowData::default());
        assert_eq!(flat.row_count(), 2);
        assert_eq!(flat.row_data(0).unwrap().section.unread_count, 4);
        assert_eq!(flat.row_data(1).unwrap().key, "next-folder");
    }
}

/// Send-safe intermediate for one section and the rows it owns.
struct SectionSendData {
    section_id: String,
    name: String,
    icon_emoji: String,
    is_expanded: bool,
    unread_count: i32,
    sort_mode: String,
    conversations: Vec<ChatRowSendData>,
}

impl SectionSendData {
    fn to_slint(self) -> CustomSectionData {
        let rows: Vec<ChatRowData> = self
            .conversations
            .into_iter()
            .map(ChatRowExt::to_slint)
            .collect();
        CustomSectionData {
            section_id: self.section_id.into(),
            name: self.name.into(),
            icon_emoji: self.icon_emoji.into(),
            is_expanded: self.is_expanded,
            unread_count: self.unread_count,
            sort_mode: self.sort_mode.into(),
            conversations: ModelRc::new(VecModel::from(rows)),
        }
    }
}

fn section_error(weak: &slint::Weak<MainWindow>, action: &str, error: impl std::fmt::Display) {
    let message = format!("Couldn't {action}: {error}");
    run_ui!(weak, |w| {
        w.set_toast_message(message.into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
    });
}

/// Start the initial folder read off the event loop, as on later refreshes.
pub(crate) fn load_sections(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        refresh_sections_ui(&weak, &ui_ctx);
    });
}

/// Wire all section-related callbacks.
pub(crate) fn wire_section_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_create_section(window, ui_ctx);
    wire_delete_section(window, ui_ctx);
    wire_rename_section(window, ui_ctx);
    wire_toggle_expanded(window, ui_ctx);
    wire_move_to_section(window, ui_ctx);
    wire_remove_from_section(window, ui_ctx);
    wire_section_conversation_selected(window, ui_ctx);
    wire_move_section(window, ui_ctx);
    wire_set_sort_mode(window, ui_ctx);
    wire_set_conversation_pinned(window, ui_ctx);
}

fn wire_create_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::ModalHostState>().on_create_section(move |name: SharedString, emoji: SharedString, conv_id: SharedString, kind: crate::ConversationKind| {
        let name_str = name.to_string();
        let emoji_str = emoji.to_string();
        let conv_id_str = conv_id.to_string();
        let conversation = (!conv_id_str.is_empty())
            .then(|| crate::bin_ui::converters::conversation_type(kind));
        let moves_conversation = conversation.is_some();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        run_ui!(weak, |w| w.global::<crate::ModalHostState>().set_create_section_loading(true));

        if let Some(w) = weak_clone.upgrade() { invalidate_structure(&w, &ui_ctx); }
        tokio::spawn(async move {
            match ui_ctx.create_section(
                &name_str,
                &emoji_str,
                conversation.map(|kind| (conv_id_str.as_str(), kind)),
            ) {
                Ok(section) => {
                    tracing::info!(section_id = %section.id, name = %name_str, "Created custom section");
                    run_ui!(weak_clone, |w| {
                        w.global::<crate::ModalHostState>().set_create_section_loading(false);
                        crate::bin_ui::features::back::close(&w, "create-section");
                    });
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                    if moves_conversation {
                        super::rebuild_chat_list(&weak_clone, &ui_ctx);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create section");
                    run_ui!(weak_clone, |w| w.global::<crate::ModalHostState>().set_create_section_loading(false));
                    section_error(&weak_clone, "create folder", e);
                }
            }
        });
    });
}

fn wire_delete_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_delete_section_clicked(move |section_id: SharedString| {
        let section_id_str = section_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        if let Some(w) = weak_clone.upgrade() { invalidate_structure(&w, &ui_ctx); }
        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else {
                tracing::error!(section_id = %section_id_str, "Invalid section ID");
                section_error(&weak_clone, "delete folder", "invalid folder ID");
                return;
            };

            match ui_ctx.delete_section(&id) {
                Ok(()) => {
                    tracing::info!(section_id = %section_id_str, "Deleted custom section");
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                    // The folder's conversations become loose again.
                    super::rebuild_chat_list(&weak_clone, &ui_ctx);
                }
                Err(e) => {
                    tracing::error!(error = %e, section_id = %section_id_str, "Failed to delete section");
                    section_error(&weak_clone, "delete folder", e);
                }
            }
        });
    });
}

fn wire_rename_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    {
        let weak = window.as_weak();

        window
            .global::<crate::SidebarPane>()
            .on_rename_section_clicked(move |section_id: SharedString| {
                if let Some(w) = weak.upgrade() {
                    let sections = w.global::<crate::SidebarPane>().get_custom_sections();
                    if let Some(section) = sections.iter().find(|s| s.section_id == section_id) {
                        w.global::<crate::ModalHostState>()
                            .set_create_section_name(section.name);
                        w.global::<crate::ModalHostState>()
                            .set_create_section_emoji(section.icon_emoji);
                    }
                }
            });
    }

    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::ModalHostState>().on_rename_section(
        move |section_id: SharedString, name: SharedString, emoji: SharedString| {
            let section_id_str = section_id.to_string();
            let name_str = name.to_string();
            let emoji_str = emoji.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak_clone = weak.clone();

            if let Some(w) = weak_clone.upgrade() {
                invalidate_structure(&w, &ui_ctx);
            }
            tokio::spawn(async move {
                let Ok(id) = section_id_str.parse::<SectionId>() else {
                    section_error(&weak_clone, "rename folder", "invalid folder ID");
                    return;
                };

                if let Err(e) = ui_ctx.rename_section(&id, &name_str) {
                    tracing::error!(error = %e, "Failed to rename section");
                    section_error(&weak_clone, "rename folder", e);
                    return;
                }

                if let Err(e) = ui_ctx.change_section_icon(&id, &emoji_str) {
                    tracing::error!(error = %e, "Failed to change icon");
                    section_error(&weak_clone, "change folder icon", e);
                    return;
                }

                run_ui!(weak_clone, |w| crate::bin_ui::features::back::close(
                    &w,
                    "create-section"
                ));
                refresh_sections_ui(&weak_clone, &ui_ctx);
            });
        },
    );
}

fn wire_toggle_expanded(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window
        .global::<crate::SidebarPane>()
        .on_section_toggle_expanded(move |section_id: SharedString| {
            let section_id_str = section_id.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak_clone = weak.clone();

            if let Some(w) = weak_clone.upgrade() {
                invalidate_structure(&w, &ui_ctx);
            }
            tokio::spawn(async move {
                let Ok(id) = section_id_str.parse::<SectionId>() else {
                    section_error(&weak_clone, "expand folder", "invalid folder ID");
                    return;
                };

                if let Ok(sections) = ui_ctx.list_sections_with_conversations() {
                    if let Some(section) = sections.iter().find(|s| s.id == id) {
                        match ui_ctx.toggle_section_expanded(&id, !section.is_expanded) {
                            Ok(()) => refresh_sections_ui(&weak_clone, &ui_ctx),
                            Err(e) => section_error(&weak_clone, "expand folder", e),
                        }
                    } else {
                        section_error(&weak_clone, "expand folder", "folder not found");
                    }
                } else {
                    section_error(&weak_clone, "expand folder", "folder list unavailable");
                }
            });
        });
}

/// Move a folder one place up or down.
///
/// `SectionService::reorder_sections` was fully implemented with zero callers —
/// the back half of a drag-to-reorder nobody finished. This is its caller.
/// Deliberately not drag-and-drop: nothing in this UI has ever had a DropArea,
/// and dragging a row inside a scrolling list is the worst gesture to debut on
/// a phone. Two menu entries do the same job with no new gesture vocabulary.
fn wire_move_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_move_section(move |section_id: SharedString, delta: i32| {
        let section_id_str = section_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        if let Some(w) = weak_clone.upgrade() { invalidate_structure(&w, &ui_ctx); }
        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else {
                tracing::error!(section_id = %section_id_str, "move section: unparseable section id");
                section_error(&weak_clone, "move folder", "invalid folder ID");
                return;
            };
            match ui_ctx.move_section(&id, delta) {
                Ok(()) => refresh_sections_ui(&weak_clone, &ui_ctx),
                Err(e) => {
                    tracing::error!(error = %e, section_id = %section_id_str, delta, "Failed to move section");
                    section_error(&weak_clone, "move folder", e);
                }
            }
        });
    });
}

/// Choose how one folder orders the conversations inside it.
fn wire_set_sort_mode(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_set_section_sort_mode(move |section_id: SharedString, mode: SharedString| {
        let section_id_str = section_id.to_string();
        let mode = SectionSortMode::from_str_value(mode.as_str());
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        if let Some(w) = weak_clone.upgrade() { invalidate_structure(&w, &ui_ctx); }
        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else {
                section_error(&weak_clone, "sort folder", "invalid folder ID");
                return;
            };
            match ui_ctx.set_section_sort_mode(&id, mode) {
                Ok(()) => refresh_sections_ui(&weak_clone, &ui_ctx),
                Err(e) => {
                    tracing::error!(error = %e, section_id = %section_id_str, "Failed to set section sort mode");
                    section_error(&weak_clone, "sort folder", e);
                }
            }
        });
    });
}

/// Pin or unpin a conversation. Local-only by design — no server call.
fn wire_set_conversation_pinned(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_set_conversation_pinned(move |conv_id: SharedString, pinned: bool| {
        let conv_id_str = conv_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        if let Some(w) = weak_clone.upgrade() { invalidate_structure(&w, &ui_ctx); }
        tokio::spawn(async move {
            if !ui_ctx.set_conversation_pinned(&conv_id_str, pinned) {
                section_error(&weak_clone, "pin chat", "storage unavailable");
                return;
            }
            tracing::info!(conversation_id = %conv_id_str, pinned, "conversation pin toggled (local only)");
            // Pin order is core's; every list that renders it has to be rebuilt.
            refresh_sections_ui(&weak_clone, &ui_ctx);
            super::rebuild_chat_list(&weak_clone, &ui_ctx);
        });
    });
}

fn wire_move_to_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_move_to_section(move |conv_id: SharedString, kind: crate::ConversationKind, section_id: SharedString| {
        let conv_id_str = conv_id.to_string();
        let conv_type = crate::bin_ui::converters::conversation_type(kind);
        let section_id_str = section_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        if let Some(w) = weak_clone.upgrade() { invalidate_structure(&w, &ui_ctx); }
        tokio::spawn(async move {
            let Ok(sid) = section_id_str.parse::<SectionId>() else {
                tracing::error!(section_id = %section_id_str, "move to folder: unparseable section id");
                section_error(&weak_clone, "move chat to folder", "invalid folder ID");
                return;
            };
            match ui_ctx.move_conversation_to_section(&conv_id_str, conv_type, &sid) {
                Ok(()) => {
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                    super::rebuild_chat_list(&weak_clone, &ui_ctx);
                }
                Err(e) => {
                    tracing::error!(
                        error = %e, conversation_id = %conv_id_str, section_id = %section_id_str,
                        "Failed to move conversation into folder"
                    );
                    section_error(&weak_clone, "move chat to folder", e);
                }
            }
        });
    });
}

fn wire_remove_from_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window
        .global::<crate::SidebarPane>()
        .on_remove_from_section(move |conv_id: SharedString| {
            let conv_id_str = conv_id.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak_clone = weak.clone();

            if let Some(w) = weak_clone.upgrade() {
                invalidate_structure(&w, &ui_ctx);
            }
            tokio::spawn(async move {
                match ui_ctx.remove_conversation_from_section(&conv_id_str) {
                    Ok(()) => {
                        refresh_sections_ui(&weak_clone, &ui_ctx);
                        super::rebuild_chat_list(&weak_clone, &ui_ctx);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, conversation_id = %conv_id_str,
                            "Failed to remove conversation from folder");
                        section_error(&weak_clone, "remove chat from folder", e);
                    }
                }
            });
        });
}

fn wire_section_conversation_selected(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();

    window
        .global::<crate::SidebarPane>()
        .on_section_conversation_selected(
            move |conv_id: SharedString, kind: crate::ConversationKind| {
                use crate::ConversationKind as K;
                let conv_id_str = conv_id.to_string();
                run_ui!(weak, |w| {
                    match kind {
                        K::Channel | K::SavedMessages => w
                            .global::<crate::SidebarPane>()
                            .invoke_channel_selected(conv_id_str.into()),
                        K::ServerDm | K::AnonDm => w
                            .global::<crate::SidebarPane>()
                            .invoke_dm_selected(conv_id_str.into()),
                        K::AnonGroup => w
                            .global::<crate::SidebarPane>()
                            .invoke_private_group_selected(conv_id_str.into()),
                        // Mesh conversations are never filed into folders.
                        K::Mesh | K::MeshDm => {}
                    }
                });
            },
        );
}

/// Rebuild the folder models. Admit on the UI thread, read on the runtime,
/// then validate ownership before applying the model swap. The rebuild clears every row's
/// selection flag, so the open conversation is re-marked after it.
pub(crate) fn refresh_sections_ui(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let session = ui_ctx.session_guard();
    let ui_ctx = ui_ctx.clone();
    // Admit on the owning window's thread before starting any storage reads.
    run_ui!(weak, |w| {
        if !session.is_current() {
            return;
        }
        let epoch = with_members(&w, &ui_ctx, MemberIndex::begin_snapshot);
        let weak = w.as_weak();
        tokio::spawn(async move {
            if !session.is_current() {
                return;
            }
            let result = ui_ctx.list_sections_with_conversations().map(|sections| {
                let data = build_section_data(&sections, &ui_ctx);
                let contributions = sections
                    .iter()
                    .flat_map(|s| &s.conversations)
                    .map(|c| {
                        (
                            c.conversation_id.clone(),
                            ui_ctx.section_member_unread_badge(c) as i32,
                        )
                    })
                    .collect();
                (sections, data, contributions)
            });
            run_ui!(weak, |w| {
                if !session.is_current() {
                    return;
                }
                publish_snapshot(&w, &ui_ctx, epoch, result);
            });
        });
    });
}

fn publish_snapshot(
    window: &MainWindow,
    client: &Arc<ChatterClient>,
    epoch: u64,
    result: Result<
        (
            Vec<chatter_core::domain::section::CustomSection>,
            Vec<SectionSendData>,
            std::collections::HashMap<String, i32>,
        ),
        chatter_core::services::section_service::SectionError,
    >,
) {
    if !owns_members(window, client) {
        return;
    }
    let Some(dirty) = with_members(window, client, |state| state.finish_snapshot(epoch)) else {
        return;
    };
    // Publish current structure even during sustained member traffic. Those
    // keys are drained against the new index below, so repeated arrivals cannot
    // starve a structural refresh or disappear when its metadata is installed.
    if let Ok((sections, send_data, contributions)) = result {
        with_members(window, client, |state| {
            index_members(state, &sections, contributions)
        });
        apply_sections(
            window,
            send_data.into_iter().map(|s| s.to_slint()).collect(),
        );
        super::refresh_section_selection(window, client);
    }
    for key in dirty {
        refresh_member(window, client, &key);
    }
}

/// Retain each section and its conversation model across badge/selection
/// refreshes. Only a real section insertion or removal creates delegates.
fn apply_sections(window: &MainWindow, sections: Vec<CustomSectionData>) {
    let model = window.global::<crate::SidebarPane>().get_custom_sections();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<CustomSectionData>>() else {
        window
            .global::<crate::SidebarPane>()
            .set_custom_sections(ModelRc::new(VecModel::from(sections)));
        return;
    };
    let target_len = sections.len();
    let mut keys = (0..vec.row_count())
        .filter_map(|i| {
            vec.row_data(i)
                .map(|section| section.section_id.to_string())
        })
        .collect::<Vec<_>>();
    let mut positions = keys
        .iter()
        .enumerate()
        .map(|(i, key)| (key.clone(), i))
        .collect::<std::collections::HashMap<_, _>>();
    for (target, mut section) in sections.into_iter().enumerate() {
        let key = section.section_id.to_string();
        let existing = positions.get(&key).copied().filter(|&i| i >= target);
        if let Some(from) = existing {
            let old = vec.row_data(from).expect("located section");
            let rows = old.conversations.clone();
            if let Some(row_vec) = rows.as_any().downcast_ref::<VecModel<ChatRowData>>() {
                let new_rows: Vec<_> = section.conversations.iter().collect();
                super::reconcile_chat_rows(row_vec, new_rows, false);
                section.conversations = rows;
            }
            if from == target {
                if old.name != section.name
                    || old.icon_emoji != section.icon_emoji
                    || old.is_expanded != section.is_expanded
                    || old.unread_count != section.unread_count
                    || old.sort_mode != section.sort_mode
                {
                    vec.set_row_data(target, section);
                }
            } else {
                vec.remove(from);
                vec.insert(target, section);
                keys.remove(from);
                keys.insert(target, key.clone());
            }
        } else {
            vec.insert(target, section);
            keys.insert(target, key.clone());
        }
        for i in target..keys.len() {
            positions.insert(keys[i].clone(), i);
        }
    }
    while vec.row_count() > target_len {
        vec.remove(vec.row_count() - 1);
    }
}

fn build_section_data(
    sections: &[chatter_core::domain::section::CustomSection],
    ui_ctx: &Arc<ChatterClient>,
) -> Vec<SectionSendData> {
    let rows = ui_ctx.section_rows(sections);
    sections
        .iter()
        .zip(rows)
        .map(|(section, conversations)| SectionSendData {
            section_id: section.id.to_string(),
            name: section.name.clone(),
            icon_emoji: section.icon_emoji.clone(),
            is_expanded: section.is_expanded,
            unread_count: ui_ctx.section_unread_badge(section) as i32,
            sort_mode: section.sort_mode.as_str().to_string(),
            conversations,
        })
        .collect()
}
