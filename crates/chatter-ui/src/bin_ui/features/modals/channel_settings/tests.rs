use super::*;
use crate::bin_ui::converters::ChatRowExt;
use chatter_core::services::channel::ChannelError;
use chatter_core::storage::repositories::{
    channel_repo::ChannelRepository,
    settings_repo::{SettingsRepository, keys},
};

const CHILD_MARKER: &str = "CHATTER_UI_CHANNEL_DELETE_TEST_CHILD";
const FIXTURE_COMPLETED: &str = "channel deletion fixture completed";

fn run_isolated_delete_test(test_name: &str) {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Slint's integration backend may only be initialized once per process.
    // Re-run this exact test alone, preserving the inherited no-keyring env.
    // Files avoid pipe-buffer deadlocks even if the fixture logs heavily.
    let mut stdout = tempfile::tempfile().expect("create child stdout capture");
    let mut stderr = tempfile::tempfile().expect("create child stderr capture");
    let mut child = Command::new(std::env::current_exe().expect("locate UI test executable"))
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, test_name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .expect("spawn isolated channel deletion test");
    let deadline = Instant::now() + Duration::from_secs(45);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => break Err("child exceeded 45-second timeout".to_owned()),
            Err(error) => break Err(format!("polling child failed: {error}")),
        }
    };
    // Never signal another test or service; reap only the child we spawned.
    let cleanup = if result.is_err() {
        let killed = child.kill();
        let reaped = child.wait();
        format!("; child kill: {killed:?}; child wait: {reaped:?}")
    } else {
        String::new()
    };
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    stdout.read_to_string(&mut stdout_text).unwrap();
    stderr.read_to_string(&mut stderr_text).unwrap();
    assert!(
        matches!(&result, Ok(status) if status.success())
            && stderr_text.lines().any(|line| line == FIXTURE_COMPLETED),
        "isolated test {test_name}: {result:?}{cleanup}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}"
    );
}

