package com.chatter.client

import com.google.androidgamesdk.GameActivity

/**
 * Thin GameActivity host for the Slint/Rust client.
 *
 * The native side (`android_main`, GameActivity glue) lives in
 * `libchatter_ui.so`, loaded via the `android.app.lib_name` manifest meta-data.
 * This subclass exists so Phase 2 can hang OS-facing bridges off it —
 * runtime permission requests, Storage Access Framework / Photo Picker,
 * CameraX, and MediaProjection screen capture — and surface results to Rust
 * over JNI (the Rust side reaches back via `ndk-context`).
 */
class MainActivity : GameActivity()
