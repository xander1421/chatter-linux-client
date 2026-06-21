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

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
        getByName("release") {
            // Local dev: reuse the Android debug keystore (matches cargo-apk
            // history) so reinstalls don't wipe data. Replace for production.
            isMinifyEnabled = false
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
}
