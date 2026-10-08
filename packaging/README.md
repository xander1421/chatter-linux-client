# Production client packaging

## Android

The ordinary `android/build-android.sh [install]` invocation remains a local debug
build with version code `1`, version name `0.1.0`, existing debug signer and dev
install/tunnel behavior. Override metadata with `CHATTER_ANDROID_VERSION_CODE`
and `CHATTER_ANDROID_VERSION_NAME`. Codes must be decimal 1..2100000000 without
leading zeros; names are 1..64 ASCII letters/digits or `.`, `_`, `+`, `-`, beginning
with a digit. Both the shell and Gradle validate supplied values.

For production, provide both explicitly, choose a code greater than the last
published code, and retain the production signing identity for upgrades:

```sh
RELEASE=1 DEV_TRUST_ROOT=0 DEV_AUTO_REGISTER=0 \
  CHATTER_ANDROID_VERSION_CODE=100 CHATTER_ANDROID_VERSION_NAME=1.0.0 \
  ./android/build-android.sh
```

Before that command, export existing provisioned public pins
`CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX`, `CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX`,
`CHATTER_KT_VRF_PUBLIC_KEY_HEX`, `CHATTER_TLS_ROOT_CA_PEM`, and the public
`CHATTER_SERVER_URL` (or Android endpoint override `CHATTER_ANDROID_SERVER`).
Export `CHATTER_KEYSTORE`, `CHATTER_KEYSTORE_PASSWORD`, `CHATTER_KEY_ALIAS`, and
`CHATTER_KEY_PASSWORD` through the established external signing setup. This
change neither creates nor rotates a signer. Direct Gradle release task names
also require version metadata; generic Gradle task aliases should use `RELEASE=1`.
Push wake requires the optional public `CHATTER_NTFY_URL`; onion transport needs
`CHATTER_ONION_ADDR`. Neither is invented by packaging.

## Linux x86_64 system-runtime archive

Export the existing public HTTPS RPC origin `CHATTER_SERVER_URL`, the four
provisioned pins listed above, and `CHATTER_LINUX_VERSION`, then run:

```sh
CHATTER_LINUX_VERSION=1.0.0 ./packaging/build-linux-release.sh
```

The entrypoint refuses absent inputs, known development public keys, placeholder
or local endpoints, malformed/expired CA certificates, dev switches, custom Rust
flags, and extra cargo arguments. It does not provision anchors, contact Vault,
or read a private key. Operators must establish that the supplied CA and public
pins match the actual production server; local syntax checks cannot prove that.
The existing Rust compile-time guards remain active.

It runs a locked release build with `--no-default-features --features desktop,jarvis`
(full GStreamer decoding, LiveKit calls, desktop integration), targeting
`x86_64-unknown-linux-gnu` and `target-cpu=x86-64` rather than `native`. The existing
mold and duplicate-definition linker arguments are preserved. C/C++ flags append
baseline `-march=x86-64 -mtune=generic`. Use a clean release environment without
per-target compiler overrides; separately supplied native libraries, including
prebuilt WebRTC, require their own CPU compatibility evidence. This is not proof
that arbitrary older glibc systems can run the output.

Output is under `target/linux-packages/`, with a checksum sidecar. The archive
contains the freshly built binary, desktop assets, runtime checker and these
instructions. It is a **system-runtime archive**, not an AppImage, Flatpak, `.deb`
or a portable bundle. No shared libraries or GStreamer plugins are bundled.
The existing `install-linux-user.sh` remains a developer convenience that copies
an arbitrary existing binary; it does not establish production admission.

### Destination runtime requirements

Use the same or a compatible distribution/glibc baseline as the release builder.
Install GStreamer 1.x core, base, good, bad and libav plugins, GL support and tools,
plus an audio backend (PulseAudio, or PipeWire with a GStreamer sink). Required
factories are encoded in `check-linux-runtime.sh`: appsrc/appsink, playback and
decode bins, audio/video converters, demuxers, Opus/Vorbis/VP8/VP9/H.264/AAC/GIF
decoders, and an audio sink. Additional formats require their codec plugins.

Common Debian/Ubuntu package names are `libgstreamer1.0-0`,
`libgstreamer-plugins-base1.0-0`, `gstreamer1.0-tools`,
`gstreamer1.0-plugins-base`, `gstreamer1.0-plugins-good`,
`gstreamer1.0-plugins-bad`, `gstreamer1.0-libav`, `gstreamer1.0-gl`, and
`gstreamer1.0-pulseaudio` (or `gstreamer1.0-pipewire`). Arch equivalents include
`gstreamer`, `gst-plugins-base`, `gst-plugins-good`, `gst-plugins-bad`, `gst-libav`
and the session audio service. Package availability and ABI versions must be
checked on the chosen release distribution. Build hosts additionally need the
matching development headers/pkg-config packages, Rust, clang and mold.

After unpacking, run on the destination host before copying or launching:

```sh
./check-linux-runtime.sh ./bin/chatter-ui
```

This fails if a required factory or linked shared library is missing. It does
not initialize devices, open a keyring or prove calls. A graphical desktop,
working session D-Bus Secret Service keyring, graphics drivers and usable audio/
video devices are required separately. Preserve OS keyring use; do not disable
it to make an incomplete runtime appear ready. Live calls, media decoding and
server trust need coordinated final runtime verification.

Install the checked binary to your chosen user binary directory and assets to
its desktop/icon directories; rewrite `Exec=chatter-ui` in the desktop file to
the installed absolute binary path. Packaging does not launch or install it.