#[test]
fn icon_picker_stages_bytes_preserves_drafts_and_rejects_obsolete_completions() {
    let test_name = format!(
        "{}::icon_picker_stages_bytes_preserves_drafts_and_rejects_obsolete_completions",
        module_path!().split_once("::").unwrap().1
    );
    if std::env::var(CHILD_MARKER).ok().as_deref() != Some(test_name.as_str()) {
        run_isolated_delete_test(&test_name);
        return;
    }
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var("CHATTER_DB_PASSPHRASE", "icon-picker-fixture");
        std::env::set_var("CHATTER_HEADLESS", "1");
    }
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    let w = MainWindow::new().unwrap();
    let id = ChannelId::new();
    client.select_conversation(ActiveConversation::Channel(id));
    show_channel_settings_shell(&w, &id.to_string(), "private");
    w.set_channel_settings_can_edit(true);
    w.set_channel_settings_name_draft("Unsaved name".into());
    w.set_channel_settings_description_draft("Unsaved description".into());
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let bytes = encoded.into_inner();
    let picked = || {
        Ok(Some((
            std::path::PathBuf::from("staged-icon.png"),
            bytes.clone(),
        )))
    };
    let first = begin_icon_request(&w, &client).unwrap();
    let latest = begin_icon_request(&w, &client).unwrap();
    finish_icon_request(&w, &client, first, picked());
    assert!(!w.get_channel_settings_has_icon_preview());
    finish_icon_request(&w, &client, latest, picked());
    assert!(w.get_channel_settings_has_icon_preview());
    ICON_STATE.with(|s| assert_eq!(s.borrow().staged.as_ref().unwrap().1, bytes));
    assert_eq!(w.get_channel_settings_name_draft().as_str(), "Unsaved name");
    assert_eq!(
        w.get_channel_settings_description_draft().as_str(),
        "Unsaved description"
    );

    // The path intentionally does not exist: Save must use the frozen bytes,
    // then show core's offline failure and preserve the editable draft.
    wire_save_settings(&w, &client);
    w.invoke_channel_settings_save("Unsaved name".into(), "Unsaved description".into());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while w.get_channel_settings_error().is_empty() && std::time::Instant::now() < deadline {
        slint::Timer::single_shot(std::time::Duration::from_millis(25), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
    }
    assert!(
        !w.get_channel_settings_error().is_empty(),
        "offline Save must report a failure"
    );
    assert!(
        !w.get_channel_settings_error()
            .contains("Couldn't read channel icon"),
        "Save must use staged bytes, not re-read the native cache file"
    );
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    assert_eq!(w.get_channel_settings_name_draft().as_str(), "Unsaved name");
    assert_eq!(
        w.get_channel_settings_description_draft().as_str(),
        "Unsaved description"
    );

    let cancelled = begin_icon_request(&w, &client).unwrap();
    finish_icon_request(&w, &client, cancelled, Ok(None));
    assert!(w.get_channel_settings_has_icon_preview());
    let invalid = begin_icon_request(&w, &client).unwrap();
    finish_icon_request(
        &w,
        &client,
        invalid,
        Ok(Some(("bad.png".into(), vec![1, 2, 3]))),
    );
    assert!(
        w.get_channel_settings_error()
            .contains("Couldn't open channel icon")
    );
    assert_eq!(w.get_channel_settings_name_draft().as_str(), "Unsaved name");
    ICON_STATE.with(|s| assert_eq!(s.borrow().staged.as_ref().unwrap().1, bytes));

    let dismissed = begin_icon_request(&w, &client).unwrap();
    crate::bin_ui::features::back::close(&w, "channel-settings");
    show_channel_settings_shell(&w, &id.to_string(), "private");
    w.set_channel_settings_can_edit(true);
    finish_icon_request(&w, &client, dismissed, picked());
    assert!(!w.get_channel_settings_has_icon_preview());

    // Even direct navigation that bypasses the shell changes stack identity.
    let reopened = begin_icon_request(&w, &client).unwrap();
    crate::bin_ui::features::back::close(&w, "channel-settings");
    crate::bin_ui::features::back::push(&w, "channel-settings");
    finish_icon_request(&w, &client, reopened, picked());
    assert!(!w.get_channel_settings_has_icon_preview());
    let switched = begin_icon_request(&w, &client).unwrap();
    client.select_conversation(ActiveConversation::Channel(ChannelId::new()));
    finish_icon_request(&w, &client, switched, picked());
    assert!(!w.get_channel_settings_has_icon_preview());
    client.select_conversation(ActiveConversation::Channel(id));
    let read_error = begin_icon_request(&w, &client).unwrap();
    finish_icon_request(&w, &client, read_error, Err("fixture read failed".into()));
    assert!(
        w.get_channel_settings_error()
            .contains("fixture read failed")
    );
    let old_session = begin_icon_request(&w, &client).unwrap();
    client.clear_composer_session();
    client.select_conversation(ActiveConversation::Channel(id));
    finish_icon_request(&w, &client, old_session, picked());
    assert!(!w.get_channel_settings_has_icon_preview());
    eprintln!("{FIXTURE_COMPLETED}");
}

