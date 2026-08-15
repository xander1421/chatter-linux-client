fn main() {
    slint_build::compile("../../ui/main.slint").unwrap();

    // Slint's `ElementHandle` query API is inert unless the .slint compiler
    // emitted debug info, and that is driven by an env var read at *build*
    // time. Without it the UI tests do not fail with "you forgot a flag" —
    // every query silently returns zero elements, so they fail as
    // "ThreadPanel was never instantiated" and "there are no Rectangles",
    // which reads like a broken UI rather than a wrong command line.
    // Forward the answer so a test can say what actually happened.
    println!("cargo:rerun-if-env-changed=SLINT_EMIT_DEBUG_INFO");
    println!(
        "cargo:rustc-env=CHATTER_UI_SLINT_DEBUG_INFO={}",
        u8::from(std::env::var_os("SLINT_EMIT_DEBUG_INFO").is_some())
    );

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_os == "macos" {
        // webrtc-sys links a static WebRTC lib containing Objective-C
        // categories (e.g. NSString+AbslStringView, adding
        // `stringForAbslStringView:`). Apple's linker drops object files
        // from a static archive that nothing directly references, and a
        // category isn't a direct reference — so the method silently
        // never registers and calling it crashes at runtime with
        // "unrecognized selector" instead of failing at link time.
        // -ObjC forces the linker to load every Objective-C object file
        // in linked archives. Same class of issue as the JNI symbol
        // stripping below: a dependency's rustc-link-arg does not reach
        // this crate's final binary link line, only this build.rs does.
        println!("cargo:rustc-link-arg=-ObjC");
    }

    if target_os != "android" {
        return;
    }

    // Calls: keep + export libwebrtc's Java_livekit_org_webrtc_* JNI symbols on
    // the FINAL cdylib link so the bundled webrtc Java classes resolve their
    // native methods. webrtc-sys's own build script emits these link args too,
    // but a dependency's rustc-link-arg does NOT reach the final cdylib — only
    // the crate being linked (this one) can. Without it the linker dead-strips
    // the symbols and the app aborts with UnsatisfiedLinkError on call join.
    match webrtc_sys_build::configure_jni_symbols() {
        Ok(()) => println!("cargo:warning=webrtc JNI symbols configured for export"),
        Err(e) => println!("cargo:warning=webrtc JNI symbol config failed: {e}"),
    }
    // configure_jni_symbols emits --undefined (prevents GC) + a version-script,
    // but rust's own cdylib version-script (`local: *;`) hides the symbols from
    // the dynamic table anyway. Force them in with an explicit export glob
    // (lld honours this over the wildcard local rule).
    println!("cargo:rustc-link-arg=-Wl,--export-dynamic-symbol=Java_livekit_org_webrtc_*");

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let ndk = std::env::var("ANDROID_NDK_HOME").unwrap_or_default();
    let gst_root = std::env::var("GSTREAMER_ROOT_ANDROID").unwrap_or_default();

    // cargo-apk defaults to API level 23 which lacks libaaudio.
    // Add NDK sysroot library path for min SDK 26 (AAudio introduced here).
    if !ndk.is_empty() {
        let abi = match target_arch.as_str() {
            "aarch64" => "aarch64-linux-android",
            "arm" => "arm-linux-androideabi",
            "x86_64" => "x86_64-linux-android",
            "x86" => "i686-linux-android",
            _ => "aarch64-linux-android",
        };
        println!(
            "cargo:rustc-link-search=native={}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{}/26",
            ndk, abi
        );
    }

    // GStreamer Android SDK bundles static OpenSSL which conflicts with
    // the NDK's BoringSSL (-lcrypto). Allow multiple definitions.
    println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");

    // GStreamer static libs need transitive deps linked explicitly.
    // Using link-arg ensures they appear AFTER rlibs (ordering matters).
    if !gst_root.is_empty() {
        let gst_arch = match target_arch.as_str() {
            "aarch64" => "arm64",
            "arm" => "armv7",
            "x86_64" => "x86_64",
            "x86" => "x86",
            _ => "arm64",
        };
        let gst_lib = format!("{}/{}/lib", gst_root, gst_arch);
        println!("cargo:rustc-link-search=native={}", gst_lib);
        println!("cargo:rustc-link-arg=-L{}", gst_lib);
        for lib in &["pcre2-8", "gmodule-2.0", "ffi", "orc-0.4", "iconv", "z"] {
            println!("cargo:rustc-link-arg=-l{}", lib);
        }
    }

    // libffi needs __clear_cache from compiler-rt builtins
    if !ndk.is_empty() {
        println!(
            "cargo:rustc-link-arg={}/toolchains/llvm/prebuilt/linux-x86_64/lib/clang/19/lib/linux/libclang_rt.builtins-{}-android.a",
            ndk, target_arch
        );
    }
}
