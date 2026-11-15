//! Adapt avatar choices to the shared Slint picker.
use crate::{AvatarOption, MainWindow};
use slint::ComponentHandle;
use slint::{ModelRc, VecModel};
use std::rc::Rc;

pub(crate) fn install(window: &MainWindow) {
    let options: Vec<AvatarOption> = crate::bin_ui::converters::DEFAULT_AVATARS
        .iter()
        .map(|choice| {
            let source = choice
                .image
                .and_then(|bytes| {
                    crate::bin_ui::slint_conversions::memory_to_slint_image(bytes).ok()
                })
                .unwrap_or_default();
            AvatarOption {
                label: option_label(choice.label).into(),
                source,
                color: slint::Color::from_rgb_u8(choice.color[0], choice.color[1], choice.color[2]),
                initials: choice.image.is_none(),
            }
        })
        .collect();
    window
        .global::<crate::AuthFlow>()
        .set_avatar_options(ModelRc::from(Rc::new(VecModel::from(options))));

    // The register carousel cycles only the photo-backed presets, which occupy
    // indices [0, count) of DEFAULT_AVATARS. The initials presets after them
    // would render a bare "?" before a display name is typed, so they are not
    // offered at registration (the profile editor still shows the full list).
    let preset_count = crate::bin_ui::converters::DEFAULT_AVATARS
        .iter()
        .filter(|choice| choice.image.is_some())
        .count() as i32;
    window
        .global::<crate::AuthFlow>()
        .set_register_preset_count(preset_count);
}

/// Accessible name of a preset. The English lives on `DEFAULT_AVATARS`; this
/// is the copy the picker reads aloud.
fn option_label(label: &str) -> String {
    match label {
        "Doge" => chatter_i18n::tr("Doge"),
        "Pepe" => chatter_i18n::tr("Pepe"),
        "Wojak" => chatter_i18n::tr("Wojak"),
        "Teal initials" => chatter_i18n::tr("Teal initials"),
        "Blue initials" => chatter_i18n::tr("Blue initials"),
        "Purple initials" => chatter_i18n::tr("Purple initials"),
        other => other.to_string(),
    }
}

pub(crate) fn show_register_default(window: &MainWindow, index: usize) {
    let Some(choice) = crate::bin_ui::converters::DEFAULT_AVATARS.get(index) else {
        return;
    };
    let source = choice
        .image
        .and_then(|bytes| crate::bin_ui::slint_conversions::memory_to_slint_image(bytes).ok())
        .unwrap_or_default();
    window
        .global::<crate::AuthFlow>()
        .set_register_avatar_source(source);
    window
        .global::<crate::AuthFlow>()
        .set_register_avatar_color(slint::Color::from_rgb_u8(
            choice.color[0],
            choice.color[1],
            choice.color[2],
        ));
    window
        .global::<crate::AuthFlow>()
        .set_register_selected_default_idx(index as i32);
    window
        .global::<crate::AuthFlow>()
        .set_register_has_avatar(false);
}

pub(crate) fn show_edit_default(window: &MainWindow, index: usize) {
    let Some(choice) = crate::bin_ui::converters::DEFAULT_AVATARS.get(index) else {
        return;
    };
    let source = choice
        .image
        .and_then(|bytes| crate::bin_ui::slint_conversions::memory_to_slint_image(bytes).ok())
        .unwrap_or_default();
    window
        .global::<crate::ModalHostState>()
        .set_edit_profile_avatar(source);
    window
        .global::<crate::ModalHostState>()
        .set_edit_profile_avatar_color(slint::Color::from_rgb_u8(
            choice.color[0],
            choice.color[1],
            choice.color[2],
        ));
    window
        .global::<crate::ModalHostState>()
        .set_edit_profile_selected_default_idx(index as i32);
    window
        .global::<crate::ModalHostState>()
        .set_edit_profile_has_photo(false);
}
