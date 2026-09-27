fn main() {
    // Build scripts do not receive cfg(test). Query-based UI suites opt in
    // explicitly; ordinary builds can optimize away empty rectangles.
    let debug_info = std::env::var_os("CARGO_FEATURE_UI_TEST_DEBUG").is_some();
    slint_build::compile_with_config(
        "../../ui/main.slint",
        slint_build::CompilerConfiguration::new().with_debug_info(debug_info),
    )
    .unwrap();
}
