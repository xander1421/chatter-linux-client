# Phase 74: LiveKit Video & Audio Calls - Research

**Researched:** 2026-02-23
**Domain:** LiveKit Rust SDK + Signal Protocol media encryption
**Confidence:** HIGH

<research_summary>
## Summary

Researched LiveKit's official Rust SDK (`livekit` crate v0.7.32) for integrating real-time video/audio calls into the Chatter linux-client. The SDK provides full WebRTC functionality with native H.264 hardware encoding on Linux (NVidia/AMD GPUs).

LiveKit includes built-in E2EE using AES-GCM frame encryption with insertable streams, but **Signal Protocol is NOT suitable for real-time media**. Double Ratchet requires per-message state updates which are impractical at 30-60fps. Instead, we should use LiveKit's native E2EE with **Signal Protocol for key exchange only**.

**Primary recommendation:** Use LiveKit Rust SDK with built-in AES-GCM E2EE. Derive the shared encryption key using Signal Protocol's existing key agreement (X3DH/PQXDH) but NOT Double Ratchet for frame encryption. Integrate with existing `SenderKeyManager` for group calls or use shared keys derived from DM sessions.

</research_summary>

<standard_stack>
## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| livekit | 0.7.32 | WebRTC real-time SDK | Official Rust SDK, Tokio-based |
| livekit-api | 0.7.x | Server API + access tokens | Room management, JWT generation |
| livekit-protocol | 0.7.x | Protocol definitions | Protobuf types for signaling |
| libwebrtc (bundled) | - | WebRTC core | C++ WebRTC wrapped by livekit |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| cpal | 0.15+ | Audio device capture | Microphone input |
| rodio | 0.17+ | Audio playback | Speaker output (existing in project) |
| v4l | 0.14+ | Video4Linux capture | Webcam capture on Linux |
| ashpd | 0.8+ | XDG Desktop Portal | Screen sharing via PipeWire |

### Server-Side (Go)
| Library | Version | Purpose |
|---------|---------|---------|
| github.com/livekit/server-sdk-go | latest | Generate JWT access tokens |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| LiveKit | WebRTC directly | LiveKit handles SFU, scalability, reconnection |
| AES-GCM E2EE | Signal Double Ratchet | Double Ratchet too slow for 30fps media |
| LiveKit cloud | Self-hosted | Self-hosted requires livekit-server deployment |

**Installation:**
```toml
[dependencies]
livekit = { version = "0.7", features = ["native-tls"] }
livekit-api = "0.7"
tokio = { version = "1", features = ["full"] }
```

**Required rustflags (`.cargo/config.toml`):**
```toml
[target.x86_64-unknown-linux-gnu]
# No special flags needed for Linux
```

</standard_stack>

<architecture_patterns>
## Architecture Patterns

### Recommended Project Structure
```
src/
├── calls/
│   ├── mod.rs              # Public API
│   ├── call_service.rs     # Call orchestration
│   ├── room_manager.rs     # LiveKit room connection
│   ├── track_manager.rs    # Audio/video track publishing
│   ├── key_provider.rs     # Signal-derived key provider
│   └── ui_bridge.rs        # Slint integration
├── media/
│   ├── audio_capture.rs    # Microphone via cpal
│   ├── video_capture.rs    # Webcam via v4l
│   ├── screen_capture.rs   # Screen via ashpd
│   └── frame_renderer.rs   # Video frames → Slint
```

### Pattern 1: Room Connection with E2EE
**What:** Connect to LiveKit room with encryption enabled
**When to use:** Every call session
**Example:**
```rust
use livekit::prelude::*;
use livekit::e2ee::{E2eeOptions, EncryptionType, key_provider::{KeyProvider, KeyProviderOptions}};

// Derive shared key from Signal session (simplified)
let call_key = derive_call_key_from_signal_session(&session_key);

// Create key provider with Signal-derived key
let key_provider = KeyProvider::with_shared_key(
    KeyProviderOptions::default(),
    call_key.to_vec(),
);

// Configure room with E2EE
let room_options = RoomOptions {
    encryption: Some(E2eeOptions {
        key_provider,
        encryption_type: EncryptionType::Gcm,
    }),
    ..Default::default()
};

// Connect
let (room, mut events) = Room::connect(&url, &token, room_options).await?;
room.e2ee_manager().set_enabled(true);
```

