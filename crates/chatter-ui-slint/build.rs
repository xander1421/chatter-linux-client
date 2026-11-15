fn main() {
    // Build scripts do not receive cfg(test). Query-based UI suites opt in
    // explicitly; ordinary builds can optimize away empty rectangles.
    let debug_info = std::env::var_os("CARGO_FEATURE_UI_TEST_DEBUG").is_some();
    // Every language but English lives in the chatter-i18n crate; its
    // catalogs are compiled into the UI (no gettext at runtime, so Android
    // and Windows work the same way). No per-component context: one "Cancel"
    // in the catalog, not one per screen.
    let mut config = slint_build::CompilerConfiguration::new()
        .with_debug_info(debug_info)
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    // Slint refuses to bundle a directory that holds no language.
    let translations = std::path::Path::new("../chatter-i18n/translations");
    let has_language = std::fs::read_dir(translations)
        .is_ok_and(|entries| entries.flatten().any(|entry| entry.path().is_dir()));
    if has_language {
        config = config.with_bundled_translations(translations);
    }
    slint_build::compile_with_config("../../ui/main.slint", config).unwrap();
    println!("cargo:rerun-if-changed=../chatter-i18n/translations");
}
