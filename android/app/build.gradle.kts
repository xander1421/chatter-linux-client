plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.chatter.client"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.chatter.client"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
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
    // the keystore lives outside the repository. Without it, assembleRelease
    // yields only an unsigned APK, which build-android.sh refuses to ship.
    val releaseKeystore = System.getenv("CHATTER_KEYSTORE")
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
    implementation(
        fileTree(
            mapOf(
                "dir" to "$projectDir/../../target",
                "include" to listOf("**/livekit_webrtc/**/android-arm64-release/libwebrtc.jar"),
            ),
        ),
    )
}
