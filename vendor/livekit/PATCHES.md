# LiveKit 0.7.52 media sender key-index patch (RC64)

Upstream: https://crates.io/crates/livekit/0.7.52 (Apache-2.0).
The complete published `.crate` archive was extracted without rewriting its
manifest, source, tests, build script, license or metadata. Archive SHA-256:

`f63eeb45aa7d13d3e5c0009ea39c9ce8b7a3587f795eb7c3a13aa4880637295f`

This matches the previously locked crates.io checksum. Workspace
`[patch.crates-io] livekit = { path = "vendor/livekit" }` selects this copy;
Cargo.lock's LiveKit source/checksum lines are removed because a local path
package has no registry checksum. The original patch left dependency versions unchanged; see the later dependency constraints below.
The RC64 upstream files modified are `src/room/e2ee/manager.rs` and
`src/room/e2ee/key_provider.rs`. Runtime configuration additionally modifies
`src/room/mod.rs` to forward an explicit private CA through RoomOptions to
SignalOptions; see `../livekit-api/PATCHES.md`. This document is additional. No native WebRTC,
libwebrtc or codec implementation is changed by these patches.
Vendoring the package is necessary for Cargo's reproducible path patch; editing
an installed registry cache would neither persist nor define the shipped build.

## Contract

KeyProvider.latest_key_index selects SDK data keys. It does not call the native
RTP FrameCryptorTransformer.SetKeyIndex API. Merely installing an externally
rotated shared key therefore leaves media encrypting with slot zero.

`E2eeManager::install_shared_media_key` installs the current shared key, selects
that index on all registered **local** native RTP cryptors, then installs the
optional future receive key without selecting it for data or RTP sending. The
future slot may overlap the old sending slot on an adoption jump, so selecting
all senders before preinstalling that slot matters. Older ring slots remain
available until normal ring reuse. No receiver cryptor is reindexed; encrypted
received frames carry their own index. No cryptor is disabled during rotation,
which avoids turning encrypted media into plaintext while switching keys.
Native shared-provider rejection is returned to the app before selecting an
index. The native provider SetSharedKey contract returns false only when the
provider is not shared: after a successful current-key install, the future-key
install on that same provider cannot reject; both native returns are checked. Invalid ring indices and a current/next slot collision are rejected
before mutation. The app's authenticated key-authority and publish-gate rules
remain outside the SDK.

Sender creation, index selection, registration and enablement share the same
manager mutex as key installation. Every newly created native cryptor selects
the manager's current sending index while still disabled, before it can encrypt
with stale slot zero. A rotation cannot fall between creation and registration
and miss a new sender. The packet-trailer and state observer setup still occur
before enablement. Unpublication removes sender enrollment; cleanup disables and
clears both registries. SDK reconnect/republication uses the retained sending
index. A new room initializes it from its supplied provider.

The manager lock serializes publication and key lifecycle operations, not the
native encoder thread or frames already in flight. Peers still need the existing
receive ring/skew policy. This patch does not claim packet-level atomic rotation,
authenticated decryption success, physical audibility, or any new guarantee for
unencrypted publications.

## Regression coverage and validation

The `media_key_index_tests` unit-test module in manager.rs creates real native
RTP audio/video senders without an SFU, network negotiation or physical audio
devices. It invokes the production registration/install path and reads the
native cryptor's index, including selection **before** enablement. It covers:

- Existing and later senders, adoption jumps, 15/16 ring wrap and future receive
  slots, preserving prior slots and data sending selection.
- Video publication, unpublication, cleanup/republication and DM index-zero
  re-rooting.
- Rotation contending with sender registration, and unchanged receiver indices.
- Rejection by a non-shared native provider and invalid/current-next collisions.

Protected root validation command (not run by this worker):

`cargo test --locked -p livekit --lib media_key_index_tests`

Run in the established protected native build environment; it links the actual
locked WebRTC APIs. It does not run upstream SFU integration tests. Physical APK
validation must separately inspect the numeric epoch/sender-index diagnostics,
receiver cryptor success, nonzero queued/rendered PCM and actual audibility.
New diagnostics contain only bounded ring indices (0..15 in Chatter) and app
epoch numbers, with no keys, identities, track IDs or media content. They run in
publication/key lifecycle code, never realtime audio callbacks.

Chatter additionally stages a zeroizing CallKeyState candidate in the command
actor before rotation/adoption installation. Only native selection success
commits the chain and opens deferred publication; failure propagates through
the existing oneshot Result to the caller, retaining the authority epoch for
retry. Catch-up/timer direct setters likewise stop on failure without committing
the candidate or claiming successful installation. No candidate secret is logged
or exposed via Debug. Under the inspected native provider contract a rejected
shared install has no partial mutation; a successful current install implies
the same shared provider accepts next. If a future SDK changes that contract to
allow a second install to fail independently, the manager will report failure
but the native current key/index may already be selected; do not treat staged
application state as rollback of native key material in that hypothetical case.

## Dependency constraints (2026-10-11)

Cargo.toml now uses exact versions for the LiveKit protocol family used by
this patched SDK. The 0.7.13 protocol release adds required request fields that
break livekit-api 0.5.5; newer livekit-common/data-stream/datatrack releases also
pull protocol 0.8 types into the 0.7.52 SDK. Keep protocol 0.7.10,
livekit-common 0.1.0, livekit-data-stream 0.1.0, livekit-datatrack 0.1.11, and
libwebrtc 0.3.41 with webrtc-sys 0.3.38 and webrtc-sys-build 0.3.18 together until the deferred SDK upgrade has a live-call proof.
These are dependency constraints only; no media implementation changed.

## Adaptive screen-share sender (2026-10-31)

`LocalVideoTrack::set_screen_share_encoding` updates a single encoding's maximum
bitrate, frame rate and resolution scale through getParameters/setParameters,
preserving all other sender state and propagating SDK errors. Chatter's
screen-share controller uses publisher candidate-pair BWE and track-specific
outbound congestion/encoder statistics. No renegotiation or frame queue is added.

`Room::get_stats` returns an owned session snapshot future, allowing callers to
release the application room lock before awaiting native statistics. Existing
`.get_stats().await` callers retain the same result type and behavior.