### Pattern 2: Publishing Audio/Video Tracks
**What:** Capture and publish local media
**When to use:** Starting a call
**Example:**
```rust
use livekit::prelude::*;
use livekit::track::{LocalAudioTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::prelude::*;

// Create audio track from native source
let audio_source = NativeAudioSource::new(/* sample rate, channels */);
let audio_track = LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(audio_source.clone()));

// Publish with options
room.local_participant()
    .publish_track(
        LocalTrack::Audio(audio_track),
        TrackPublishOptions {
            source: TrackSource::Microphone,
            ..Default::default()
        },
    )
    .await?;
```

### Pattern 3: Receiving and Rendering Video
**What:** Subscribe to remote tracks and render frames
**When to use:** Handling incoming media
**Example:**
```rust
use futures::StreamExt;
use livekit::prelude::*;

// Handle track subscription
match event {
    RoomEvent::TrackSubscribed { track, participant, .. } => {
        match track {
            RemoteTrack::Video(video_track) => {
                let rtc_track = video_track.rtc_track();
                let mut video_stream = NativeVideoStream::new(rtc_track);

                tokio::spawn(async move {
                    while let Some(frame) = video_stream.next().await {
                        // Convert VideoFrame to Slint Image
                        let image = convert_frame_to_slint_image(&frame);
                        // Update UI
                        update_video_display(participant.identity(), image);
                    }
                });
            }
            RemoteTrack::Audio(audio_track) => {
                // Audio goes to default output via LiveKit
            }
        }
    }
    _ => {}
}
```

### Pattern 4: Signal-Derived Key Exchange
**What:** Use Signal Protocol to establish call encryption key
**When to use:** Before call starts (key agreement)
**Example:**
```rust
// For DM calls: derive from existing session
async fn derive_dm_call_key(
    session_store: &mut dyn SessionStore,
    peer_address: &ProtocolAddress,
) -> Result<[u8; 32], CryptoError> {
    // Use HKDF to derive call key from session's root key
    // This is separate from message encryption - one key per call
    let session = session_store.load_session(peer_address, None).await?
        .ok_or(CryptoError::NoSession)?;

    // Derive call-specific key using HKDF with call info
    let hkdf = Hkdf::<Sha256>::new(Some(b"ChatterCall"), session.root_key());
    let mut call_key = [0u8; 32];
    hkdf.expand(b"call-encryption-key", &mut call_key)?;
    Ok(call_key)
}

// For group calls: use sender key distribution
async fn derive_group_call_key(
    distribution_id: Uuid,
    sender_key_store: &mut dyn SenderKeyStore,
) -> Result<[u8; 32], CryptoError> {
    // Create a call-specific sender key distribution
    // All participants derive the same key
    // ...
}
```

### Anti-Patterns to Avoid
- **Double Ratchet for media frames:** Too slow, state explosion at 30fps
- **Re-encrypting every frame with Signal:** Use frame-level AES-GCM instead
- **Not handling reconnection:** LiveKit handles it, but key state must persist
- **Blocking UI thread with frame processing:** Always spawn async tasks

</architecture_patterns>

<dont_hand_roll>
## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| WebRTC stack | Custom WebRTC | LiveKit SDK | Years of edge cases, NAT traversal, codec negotiation |
| Frame encryption | Custom crypto | LiveKit E2EE (AES-GCM) | Insertable streams, hardware acceleration |
| SFU/media routing | Peer mesh | LiveKit server | Scales beyond 4 participants |
| Key ratcheting for media | Double Ratchet | Shared key + epoch | Double Ratchet unsuitable for media frame rate |
| Video encoding | Raw frames | Let WebRTC encode | Hardware H.264/VP9 encoding |
| Screen capture | X11 direct | ashpd/PipeWire | Wayland compatibility, portal permissions |
| Audio device management | Direct ALSA | cpal abstraction | Cross-platform, handles device changes |

