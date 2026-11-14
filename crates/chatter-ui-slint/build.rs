fn main() {
    // Build scripts do not receive cfg(test). Query-based UI suites opt in
    // explicitly; ordinary builds can optimize away empty rectangles.
    let debug_info = std::env::var_os("CARGO_FEATURE_UI_TEST_DEBUG").is_some();
    slint_build::compile_with_config(
        "../../ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .with_debug_info(debug_info)
            // Every language but English lives in the chatter-i18n crate; its
            // catalogs are compiled into the UI (no gettext at runtime, so
            // Android and Windows work the same way). No per-component
            // context: one "Cancel" in the catalog, not one per screen.
            .with_bundled_translations("../chatter-i18n/translations")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .unwrap();
    println!("cargo:rerun-if-changed=../chatter-i18n/translations");
}
