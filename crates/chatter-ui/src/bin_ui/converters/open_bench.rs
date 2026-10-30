//! What opening a conversation costs, end to end on this side of the network:
//! a real `ChatterClient` over a real SQLCipher database, through the calls
//! `features/sidebar/{dm,channel}.rs` make — mark read, pins, the history
//! page, the row batch (names, avatars, attachments, thumbnails) and the Slint
//! rows the UI thread builds.
//!
//! Guards the 2026-10-05 fix. Before it, every row decoded its sender's 512 px
//! avatar (and DM rows, which never draw one, did too): 400 ms of an 870 ms DM
//! open on a Galaxy S9, ~225 ms of ~430 ms here. The budgets below are several
//! times the fixed cost and well under the old one, so they fail on a return to
//! per-row decoding, not on a slow machine. The structural checks fail on it
//! regardless of timing.
//!
//!   cargo test -p chatter-ui --lib open_bench -- --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::domain::attachment::{
    Attachment, AttachmentStatus, ContentLocation, FileMetadata,
};
use chatter_core::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::domain::user::Profile;
use chatter_core::storage::{AttachmentRepository, Database, MessageRepository, UserRepository};

use super::{MessageDisplayData, messages_to_display_batch};
use crate::bin_ui::decode_cache;

const HISTORY: usize = 100;
/// Open = batch + Slint rows, warm cache, unoptimised test build. Measured
/// ~15-40 ms after the fix; per-row avatar decoding alone added ~350 ms.
const WARM_BUDGET: Duration = Duration::from_millis(150);
/// First open after a wipe: also decodes each thumbnail and avatar once.
const COLD_BUDGET: Duration = Duration::from_millis(400);

fn photo(w: u32, h: u32, seed: u32) -> image::RgbaImage {
    // Smooth gradients plus noise: lossless WebP sizes it like a real photo.
    let mut s = seed.wrapping_mul(2_654_435_761) | 1;
    image::RgbaImage::from_fn(w, h, |x, y| {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let n = (s % 24) as i32 - 12;
        let c = |v: i32| (v + n).clamp(0, 255) as u8;
        image::Rgba([
            c((x * 255 / w) as i32),
            c((y * 255 / h) as i32),
            c(((x + y) / 3 % 256) as i32),
            255,
        ])
    })
}

/// Avatars and inline thumbnails are stored as lossless WebP
/// (`media/image_processor.rs`).
fn webp(img: &image::RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    out
}

struct Fixture {
    _tmp: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    client: Arc<ChatterClient>,
    me: UserId,
    dm: ChannelId,
    channel: ChannelId,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = rt.enter();