**Key insight:** Signal Protocol's Double Ratchet is designed for asynchronous messaging with forward secrecy per message. Real-time media (30-60fps) requires symmetric encryption with a single key for duration of call. Use Signal for key establishment, not frame encryption.

</dont_hand_roll>

<common_pitfalls>
## Common Pitfalls

### Pitfall 1: Using Double Ratchet for Media Frames
**What goes wrong:** Performance disaster, state explosion
**Why it happens:** Assuming Signal Protocol applies to all E2EE
**How to avoid:** Use AES-GCM with a Signal-derived shared key
**Warning signs:** CPU spike, dropped frames, state storage growing

### Pitfall 2: Not Handling Track Subscription Lifecycle
**What goes wrong:** Memory leaks, zombie streams
**Why it happens:** Not cleaning up when participants leave
**How to avoid:** Handle `TrackUnsubscribed` event, drop video streams
**Warning signs:** Memory growth over call duration

### Pitfall 3: Blocking on Frame Processing
**What goes wrong:** Audio/video stutters
**Why it happens:** Processing frames on main thread
**How to avoid:** Spawn tokio tasks for frame handling, use channels
**Warning signs:** UI freezes during calls

### Pitfall 4: Missing E2EE Key Distribution
**What goes wrong:** Participants can't decrypt each other's media
**Why it happens:** Key not shared before joining room
**How to avoid:** Exchange Signal-derived key via existing encrypted DM channel before call
**Warning signs:** `EncryptionState::DecryptionFailed` events

### Pitfall 5: Not Testing with Real SFU
**What goes wrong:** Works in tests, fails in production
**Why it happens:** Local loopback vs real network
**How to avoid:** Test with actual LiveKit server, even local Docker
**Warning signs:** Tests pass but calls fail

### Pitfall 6: Ignoring Screen Share Permissions
**What goes wrong:** Screen capture fails silently on Wayland
**Why it happens:** X11 assumptions, missing portal integration
**How to avoid:** Use ashpd for XDG Desktop Portal, handle permission dialogs
**Warning signs:** Black frames, "access denied" in logs

</common_pitfalls>

<code_examples>
## Code Examples

Verified patterns from LiveKit Rust SDK source:

### Complete Room Connection with E2EE
```rust
// Source: examples/encrypted_text_stream/src/main.rs
use livekit::prelude::*;
use livekit::e2ee::{E2eeOptions, EncryptionType, key_provider::{KeyProvider, KeyProviderOptions}};

async fn connect_encrypted_call(
    url: &str,
    token: &str,
    shared_key: Vec<u8>,
) -> Result<(Room, mpsc::UnboundedReceiver<RoomEvent>), RoomError> {
    let key_provider = KeyProvider::with_shared_key(
        KeyProviderOptions::default(),
        shared_key,
    );

    let room_options = RoomOptions {
        encryption: Some(E2eeOptions {
            key_provider,
            encryption_type: EncryptionType::Gcm,
        }),
        ..Default::default()
    };

    let (room, events) = Room::connect(url, token, room_options).await?;
    room.e2ee_manager().set_enabled(true);

    Ok((room, events))
}
```

### Screen Sharing with Desktop Capturer
```rust
// Source: examples/screensharing/src/lib.rs
use livekit::track::{LocalTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::desktop_capturer::{DesktopCapturer, DesktopCapturerOptions, DesktopCaptureSourceType};
use livekit::webrtc::video_source::native::NativeVideoSource;

async fn publish_screen_share(room: &Room) -> Result<(), RoomError> {
    // Create desktop capturer
    let options = DesktopCapturerOptions::new(DesktopCaptureSourceType::Screen);
    let mut capturer = DesktopCapturer::new(options)?;

    // Get available sources
    let sources = capturer.get_source_list();
    let selected = sources.first().cloned();

    // Create video source
    let video_source = NativeVideoSource::new(
        VideoResolution { width: 1920, height: 1080 },
        true, // is_screencast
    );

    // Start capture with callback
    capturer.start_capture(selected, |frame| {
        // Convert and push frame to video_source
        video_source.capture_frame(&frame);
    });

    // Create and publish track
    let track = LocalVideoTrack::create_video_track(
        "screen_share",
        RtcVideoSource::Native(video_source),
    );

    room.local_participant()
        .publish_track(
            LocalTrack::Video(track),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: VideoCodec::VP9,
                simulcast: false, // Usually false for screen share
                ..Default::default()
            },
        )
        .await?;

    Ok(())
}
```