#[test]
fn save_success_reloads_metadata_and_sidebar_and_rejects_replacement_owners() {
    let test_name = format!(
        "{}::save_success_reloads_metadata_and_sidebar_and_rejects_replacement_owners",
        module_path!().split_once("::").unwrap().1
    );
    if std::env::var(CHILD_MARKER).ok().as_deref() != Some(test_name.as_str()) {
        run_isolated_delete_test(&test_name);
        return;
    }
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var("CHATTER_DB_PASSPHRASE", "channel-save-fixture");
        std::env::set_var("CHATTER_HEADLESS", "1");
    }
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    let path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    let db =
        Arc::new(chatter_core::storage::Database::open(&path, "channel-save-fixture").unwrap());
    let me = UserId::new();
    SettingsRepository::new(db.clone())
        .set_string(keys::CURRENT_USER_ID, &me.to_string())
        .unwrap();
    let repo = ChannelRepository::new(db);
    let id = ChannelId::new();
    let mut channel = chatter_core::domain::channel::Channel::new(
        id,
        "Old name".into(),
        ChannelType::Private,
        me,
    );
    repo.insert(&channel).unwrap();
    repo.mark_former(&id, false).unwrap();
    let w = MainWindow::new().unwrap();
    client.select_conversation(ActiveConversation::Channel(id));
    show_channel_settings_shell(&w, &id.to_string(), "private");
    w.set_channel_settings_can_edit(true);
    w.set_channel_name("Old name".into());
    w.set_topic("Old description".into());
    let epoch = begin_settings_save(&w, &client).unwrap();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(3, 2)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    channel.name = "Saved name".into();
    channel.description = Some("Saved description".into());
    channel.icon = Some(encoded.into_inner());
    // Simulate the real service's confirmed local write, then call the
    // exact production Save-success handler; no transport/keyring/device.
    repo.update(&channel).unwrap();
    let complete = |epoch| {
        let rows = client.sidebar_rows();
        apply_saved_channel(
            &w,
            &client,
            epoch,
            crate::bin_ui::features::sidebar::channel::ChannelMetadata::from_channel(&channel),
            rows,
        );
    };
    complete(epoch);
    assert_eq!(w.get_channel_name().as_str(), "Saved name");
    assert_eq!(w.get_topic().as_str(), "Saved description");
    assert!(w.get_channel_has_avatar());
    assert_eq!(
        (
            w.get_channel_avatar_source().size().width,
            w.get_channel_avatar_source().size().height
        ),
        (3, 2)
    );
    let row = w
        .get_chat_rows()
        .iter()
        .find(|r| r.conversation_id.as_str() == id.to_string())
        .unwrap();
    assert_eq!(row.display_name.as_str(), "Saved name");
    assert!(row.has_avatar);
    assert_eq!(
        (
            row.avatar_source.size().width,
            row.avatar_source.size().height
        ),
        (3, 2)
    );
    assert_ne!(w.get_active_modal().as_str(), "channel-settings");

    show_channel_settings_shell(&w, &id.to_string(), "private");
    w.set_channel_settings_can_edit(true);
    let stale = begin_settings_save(&w, &client).unwrap();
    crate::bin_ui::features::back::close(&w, "channel-settings");
    crate::bin_ui::features::back::push(&w, "channel-settings");
    w.set_channel_name("Replacement modal".into());
    w.set_chat_rows(ModelRc::default());
    complete(stale);
    assert_eq!(w.get_channel_name().as_str(), "Replacement modal");
    assert_eq!(w.get_chat_rows().row_count(), 0);
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    let older = begin_settings_save(&w, &client).unwrap();
    let current = begin_settings_save(&w, &client).unwrap();
    complete(older);
    assert_eq!(w.get_chat_rows().row_count(), 0);
    client.select_conversation(ActiveConversation::Channel(ChannelId::new()));
    complete(current);
    assert_eq!(w.get_chat_rows().row_count(), 0);
    client.select_conversation(ActiveConversation::Channel(id));
    client.clear_composer_session();
    client.select_conversation(ActiveConversation::Channel(id));
    complete(current);
    assert_eq!(w.get_chat_rows().row_count(), 0);
    assert_eq!(w.get_channel_name().as_str(), "Replacement modal");
    eprintln!("{FIXTURE_COMPLETED}");
}

