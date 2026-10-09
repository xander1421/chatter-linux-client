plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Shared with build-android.sh; direct Gradle release tasks also need metadata.
val suppliedVersionCode = System.getenv("CHATTER_ANDROID_VERSION_CODE")
val suppliedVersionName = System.getenv("CHATTER_ANDROID_VERSION_NAME")
val productionRequested = System.getenv("RELEASE") == "1" ||
    gradle.startParameter.taskNames.any { it.contains("release", ignoreCase = true) }
require(!productionRequested || (!suppliedVersionCode.isNullOrEmpty() && !suppliedVersionName.isNullOrEmpty())) {
    "Release requires CHATTER_ANDROID_VERSION_CODE and CHATTER_ANDROID_VERSION_NAME"
}
val clientVersionCode = suppliedVersionCode ?: "1"
val clientVersionName = suppliedVersionName ?: "0.1.0"
require(clientVersionCode.matches(Regex("[1-9][0-9]{0,9}")) &&
    (clientVersionCode.toLongOrNull() ?: Long.MAX_VALUE) <= 2100000000L) {
    "CHATTER_ANDROID_VERSION_CODE must be 1..2100000000 (decimal, no leading zeros)"
}
require(clientVersionName.matches(Regex("[0-9][A-Za-z0-9._+-]{0,63}"))) {
    "CHATTER_ANDROID_VERSION_NAME must be 1..64 ASCII version characters, starting with a digit"
}

android {
    namespace = "com.chatter.client"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.chatter.client"
        minSdk = 26
        targetSdk = 35
        versionCode = clientVersionCode.toInt()
        versionName = clientVersionName
        // We only ship the Rust cdylib for arm64 today.
        ndk { abiFilters += "arm64-v8a" }
    }

    // Opt-in debug test identities share Rust/JNI staging and one Gradle run.
    // Ordinary builds retain their existing application ID and output paths.
    if (providers.gradleProperty("multiInstance").orNull == "true") {
        flavorDimensions += "instance"
        productFlavors {
            create("base") { dimension = "instance" }
            create("b") {
                dimension = "instance"
                applicationIdSuffix = ".b"
            }
            create("c") {
                dimension = "instance"
                applicationIdSuffix = ".c"
            }
        }
    }

    // Release signing comes from the environment (build-android.sh RELEASE=1);
    // the keystore lives outside the repository. A release task without all
    // four values fails here rather than producing an unsigned or debug-signed
    // APK; build-android.sh additionally rejects the debug certificate.
    val releaseKeystore = System.getenv("CHATTER_KEYSTORE")
    if (productionRequested) {
        val missing = listOf("CHATTER_KEYSTORE", "CHATTER_KEYSTORE_PASSWORD", "CHATTER_KEY_ALIAS", "CHATTER_KEY_PASSWORD")
            .filter { System.getenv(it).isNullOrEmpty() }
        require(missing.isEmpty()) { "Release requires signing inputs: ${missing.joinToString()}" }
        require(file(releaseKeystore!!).isFile) { "CHATTER_KEYSTORE is not a file" }
    }
    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = System.getenv("CHATTER_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("CHATTER_KEY_ALIAS")
                keyPassword = System.getenv("CHATTER_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
        getByName("release") {
            isMinifyEnabled = false
            if (releaseKeystore != null) signingConfig = signingConfigs.getByName("release")
        }
    }

    // The prebuilt libchatter_ui.so is produced by cargo-ndk into
    // src/main/jniLibs/arm64-v8a/ (see ../build-android.sh). It statically
    // links GStreamer/OpenSSL/Slint, so no other native libs are bundled.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    packaging {
        jniLibs {
            // Keep the .so uncompressed + page-aligned for direct mmap load.
            useLegacyPackaging = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    // Java GameActivity class — its native glue is compiled INTO libchatter_ui.so
    // by the android-activity `game-activity` feature. Do NOT enable prefab.
    implementation("androidx.games:games-activity:4.4.0")
    // GameActivity extends AppCompatActivity → needs an AppCompat theme + runtime.
    implementation("androidx.appcompat:appcompat:1.7.0")
    // CameraX (modern Camera2 stack) for call video capture — feeds I420 frames
    // to Rust/LiveKit. Not the deprecated android.hardware.Camera.
    implementation("androidx.camera:camera-core:1.4.1")
    implementation("androidx.camera:camera-camera2:1.4.1")
    implementation("androidx.camera:camera-lifecycle:1.4.1")
    // In-app QR scanner (QrScanner.kt): the preview view, and ZXing core to
    // decode — Apache-2.0, pure Java, no Google Play services.
    implementation("androidx.camera:camera-view:1.4.1")
    implementation("com.google.zxing:core:3.5.3")
    // LiveKit webrtc Java classes (livekit.org.webrtc / jni_zero). Sourced
    // in-place from the webrtc-sys download under target/ (version-locked to the
    // native side compiled into libchatter_ui.so), so the jar always matches the
    // .so and nothing is copied into or committed to the source tree. The
    // cargo-ndk step in build-android.sh produces it before gradle runs.
    // Exact same directory selected for the native build, including custom trees.
    val customWebrtc = System.getenv("LK_CUSTOM_WEBRTC")
    if (customWebrtc != null) {
        implementation(files("$customWebrtc/libwebrtc.jar"))
    } else {
        implementation(fileTree(mapOf(
            "dir" to (System.getenv("CARGO_TARGET_DIR") ?: "$projectDir/../../target"),
            "include" to listOf("**/livekit_webrtc/**/android-arm64-release/libwebrtc.jar"),
        )))
    }
}
