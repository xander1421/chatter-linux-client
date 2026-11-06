# S9 native vsync lifecycle check

This runs the actual backported driver with Android's native ALooper and
Choreographer. Only AndroidAppWaker is replaced by a recording channel and tracing
is disabled. It reads no app/account data and generates no pixels.

From `linux-client`:

```sh
cargo ndk -t arm64-v8a -P 26 build --manifest-path vendor/i-slint-backend-android-activity/tests/native-vsync/Cargo.toml --target-dir target --release
adb -s DEVICE push target/aarch64-linux-android/release/chatter-native-vsync-check /data/local/tmp/chatter-native-vsync-check
adb -s DEVICE shell chmod 755 /data/local/tmp/chatter-native-vsync-check
adb -s DEVICE shell /data/local/tmp/chatter-native-vsync-check
adb -s DEVICE shell rm /data/local/tmp/chatter-native-vsync-check
```

The interval bounds intentionally target the 60 Hz S9. Adapt them before using
this check on a different refresh rate. Four cycles exercise active callbacks,
parked idle, restart, active shutdown and shutdown during helper startup. This is
not a test of the full Slint event loop or AndroidAppWaker; those require client
traces. Keep all app benchmarks sequential on a device.