### Video Frame to Slint Image Conversion
```rust
// Pattern for rendering video in Slint
use slint::SharedPixelBuffer;
use livekit::webrtc::prelude::VideoFrame;

fn convert_video_frame_to_slint(frame: &VideoFrame<impl VideoBuffer>) -> slint::Image {
    // Get frame dimensions
    let width = frame.buffer.width();
    let height = frame.buffer.height();

    // Convert to RGBA (Slint expects RGBA8)
    let mut rgba_data = vec![0u8; (width * height * 4) as usize];

    // I420 to RGBA conversion (use libyuv via livekit::webrtc::native::yuv_helper)
    let (y, u, v) = frame.buffer.data();
    let (stride_y, stride_u, stride_v) = frame.buffer.strides();

    yuv_helper::i420_to_rgba(
        y, stride_y,
        u, stride_u,
        v, stride_v,
        &mut rgba_data, width * 4,
        width, height,
    );

    // Create Slint image
    let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &rgba_data,
        width,
        height,
    );
    slint::Image::from_rgba8(buffer)
}
```

### E2EE State Monitoring
```rust
// Source: livekit/src/room/mod.rs
use livekit::prelude::*;
use libwebrtc::native::frame_cryptor::EncryptionState;

async fn monitor_encryption_state(mut events: mpsc::UnboundedReceiver<RoomEvent>) {
    while let Some(event) = events.recv().await {
        match event {
            RoomEvent::E2eeStateChanged { participant, state } => {
                match state {
                    EncryptionState::Ok => {
                        log::info!("{} encryption active", participant.identity());
                    }
                    EncryptionState::DecryptionFailed => {
                        log::warn!("{} decryption failed - key mismatch?", participant.identity());
                    }
                    EncryptionState::MissingKey => {
                        log::warn!("{} missing encryption key", participant.identity());
                    }
                    EncryptionState::KeyRatcheted => {
                        log::info!("{} key ratcheted", participant.identity());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
```

</code_examples>

<sota_updates>
## State of the Art (2025-2026)

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| WebRTC peer mesh | SFU (LiveKit) | 2020+ | Scales to 100s of participants |
| SRTP only | Insertable streams E2EE | 2021 | True E2EE past SFU |
| VP8 default | AV1/VP9 preferred | 2024 | 30-40% bandwidth savings |
| X11 screen capture | PipeWire/XDG Portal | 2023 | Wayland support |

**New tools/patterns to consider:**
- **livekit-agents:** AI-powered transcription, live captions
- **SVC codecs (AV1/VP9):** Scalable video coding for adaptive quality
- **Single peer connection mode:** Reduces connection overhead

**Deprecated/outdated:**
- **Peer-to-peer mesh for groups:** Doesn't scale past 4 participants
- **X11-only screen capture:** Breaks on Wayland (default on modern distros)
- **VP8 only:** Less efficient than VP9/AV1

</sota_updates>

<signal_protocol_integration>
## Signal Protocol Integration Strategy

### Why NOT Double Ratchet for Media

The Double Ratchet algorithm:
- Updates state with **every message**
- Provides forward secrecy **per message**
- At 30fps video + 50fps audio = 80 ratchet operations/second
- State storage grows unbounded

For real-time media, this is impractical.

### Recommended Approach: Signal Key Agreement + Symmetric Media Encryption

