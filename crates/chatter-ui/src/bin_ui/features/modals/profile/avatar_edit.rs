//! UI-thread staging; callbacks carry only immutable session/edit/request identity.
use crate::MainWindow;
use chatter_core::{
    client::ChatterClient,
    domain::{ids::UserId, user::Profile},
};
use slint::{ComponentHandle, ModelRc, SharedString};
use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Owner {
    session: u64,
    user: Option<UserId>,
}
impl Owner {
    fn read(ctx: &ChatterClient) -> Self {
        let auth = ctx.auth_state().get();
        Self {
            session: auth.session_revision,
            user: auth.current_user_id,
        }
    }
}
#[derive(Clone)]
struct Request {
    owner: Owner,
    edit: u64,
    operation: u64,
    alive: Arc<AtomicBool>,
}
#[derive(Default)]
struct Edit {
    owner: Option<Owner>,
    alive: Option<Arc<AtomicBool>>,
    epoch: u64,
    operation: u64,
    stack: Option<ModelRc<SharedString>>,
    staged: Option<Result<Vec<u8>, String>>,
    default_index: Option<usize>,
    picking: bool,
    saving: bool,
}
thread_local! { static EDIT: RefCell<Edit> = RefCell::new(Edit::default()); }

pub(super) fn retire() {
    EDIT.with(|state| {
        if let Some(alive) = &state.borrow().alive {
            alive.store(false, Ordering::SeqCst);
        }
        let epoch = state.borrow().epoch.wrapping_add(1);
        *state.borrow_mut() = Edit {
            epoch,
            ..Default::default()
        };
    });
}
fn open(w: &MainWindow, owner: Owner) {
    retire();
    let default_index = crate::bin_ui::converters::random_preset_avatar_index();
    w.set_edit_profile_display_name(w.get_profile_display_name());
    w.set_edit_profile_bio(w.get_profile_bio());
    w.set_edit_profile_avatar(w.get_user_avatar_source());
    w.set_edit_profile_has_photo(w.get_user_avatar_source().size().width > 0);
    w.set_edit_profile_selected_default_idx(default_index as i32);
    w.set_edit_profile_avatar_color(slint::Color::from_rgb_u8(15, 118, 110));
    w.set_edit_profile_error("".into());
    w.set_edit_profile_loading(false);
    crate::bin_ui::features::back::push(w, "edit-profile");
    EDIT.with(|s| {
        let mut s = s.borrow_mut();
        s.stack = Some(w.get_modal_stack());
        s.owner = Some(owner);
        s.default_index = Some(default_index);
    });
}
fn current(w: &MainWindow, owner: Owner, r: &Request) -> bool {
    r.alive.load(Ordering::SeqCst)
        && owner.user.is_some()
        && owner == r.owner
        && w.get_active_modal() == "edit-profile"
        && EDIT.with(|s| {
            let s = s.borrow();
            s.epoch == r.edit
                && s.operation == r.operation
                && s.stack.as_ref() == Some(&w.get_modal_stack())
        })
}
fn begin(w: &MainWindow, owner: Owner, save: bool) -> Option<(Request, Option<Vec<u8>>)> {
    if owner.user.is_none() || w.get_active_modal() != "edit-profile" {
        return None;
    }
    EDIT.with(|s| {
        let mut s = s.borrow_mut();
        if s.owner != Some(owner) || s.stack.as_ref() != Some(&w.get_modal_stack()) || s.saving {
            return None;
        }
        if save {
            let error = if s.picking {
                Some("Wait for the avatar picker to finish.".to_owned())
            } else {
                s.staged.as_ref().and_then(|v| v.as_ref().err()).cloned()
            };
            if let Some(error) = error {
                w.set_edit_profile_error(error.into());
                return None;
            }
        }
        s.operation = s.operation.wrapping_add(1);
        if let Some(alive) = &s.alive {
            alive.store(false, Ordering::SeqCst);
        }
        let alive = Arc::new(AtomicBool::new(true));
        s.alive = Some(alive.clone());
        let r = Request {
            owner,
            edit: s.epoch,
            operation: s.operation,
            alive,
        };
        s.saving = save;
        s.picking = !save;
        w.set_edit_profile_loading(save);
        w.set_edit_profile_error("".into());
        Some((r, s.staged.as_ref().and_then(|v| v.as_ref().ok()).cloned()))
    })
}
// The canonical normalizer bounds decoded dimensions/allocation and encoded output
// to MAX_AVATAR_SIZE (1,000,000 bytes). Bound raw file reads separately to 32 MiB.
fn read_avatar(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    const INPUT_CAP: u64 = 32 * 1024 * 1024;
    let file =
        std::fs::File::open(path).map_err(|_| "Unable to read selected avatar.".to_owned())?;
    let mut data = Vec::new();
    file.take(INPUT_CAP + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Unable to read selected avatar.".to_owned())?;
    if data.len() as u64 > INPUT_CAP {
        return Err("Choose an image smaller than 32 MiB.".into());
    }
    normalize(&data)
}
fn normalize(data: &[u8]) -> Result<Vec<u8>, String> {
    let bytes = chatter_core::media::ImageProcessor::process_avatar_from_memory(data)
        .map_err(|_| "Unable to use this avatar. Choose a valid, smaller image.".to_owned())?;
    // Server profile admission caps the encrypted blob at the anonymous wire
    // cap: AES-GCM adds a 12-byte nonce and 16-byte tag (profile_crypto).
    admit_wire_size(bytes.len())?;
    Ok(bytes)
}
fn admit_wire_size(size: usize) -> Result<(), String> {
    if size
        .checked_add(12 + 16)
        .is_none_or(|size| size > chatter_core::domain::anon_dm_init::MAX_AVATAR_SIZE)
    {
        return Err("Avatar is too large to share. Choose a smaller image.".into());
    }
    Ok(())
}

fn finish_picker(
    w: &MainWindow,
    owner: Owner,
    r: Request,
    result: Result<Option<Vec<u8>>, String>,
) {
    if !current(w, owner, &r) {
        return;
    }
    EDIT.with(|s| s.borrow_mut().picking = false);
    match result {
        Ok(None) => (), // Native cancellation preserves the previous staged choice.
        Ok(Some(bytes)) => match crate::bin_ui::slint_conversions::memory_to_slint_image(&bytes) {
            Ok(image) => {
                w.set_edit_profile_avatar(image);
                w.set_edit_profile_has_photo(true);
                w.set_edit_profile_error("".into());
                EDIT.with(|s| s.borrow_mut().staged = Some(Ok(bytes)));
            }
            Err(_) => reject_picker(
                w,
                "Unable to preview this avatar. Choose another image.".into(),
            ),
        },
        Err(error) => reject_picker(w, error),
    }
}
fn reject_picker(w: &MainWindow, error: String) {
    w.set_edit_profile_error(error.clone().into());
    EDIT.with(|s| s.borrow_mut().staged = Some(Err(error)));
}

fn select_default(w: &MainWindow, owner: Owner, index: usize) {
    if owner.user.is_none() || w.get_active_modal() != "edit-profile" {
        return;
    }
    let Some(bytes) =
        crate::bin_ui::converters::default_avatar_bytes(index, w.get_edit_profile_display_name().as_str())
    else {
        return;
    };
    EDIT.with(|state| {
        let mut state = state.borrow_mut();
        if state.owner != Some(owner)
            || state.stack.as_ref() != Some(&w.get_modal_stack())
            || state.saving
        {
            return;
        }
        if let Some(alive) = &state.alive {
            alive.store(false, Ordering::SeqCst);
        }
        state.operation = state.operation.wrapping_add(1);
        state.picking = false;
        state.default_index = Some(index);
        state.staged = Some(Ok(bytes));
        crate::bin_ui::features::avatar_defaults::show_edit_default(w, index);
        w.set_edit_profile_error("".into());
    });
}
fn finish_save(w: &MainWindow, owner: Owner, r: Request, result: Result<Profile, String>) {
    if !current(w, owner, &r) {
        return;
    }
    EDIT.with(|s| s.borrow_mut().saving = false);
    w.set_edit_profile_loading(false);
    match result {
        Err(error) => w.set_edit_profile_error(error.into()),
        Ok(profile) => {
            if Some(profile.user_id) != r.owner.user {
                w.set_edit_profile_error(
                    "Profile response belongs to another account. Retry.".into(),
                );
                return;
            }
            // Use persisted/returned normalized bytes, never the raw picker input.
            if let Some(bytes) = profile.avatar_bytes.as_deref() {
                match crate::bin_ui::slint_conversions::memory_to_slint_image(bytes) {
                    Ok(image) => {
                        w.set_user_avatar_source(image.clone());
                        w.set_profile_avatar_source(image.clone());
                        w.set_edit_profile_avatar(image);
                    }
                    Err(_) => {
                        w.set_edit_profile_error(
                            "Unable to display saved avatar. Try another image.".into(),
                        );
                        return;
                    }
                }
            }
            w.set_profile_display_name(profile.display_name.clone().into());
            w.set_profile_bio(
                profile
                    .status_message
                    .map(|m| m.text)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_user_initials(
                crate::bin_ui::converters::initials_from_name(&profile.display_name).into(),
            );
            w.set_profile_initials(w.get_user_initials());
            w.set_user_display_name(profile.display_name.into());
            crate::bin_ui::features::back::close(w, "edit-profile");
        }
    }
}

pub(super) fn wire(window: &MainWindow, ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ctx.clone();
    window.on_edit_profile_select_default_avatar(move |index| {
        if let Some(w) = weak.upgrade() {
            select_default(&w, Owner::read(&client), index.max(0) as usize);
        }
    });
    let weak = window.as_weak();
    let client = ctx.clone();
    window.on_edit_profile_remove_avatar(move || {
        if let Some(w) = weak.upgrade() {
            let index = EDIT
                .with(|s| s.borrow().default_index)
                .unwrap_or_else(crate::bin_ui::converters::random_preset_avatar_index);
            select_default(&w, Owner::read(&client), index);
        }
    });
    let weak = window.as_weak();
    let client = ctx.clone();
    window.on_profile_edit_profile(move || {
        if let Some(w) = weak.upgrade() {
            if Owner::read(&client).user.is_some() {
                open(&w, Owner::read(&client));
            }
        }
    });
    let weak = window.as_weak();
    let client = ctx.clone();
    window.on_edit_profile_change_avatar(move || {
        let Some(w) = weak.upgrade() else { return };
        let Some((request, _)) = begin(&w, Owner::read(&client), false) else {
            return;
        };
        let (weak, client) = (weak.clone(), client.clone());
        tokio::spawn(async move {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let picked = chatter_core::media::FilePicker::pick_file(
                chatter_core::media::FilePickerConfig::new()
                    .with_title("Select Profile Picture")
                    .with_filter(chatter_core::media::FileFilter::images()),
            )
            .await;
            #[cfg(target_os = "android")]
            let picked = crate::platform::android::file_picker::pick_files(&["image/*"])
                .await
                .into_iter()
                .next();
            #[cfg(target_os = "ios")]
            let picked: Option<std::path::PathBuf> = None;
            let result = match picked {
                Some(path) => tokio::task::spawn_blocking(move || read_avatar(&path))
                    .await
                    .map_err(|_| "Unable to process selected avatar.".to_owned())
                    .and_then(|r| r.map(Some)),
                None => Ok(None),
            };
            let _ = weak.upgrade_in_event_loop(move |w| {
                finish_picker(&w, Owner::read(&client), request, result)
            });
        });
    });
    let weak = window.as_weak();
    let client = ctx.clone();
    window.on_edit_profile_save(move |name, bio| {
        let Some(w) = weak.upgrade() else { return };
        let Some((request, mut bytes)) = begin(&w, Owner::read(&client), true) else {
            return;
        };
        if !w.get_edit_profile_has_photo() {
            if let Some(index) = EDIT.with(|s| s.borrow().default_index) {
                bytes = crate::bin_ui::converters::default_avatar_bytes(index, name.as_str());
            }
        }
        let (weak, client) = (weak.clone(), client.clone());
        let name = name.to_string();
        let bio = bio.to_string();
        tokio::spawn(async move {
            let result = client
                .save_own_profile_in_session(
                    request.owner.user.expect("admitted own-profile edit"),
                    request.owner.session,
                    request.alive.clone(),
                    bytes,
                    name,
                    (!bio.is_empty()).then(|| chatter_core::domain::user::StatusMessage::new(bio)),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = weak.upgrade_in_event_loop(move |w| {
                finish_save(&w, Owner::read(&client), request, result)
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png(color: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }
    fn bytes(w: &MainWindow) -> Vec<u8> {
        w.get_edit_profile_avatar()
            .to_rgba8()
            .unwrap()
            .as_bytes()
            .to_vec()
    }
    #[test]
    fn avatar_admission_normalizes_large_input_and_refuses_invalid_or_over_budget_files() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("owned.png");
        let mut input = png([0, 100, 200, 255]);
        // PNG trailing bytes are legal: raw input exceeds the wire cap but
        // canonical normalization produces the bounded image actually uploaded.
        input.resize(chatter_core::domain::anon_dm_init::MAX_AVATAR_SIZE + 1, 0);
        std::fs::write(&path, input).unwrap();
        let cap = chatter_core::domain::anon_dm_init::MAX_AVATAR_SIZE;
        assert!(admit_wire_size(cap - 28).is_ok());
        assert!(admit_wire_size(cap - 27).is_err());
        assert!(admit_wire_size(usize::MAX).is_err());
        let normalized = read_avatar(&path).unwrap();
        assert!(normalized.len() <= chatter_core::domain::anon_dm_init::MAX_AVATAR_SIZE);
        let encrypted =
            chatter_core::crypto::profile_crypto::encrypt_avatar(&[11; 32], &normalized).unwrap();
        assert_eq!(encrypted.len(), normalized.len() + 28);
        assert!(encrypted.len() <= cap);
        let decoded = image::load_from_memory(&normalized).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (512, 512));
        std::fs::write(&path, b"not an image").unwrap();
        assert!(read_avatar(&path).is_err());
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024 + 1).unwrap();
        assert!(read_avatar(&path).unwrap_err().contains("32 MiB"));
    }
    #[test]
    fn avatar_edit_controlled_completions_preserve_owner_and_paint_persisted_image() {
        // Slint backend/thread affinity is process-global; isolate from other
        // library fixtures, preserving the assertions in the child process.
        const MARKER: &str = "CHATTER_AVATAR_FIXTURE_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let test = format!(
                "{}::avatar_edit_controlled_completions_preserve_owner_and_paint_persisted_image",
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
            assert!(String::from_utf8_lossy(&output.stderr).contains("AVATAR_FIXTURE_COMPLETED"));
            return;
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let w = MainWindow::new().unwrap();
        let owner = Owner {
            session: 1,
            user: Some(UserId::new()),
        };
        let picked = normalize(&png([10, 20, 30, 255])).unwrap();
        let persisted = normalize(&png([200, 100, 50, 255])).unwrap();
        w.set_profile_display_name("Original fixture".into());
        open(&w, owner);
        let old = begin(&w, owner, false).unwrap().0;
        crate::bin_ui::features::back::pop(&w);
        assert!(!old.alive.load(Ordering::SeqCst));
        open(&w, owner);
        finish_picker(&w, owner, old, Ok(Some(picked.clone())));
        assert!(EDIT.with(|s| s.borrow().staged.is_none()));
        let first = begin(&w, owner, false).unwrap().0;
        let latest = begin(&w, owner, false).unwrap().0;
        finish_picker(&w, owner, first, Ok(Some(picked.clone())));
        assert!(EDIT.with(|s| s.borrow().staged.is_none()));
        assert!(
            begin(&w, owner, true).is_none(),
            "Save cannot race pending picker"
        );
        finish_picker(&w, owner, latest, Ok(Some(picked.clone())));
        let before = bytes(&w);
        let invalid = begin(&w, owner, false).unwrap().0;
        finish_picker(&w, owner, invalid, Err("invalid fixture image".into()));
        assert_eq!(bytes(&w), before);
        assert_eq!(w.get_edit_profile_error().as_str(), "invalid fixture image");
        assert!(
            begin(&w, owner, true).is_none(),
            "invalid selected image cannot silently save previous avatar"
        );
        let retry = begin(&w, owner, false).unwrap().0;
        finish_picker(&w, owner, retry, Ok(Some(picked.clone())));
        let (save, staged) = begin(&w, owner, true).unwrap();
        assert_eq!(staged.as_deref(), Some(picked.as_slice()));
        assert!(begin(&w, owner, true).is_none(), "duplicate Save refused");
        finish_save(&w, owner, save, Err("persist fixture refusal".into()));
        assert_eq!(w.get_active_modal().as_str(), "edit-profile");
        assert!(!w.get_edit_profile_loading());
        assert_eq!(
            EDIT.with(|s| s
                .borrow()
                .staged
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .clone()),
            picked
        );
        let save = begin(&w, owner, true).unwrap().0;
        crate::bin_ui::features::back::close(&w, "edit-profile");
        open(&w, owner);
        let mut profile = Profile::new(owner.user.unwrap(), "Saved fixture".into());
        profile.avatar_bytes = Some(persisted.clone());
        finish_save(&w, owner, save, Ok(profile.clone()));
        assert_eq!(w.get_active_modal().as_str(), "edit-profile");
        assert_eq!(w.get_user_display_name().as_str(), "User");
        let changed_session = Owner {
            session: 2,
            ..owner
        };
        assert!(
            begin(&w, changed_session, true).is_none(),
            "old edit cannot be saved by new session"
        );
        let picker = begin(&w, owner, false).unwrap().0;
        finish_picker(
            &w,
            changed_session,
            picker.clone(),
            Ok(Some(picked.clone())),
        );
        assert!(EDIT.with(|s| s.borrow().staged.is_none()));
        finish_picker(
            &w,
            Owner {
                user: Some(UserId::new()),
                ..owner
            },
            picker.clone(),
            Ok(Some(picked.clone())),
        );
        assert!(EDIT.with(|s| s.borrow().staged.is_none()));
        finish_picker(&w, owner, picker, Ok(Some(picked)));
        let save = begin(&w, owner, true).unwrap().0;
        finish_save(&w, changed_session, save.clone(), Ok(profile.clone()));
        assert!(w.get_edit_profile_loading());
        finish_save(&w, owner, save, Ok(profile));
        assert_ne!(w.get_active_modal().as_str(), "edit-profile");
        let expected = image::load_from_memory(&persisted)
            .unwrap()
            .to_rgba8()
            .into_raw();
        assert_eq!(
            w.get_user_avatar_source().to_rgba8().unwrap().as_bytes(),
            expected
        );
        assert_eq!(
            w.get_profile_avatar_source().to_rgba8().unwrap().as_bytes(),
            expected
        );
        assert_eq!(bytes(&w), expected);
        assert_eq!(w.get_user_display_name().as_str(), "Saved fixture");
        open(&w, owner);
        assert_eq!(
            bytes(&w),
            expected,
            "reopen uses refreshed persisted avatar"
        );
        let cancel = begin(&w, owner, false).unwrap().0;
        finish_picker(&w, owner, cancel, Ok(None));
        assert_eq!(bytes(&w), expected);
        eprintln!("AVATAR_FIXTURE_COMPLETED");
    }
}