#[test]
fn delete_result_refreshes_rows_reports_errors_and_preserves_a_new_selection() {
    let test_name = format!(
        "{}::delete_result_refreshes_rows_reports_errors_and_preserves_a_new_selection",
        module_path!().split_once("::").unwrap().1
    );
    if std::env::var(CHILD_MARKER).ok().as_deref() != Some(test_name.as_str()) {
        run_isolated_delete_test(&test_name);
        return;
    }
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var("CHATTER_DB_PASSPHRASE", "channel-delete-fixture");
        std::env::set_var("CHATTER_HEADLESS", "1");
    }
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    let path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    let db =
        Arc::new(chatter_core::storage::Database::open(&path, "channel-delete-fixture").unwrap());
    let me = UserId::new();
    SettingsRepository::new(db.clone())
        .set_string(keys::CURRENT_USER_ID, &me.to_string())
        .unwrap();
    let repo = ChannelRepository::new(db);
    let deleted = ChannelId::new();
    let other = ChannelId::new();
    for (id, name) in [(deleted, "Delete fixture"), (other, "Keep fixture")] {
        repo.insert(&chatter_core::domain::channel::Channel::new(
            id,
            name.into(),
            ChannelType::Private,
            me,
        ))
        .unwrap();
        repo.mark_former(&id, false).unwrap();
    }
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.set_channel_settings_id(deleted.to_string().into());
    crate::bin_ui::features::back::push(&w, "channel-settings");
    w.set_channel_name("Delete fixture".into());
    client.select_conversation(ActiveConversation::Channel(deleted));
    w.set_chat_rows(ModelRc::new(VecModel::from(
        client
            .sidebar_rows()
            .into_iter()
            .map(|r| r.to_slint())
            .collect::<Vec<_>>(),
    )));
    assert_eq!(w.get_chat_rows().row_count(), 2);
    // Each round runs every UI closure queued so far and stops at its own
    // sentinel, queued behind them. Not `w.run()` with a quit timer: run's
    // hide queues a Quit (last window closed), and a timer firing after
    // its loop already exited leaves another, so stale Quits piled up at
    // the head of the queue and a later pump returned before the work it
    // waited for had run (flaky under load).
    w.show().unwrap();
    let pump = || {
        for _ in 0..4 {
            slint::invoke_from_event_loop(|| slint::quit_event_loop().unwrap()).unwrap();
            slint::run_event_loop().unwrap();
        }
    };
    finish_channel_delete(
        w.as_weak(),
        client.clone(),
        deleted,
        client.auth_snapshot().session_revision,
        SETTINGS_EPOCH.with(|epoch| epoch.get()),
        Err(ChannelError::NotAuthorized),
    );
    pump();
    assert_eq!(w.get_chat_rows().row_count(), 2);
    assert_eq!(
        client.active_conversation(),
        ActiveConversation::Channel(deleted)
    );
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    assert!(
        w.get_channel_settings_error()
            .contains("Couldn't delete channel")
    );
    assert!(w.get_toast_visible());
    assert!(w.get_mobile_show_chat());

    // Simulate the successful awaited service's local row removal. This
    // exercises the production result handler, without a server or device.
    repo.remove_confirmed_deleted_channel(&deleted).unwrap();
    finish_channel_delete(
        w.as_weak(),
        client.clone(),
        deleted,
        client.auth_snapshot().session_revision,
        SETTINGS_EPOCH.with(|epoch| epoch.get()),
        Ok(()),
    );
    pump();
    assert_eq!(w.get_chat_rows().row_count(), 1);
    assert_eq!(
        w.get_chat_rows()
            .row_data(0)
            .unwrap()
            .conversation_id
            .as_str(),
        other.to_string()
    );
    assert_eq!(client.active_conversation(), ActiveConversation::None);
    assert!(w.get_channel_name().is_empty());
    assert_eq!(w.get_messages().row_count(), 0);
    assert_ne!(w.get_active_modal().as_str(), "channel-settings");
    assert!(repo.get(&other).unwrap().is_some());
    assert!(!w.get_mobile_show_chat());
    assert_eq!(w.get_active_tab().as_str(), "chats");

    // A late result for the deleted channel must preserve a newly opened chat.
    client.select_conversation(ActiveConversation::Channel(other));
    w.set_mobile_show_chat(true);
    w.set_active_tab("contacts".into());
    w.set_channel_name("Keep fixture".into());
    w.set_channel_settings_id(other.to_string().into());
    crate::bin_ui::features::back::push(&w, "channel-settings");
    finish_channel_delete(
        w.as_weak(),
        client.clone(),
        deleted,
        client.auth_snapshot().session_revision,
        SETTINGS_EPOCH.with(|epoch| epoch.get()),
        Ok(()),
    );
    pump();
    assert_eq!(
        client.active_conversation(),
        ActiveConversation::Channel(other)
    );
    assert_eq!(w.get_channel_name().as_str(), "Keep fixture");
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    assert!(w.get_mobile_show_chat());
    assert_eq!(w.get_active_tab().as_str(), "contacts");
    // Equal UUIDs across kinds must not remove a replacement group.
    let group = AnonGroupId::from_uuid(*other.as_uuid());
    client.select_conversation(ActiveConversation::AnonGroup(group));
    crate::bin_ui::features::sidebar::conversation_removed(
        &w.as_weak(),
        &client,
        ActiveConversation::Channel(other),
    );
    pump();
    assert_eq!(
        client.active_conversation(),
        ActiveConversation::AnonGroup(group)
    );
    assert!(w.get_mobile_show_chat());
    assert_eq!(w.get_active_tab().as_str(), "contacts");

    // The exact open group reveals Chats and leaves its modal owner intact.
    crate::bin_ui::features::sidebar::conversation_removed(
        &w.as_weak(),
        &client,
        ActiveConversation::AnonGroup(group),
    );
    pump();
    assert_eq!(client.active_conversation(), ActiveConversation::None);
    assert!(!w.get_mobile_show_chat());
    assert_eq!(w.get_active_tab().as_str(), "chats");
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");

    // Desktop retains its current tab and empty detail pane.
    w.set_is_mobile_platform(false);
    w.set_active_tab("contacts".into());
    w.set_mobile_show_chat(true);
    client.select_conversation(ActiveConversation::Channel(other));
    crate::bin_ui::features::sidebar::conversation_removed(
        &w.as_weak(),
        &client,
        ActiveConversation::Channel(other),
    );
    pump();
    assert_eq!(client.active_conversation(), ActiveConversation::None);
    assert!(w.get_channel_name().is_empty());
    assert_eq!(w.get_active_tab().as_str(), "contacts");
    assert!(w.get_mobile_show_chat());

    // The same settings ID reopened after dispatch is a new screen.
    client.select_conversation(ActiveConversation::Channel(other));
    let old_epoch = SETTINGS_EPOCH.with(|epoch| epoch.get());
    show_channel_settings_shell(&w, &deleted.to_string(), "channel");
    finish_channel_delete(
        w.as_weak(),
        client.clone(),
        deleted,
        client.auth_snapshot().session_revision,
        old_epoch,
        Ok(()),
    );
    pump();
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    assert_eq!(
        client.active_conversation(),
        ActiveConversation::Channel(other)
    );

    // Session replacement retires an awaited delete and a queued removal.
    w.set_is_mobile_platform(true);
    w.set_channel_name("Replacement session".into());
    let old_session = client.auth_snapshot().session_revision;
    crate::bin_ui::features::sidebar::conversation_removed(
        &w.as_weak(),
        &client,
        ActiveConversation::Channel(other),
    );
    client.clear_composer_session();
    client.select_conversation(ActiveConversation::Channel(other));
    finish_channel_delete(
        w.as_weak(),
        client.clone(),
        other,
        old_session,
        SETTINGS_EPOCH.with(|epoch| epoch.get()),
        Ok(()),
    );
    pump();
    assert_eq!(
        client.active_conversation(),
        ActiveConversation::Channel(other)
    );
    assert_eq!(w.get_channel_name().as_str(), "Replacement session");
    assert!(w.get_mobile_show_chat());
    assert_eq!(w.get_active_tab().as_str(), "contacts");
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    eprintln!("{FIXTURE_COMPLETED}");
}
