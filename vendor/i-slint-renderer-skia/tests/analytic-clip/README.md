# Analytic clip checks

This standalone harness uses the actual renderer helper. It reads back only
synthetic white/gray shapes on an offscreen GPU surface. It opens no client
files, accounts, messages or screenshots, and retains no rendered content.

Host regressions:

```sh
cargo test --manifest-path vendor/i-slint-renderer-skia/tests/analytic-clip/Cargo.toml --target-dir target --lib
```

Device GPU regressions (select the intended device explicitly):

```sh
cargo ndk -t arm64-v8a -P 26 build --release --manifest-path vendor/i-slint-renderer-skia/tests/analytic-clip/Cargo.toml --target-dir target --bin chatter-clip-gpu-check
adb -s DEVICE push target/aarch64-linux-android/release/chatter-clip-gpu-check /data/local/tmp/chatter-clip-gpu-check
adb -s DEVICE shell chmod 755 /data/local/tmp/chatter-clip-gpu-check
adb -s DEVICE shell /data/local/tmp/chatter-clip-gpu-check
adb -s DEVICE shell rm /data/local/tmp/chatter-clip-gpu-check
```

The 36 cases cover uniform scale, fractional translations, duplicate/tile
rectangular clips, cuts through curved corners (original-path fallback), cuts
through the straight body, and photo background fill with a separate curved
rim. The simple clip comparison bounds average coverage differences and checks
that contour differences stay within one physical pixel. The fill/rim case
allows the coverage variation from eliminating duplicate antialiasing; it is
not a pixel-identity assertion. Corner geometry and interior coverage remain
constrained. These checks establish correctness boundaries, not scroll FPS.