```
┌─────────────────────────────────────────────────────────────┐
│                    CALL SETUP PHASE                          │
│  (Uses Signal Protocol)                                      │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  1. Alice initiates call → sends CallOffer via Signal DM     │
│  2. Bob accepts → sends CallAccept via Signal DM             │
│  3. Both derive call_key from session using HKDF:            │
│     call_key = HKDF(session_root_key, "ChatterCall-" + nonce)│
│  4. Both connect to LiveKit room with call_key as E2EE key   │
│                                                              │
├─────────────────────────────────────────────────────────────┤
│                    MEDIA PHASE                               │
│  (Uses LiveKit AES-GCM)                                      │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  5. Media encrypted/decrypted with call_key via FrameCryptor │
│  6. Forward secrecy via key epochs (optional rotation)       │
│  7. Call ends → call_key discarded                           │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

### DM Call Key Derivation
```rust
use hkdf::Hkdf;
use sha2::Sha256;

fn derive_dm_call_key(
    session_root_key: &[u8; 32],
    call_id: &str,
) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(Some(b"ChatterCallKey"), session_root_key);
    let mut call_key = [0u8; 32];
    hkdf.expand(call_id.as_bytes(), &mut call_key).unwrap();
    call_key
}
```

### Group Call Key (via Sender Keys)
For group calls, leverage the existing sender key infrastructure:

1. Create a call-specific distribution ID
2. Distribute call SKDM to all participants via existing group channel
3. All participants derive the same shared key
4. Use that key as LiveKit E2EE shared key

This gives us:
- **Key agreement via Signal Protocol** (trusted, audited)
- **Media encryption via AES-GCM** (fast, hardware accelerated)
- **Integration with existing crypto code**

</signal_protocol_integration>

<open_questions>
## Open Questions

1. **Key rotation during call**
   - What we know: LiveKit supports key ratcheting via `ratchet_shared_key()`
   - What's unclear: Optimal rotation interval for calls (every 5 min? hourly?)
   - Recommendation: Start without rotation, add later if needed

2. **Screen share separate encryption**
   - What we know: Screen share is a separate track
   - What's unclear: Should it use same key or separate key?
   - Recommendation: Same key for simplicity, unless UX requires "share screen without voice"

3. **Hardware codec support detection**
   - What we know: LiveKit uses H.264 on NVidia/AMD
   - What's unclear: How to detect if hardware encoding available
   - Recommendation: Let LiveKit auto-detect, fall back to VP8 software

</open_questions>

<sources>
## Sources

### Primary (HIGH confidence)
- [livekit/rust-sdks GitHub](https://github.com/livekit/rust-sdks) - Full SDK source, examples
- [LiveKit E2EE Docs](https://docs.livekit.io/transport/encryption/) - Official encryption guide
- [livekit crate docs.rs](https://docs.rs/livekit/latest/livekit/) - API reference

### Secondary (MEDIUM confidence)
- [WebRTC Insertable Streams](https://webrtchacks.com/true-end-to-end-encryption-with-webrtc-insertable-streams/) - E2EE architecture
- [Matrix Olm/Megolm](https://webrtchacks.com/how-does-webrtc-end-to-end-encryption-work-matrix-org-example-dave-baker/) - Alternative E2EE approach
- [Signal Protocol Docs](https://signal.org/docs/) - Double Ratchet specification

### Tertiary (LOW confidence - needs validation)
- Key rotation intervals - community best practice, not official guidance

</sources>

<metadata>
## Metadata

**Research scope:**
- Core technology: LiveKit Rust SDK 0.7.32
- Ecosystem: livekit, livekit-api, libwebrtc (bundled)
- Patterns: E2EE key provider, track publishing, frame rendering
- Pitfalls: Double Ratchet misuse, lifecycle management

**Confidence breakdown:**
- Standard stack: HIGH - from official SDK and crates.io
- Architecture: HIGH - from SDK examples and source
- Pitfalls: HIGH - from SDK issues and documentation
- Signal integration: MEDIUM - novel approach, not reference implementation

**Research date:** 2026-02-23
**Valid until:** 2026-03-23 (30 days - LiveKit ecosystem stable)

</metadata>

---

*Phase: 74-livekit-video-audio-calls*
*Research completed: 2026-02-23*
*Ready for planning: yes*
