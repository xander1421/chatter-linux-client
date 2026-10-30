# RC71 profile Voice title fix

The shared-content profile renderer copied `SharedContentItemData.title` directly into its display model, exposing the recorded filename `Voice message.wav` in the Voice tab (reported evidence: `/home/alex/.cache/chatter-rc71/269cda8092217ece-dm-timer-settings-small.png`).

The UI now projects only the visible title through `shared_content_display_title`. For the existing `voice` category, it normalizes whitespace and underscores and delegates recognition to the existing `is_generic_voice_note_filename` attachment predicate, which handles ASCII case. Recognized generated labels render as `Voice message`. User-authored audio titles remain exact, and even the generic filename remains exact in non-voice categories. Attachment metadata/index filenames, export operations, message IDs, attachment IDs, and the original-message click route are unchanged.

Validation: two pure Rust title regressions passed using a temporary `rustc --test` harness extracted from the actual helper, tests, and existing core predicate/constant. Covered canonical/case/space/underscore generated labels, authored audio, and non-voice categories. Source checks confirmed title projection and unchanged message/attachment IDs; `git diff --check` passed. No Cargo, Gradle, APK, keyring, clipboard, or actual media operations ran. Core, protocol, storage, timers, menus, and main app.slint were not edited.

RC72 build/device verification remains with the coordinator's batch after the other physical findings settle.
