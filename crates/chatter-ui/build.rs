fn main() {
    // Keep the About page tied to the source revision compiled into this app.
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=CHATTER_BUILD_COMMIT={commit}");
    if let Ok(reference) = std::process::Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
    {
        if reference.status.success() {
            if let Ok(reference) = String::from_utf8(reference.stdout) {
                if let Ok(path) = std::process::Command::new("git")
                    .args(["rev-parse", "--git-path", reference.trim()])
                    .output()
                {
                    if path.status.success() {
                        if let Ok(path) = String::from_utf8(path.stdout) {
                            println!("cargo:rerun-if-changed={}", path.trim());
                        }
                    }
                }
            }
        }
    }
    // Enforce this for every Cargo release entry point, not just the APK script.
    if std::env::var_os("CARGO_FEATURE_DEV_AUTO_REGISTER").is_some()
        && std::env::var("PROFILE").as_deref() == Ok("release")
    {
        panic!("dev-auto-register uses publicly-known credentials and is forbidden in release builds; remove the feature");
    }

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

    // NDK build scripts always run on the HOST doing the cross-compile, not
    // the android target — the prebuilt LLVM toolchain directory name is
    // host-specific (linux-x86_64, darwin-x86_64, windows-x86_64), and was
    // hardcoded to linux-x86_64 (this project's Android builds had only ever
    // been done from Linux). Building from macOS linked against a
    // nonexistent linux-x86_64 path and failed at link time.
    let ndk_host = match std::env::consts::OS {
        "macos" => "darwin-x86_64",
        "windows" => "windows-x86_64",
        _ => "linux-x86_64",
    };

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
            "cargo:rustc-link-search=native={}/toolchains/llvm/prebuilt/{}/sysroot/usr/lib/{}/26",
            ndk, ndk_host, abi
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
            "cargo:rustc-link-arg={}/toolchains/llvm/prebuilt/{}/lib/clang/19/lib/linux/libclang_rt.builtins-{}-android.a",
            ndk, ndk_host, target_arch
        );
    }
}
