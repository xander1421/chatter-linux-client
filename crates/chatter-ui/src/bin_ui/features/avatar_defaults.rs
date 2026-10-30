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
                label: choice.label.into(),
                source,
                color: slint::Color::from_rgb_u8(choice.color[0], choice.color[1], choice.color[2]),
                initials: choice.image.is_none(),
            }
        })
        .collect();
    window
        .global::<crate::AuthFlow>()
        .set_avatar_options(ModelRc::from(Rc::new(VecModel::from(options))));
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
