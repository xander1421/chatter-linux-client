# cpal 0.17.3: stop an AAudio stream before closing it

The published Apache-2.0 cpal 0.17.3 archive is vendored unchanged except for
`src/host/aaudio/mod.rs`; this document is an addition. Archive SHA-256 matches
the previous workspace lock checksum:

`d8942da362c0f0d895d7cac616263f2f9424edc5687364dfd1d25ef7eba506d7`

rodio 0.22.2 is the only user of this version (attachment audio playback). The
workspace's direct cpal 0.18.2 dependency is not patched.

## Failure and fix

`Stream` had no `Drop`, so releasing it dropped the `ndk::audio::AudioStream`,
which calls `AAudioStream_close` on a stream that is still started. On a
Samsung S9 (Android 10) the app aborted on the `AudioTrack` thread with
"Pure virtual function called" in
`aaudio::AudioStreamLegacy::processCallbackCommon` when one voice message ended
and the next was loaded (the player drops its output for every new source).

`Drop` now requests a stop, waits up to 500 ms for the stream to leave the
`Stopping` state, and sleeps 10 ms before the handle is released.