    // Off the OS keyring (no desktop prompt): the passphrase lives in a file
    // beside the database, which the second handle below reads back.
    let db_path = chatter_core::system::Directories::with_base_dir(tmp.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db_path
            .parent()
            .unwrap()
            .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = ChatterClient::builder()
        .server_url("https://127.0.0.1:9")
        .data_dir(tmp.path().to_str().unwrap())
        .build()
        .expect("client");
    let passphrase = chatter_core::system::keyring::Keyring::new(db_path.clone())
        .get_passphrase()
        .unwrap();
    let db = Arc::new(Database::open(&db_path, &passphrase).unwrap());
    let messages = MessageRepository::new(db.clone());
    let attachments = AttachmentRepository::new(db.clone());
    let users = UserRepository::new(db.clone());

    let me = UserId::new();
    let peers: Vec<UserId> = (0..3).map(|_| UserId::new()).collect();
    for (i, user) in std::iter::once(&me).chain(&peers).enumerate() {
        users.ensure_user_exists(user).unwrap();
        let mut profile = Profile::new(*user, format!("User {i}"));
        profile.avatar_bytes = Some(webp(&photo(512, 512, i as u32 + 1)));
        users.upsert_profile(&profile).unwrap();
    }
    let thumb = webp(&photo(576, 768, 11));
    let gif_thumb = webp(&photo(480, 270, 12));

    let dm = ChannelId::from(peers[0]);
    let channel = ChannelId::new();
    let mut ts = 1_700_000_000_000_000_000u64;
    for (conv, conv_type, senders) in [
        (dm, ConversationType::ServerDm, vec![me, peers[0]]),
        (
            channel,
            ConversationType::Channel,
            vec![me, peers[0], peers[1], peers[2]],
        ),
    ] {
        let mut ids: Vec<MessageId> = Vec::new();
        for i in 0..HISTORY {
            ts += 1_000_000_000;
            let mut msg = Message::new_text(
                conv,
                conv_type.clone(),
                Some(senders[i % senders.len()]),
                format!("message {i} with **bold**, _italic_ and https://example.com/{i}"),
                ts,
            );
            if i % 7 == 0 && !ids.is_empty() {
                msg.reply_to = Some(ids[ids.len() / 2]);
                msg.reply_to_author_id = Some(peers[0]);
                msg.reply_to_text = Some("quoted parent".into());
            }
            let meta = if i % 10 == 3 {
                let mut m = FileMetadata::new("photo.jpg".into(), "image/jpeg".into(), 2_400_000);
                (m.width, m.height) = (Some(3024), Some(4032));
                m.thumbnail = Some(thumb.clone());
                (m.thumbnail_width, m.thumbnail_height) = (Some(576), Some(768));
                Some(m)
            } else if i % 40 == 20 {
                let mut m = FileMetadata::new("anim.gif".into(), "image/gif".into(), 900_000);
                (m.width, m.height) = (Some(480), Some(270));
                m.thumbnail = Some(gif_thumb.clone());
                Some(m)
            } else if i % 17 == 5 {
                let mut m = FileMetadata::new("voice.ogg".into(), "audio/ogg".into(), 40_000);
                m.duration_seconds = Some(12.0);
                m.waveform_samples = Some((0..32u8).map(|k| k * 7).collect());
                Some(m)
            } else {
                None
            };
            if let Some(metadata) = meta {
                let att = Attachment {
                    id: AttachmentId::new(),
                    metadata,
                    status: AttachmentStatus::Ready,
                    content_location: ContentLocation::NotDownloaded,
                    remote_url: None,
                    encryption_key: None,
                    encryption_iv: None,
                    created_at: chrono::Utc::now(),
                    secret_chat: false,
                };
                attachments.insert(&att).unwrap();
                msg.attachments = vec![att.id];
            }
            ids.push(msg.id);
            messages.insert(&msg).unwrap();
        }
    }
    Fixture {
        _tmp: tmp,
        _rt: rt,
        client,
        me,
        dm,
        channel,
    }
}

/// One open, as the sidebar runs it. Returns the rows and the time from the
/// history page to the Slint model (the part this fix is about).
fn open(
    f: &Fixture,
    conv: ChannelId,
    conv_type: ConversationType,
) -> (Vec<MessageDisplayData>, Duration) {
    let client = &f.client;
    let _ = client.mark_conversation_read(&conv, &conv_type);
    let _ = f
        ._rt
        .block_on(client.load_pinned_for_channel(&conv, &conv_type));
    let page = client
        .get_messages_local_asc(
            &conv,
            &conv_type,
            None,
            chatter_core::config::MESSAGE_PAGE_SIZE as u32,
        )
        .unwrap();
    assert_eq!(page.len(), chatter_core::config::MESSAGE_PAGE_SIZE as usize);
    let started = Instant::now();
    let display = messages_to_display_batch(
        &page,
        |ms| client.message_sender_display_names(ms),
        |m| client.message_sender_avatar(m),
        |a| client.get_attachment(a),
        |m| client.reply_author_display_name(m),
        |m| client.reply_parent_missing_for(m),
        |m| client.reply_parent_media(m),
        |m| client.mention_members(&m.conversation_type, &m.conversation_id),
    );
    let me = f.me.to_string();
    let rows: Vec<_> = display.iter().map(|d| d.to_slint_with_user(&me)).collect();
    let model = slint::ModelRc::new(slint::VecModel::from(rows));
    let elapsed = started.elapsed();
    std::hint::black_box(model);
    (display, elapsed)
}

#[test]
fn conversation_open_stays_fast() {
    let _serial = decode_cache::TEST_SERIAL
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let f = fixture();

    for (name, conv, conv_type) in [
        ("dm", f.dm, ConversationType::ServerDm),
        ("channel", f.channel, ConversationType::Channel),
    ] {
        decode_cache::wipe();
        let (_, cold) = open(&f, conv, conv_type.clone());
        let (rows, warm) = open(&f, conv, conv_type.clone());
        println!(
            "open {name}: cold {cold:?}, warm {warm:?} (budgets {COLD_BUDGET:?} / {WARM_BUDGET:?})"
        );

        if conv_type == ConversationType::ServerDm {
            // A DM row draws no avatar, so none is resolved or decoded.
            assert!(
                rows.iter()
                    .all(|r| r.sender_avatar_bytes.is_none() && r.sender_avatar_decoded.is_none())
            );
        } else {
            // One decoded avatar per sender, at row size, shared by its rows.
            let mut by_sender = std::collections::HashMap::new();
            for row in &rows {
                let px = row
                    .sender_avatar_decoded
                    .as_ref()
                    .expect("channel rows carry an avatar");
                assert_eq!(px.width().max(px.height()), decode_cache::AVATAR_PX);
                let ptr = px.as_bytes().as_ptr() as usize;
                assert_eq!(
                    *by_sender.entry(row.sender_id.clone()).or_insert(ptr),
                    ptr,
                    "avatar decoded per row"
                );
            }
        }
        // Thumbnails come back from the cache on a reopen.
        let thumbs = rows
            .iter()
            .flat_map(|r| &r.attachments)
            .filter(|a| a.is_image)
            .count();
        assert!(thumbs > 0);
        assert!(
            rows.iter()
                .flat_map(|r| &r.attachments)
                .filter(|a| a.is_image)
                .all(|a| a.thumbnail_pixels.is_some())
        );

        assert!(cold < COLD_BUDGET, "{name}: first open took {cold:?}");
        assert!(warm < WARM_BUDGET, "{name}: reopen took {warm:?}");
    }
    decode_cache::wipe();
}
