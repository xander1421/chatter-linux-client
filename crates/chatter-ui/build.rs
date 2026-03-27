fn main() {
    slint_build::compile("../../ui/main.slint").unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

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
