//! Where the time goes when a conversation is opened.
//!
//! Opening a DM/channel does **no network I/O** — it is one SQLite page read
//! plus a pile of per-message resolution. This bench replays exactly the call
//! sequence `features/sidebar/{dm,channel}.rs` runs, phase by phase, and prints
//! wall time + query count for each so "load is slow" stops being an
//! impression.
//!
//! Run with:
//!   cargo test -p chatter-core --features dev-trust-root --release \
//!       --test conversation_open_bench -- --nocapture

use std::sync::Arc;
use std::time::Instant;

use chatter_core::domain::attachment::{
    Attachment, AttachmentStatus, ContentLocation, FileMetadata,
};
use chatter_core::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::storage::repositories::{AnonDmRepository, AnonGroupRepository};
use chatter_core::storage::{AttachmentRepository, Database, MessageRepository, UserRepository};
use tempfile::TempDir;

const PAGE: u32 = chatter_core::config::MESSAGE_PAGE_SIZE as u32;
/// Conversations in the corpus besides the one we open.
const OTHER_CONVOS: usize = 30;
/// Messages per conversation — a real DM has far more history than one page.
const HISTORY: usize = 400;
/// Every Nth message quotes an earlier one.
const REPLY_EVERY: usize = 5;
/// Every Nth message carries one image attachment.
const IMAGE_EVERY: usize = 6;

struct Corpus {
    _tmp: TempDir,
    db: Arc<Database>,
    /// The one full-resolution JPEG every image attachment stands for.
    photo: std::path::PathBuf,
    conversation: ChannelId,
    conv_type: ConversationType,
}

fn build_corpus() -> Corpus {
    let tmp = TempDir::new().unwrap();
    let db = Arc::new(Database::open(&tmp.path().join("bench.db"), "bench_passphrase").unwrap());
    db.run_migrations().unwrap();

    let messages = MessageRepository::new(db.clone());
    let attachments = AttachmentRepository::new(db.clone());

    // One real JPEG on disk, referenced by every image attachment. This is the
    // file `AttachmentDisplayData::decode_thumbnail` opens at full resolution.
    let photo_path = tmp.path().join("photo.jpg");
    write_photo(&photo_path, 3024, 4032);

    let conversation = ChannelId::new();
    let conv_type = ConversationType::ServerDm;
    let me = UserId::new();
    let peer = UserId::new();

    let mut ts = 1_700_000_000_000_000_000u64;
    let mut previous: Vec<MessageId> = Vec::new();

    for convo in 0..=OTHER_CONVOS {
        let cid = if convo == 0 {
            conversation
        } else {
            ChannelId::new()
        };
        for i in 0..HISTORY {
            ts += 1_000_000;
            let sender = if i % 2 == 0 { me } else { peer };
            let mut msg = Message::new_text(
                cid,
                conv_type.clone(),
                Some(sender),
                format!("message {i} in conversation {convo} with some realistic length of text"),
                ts,
            );
            if convo == 0 && i % REPLY_EVERY == 0 && !previous.is_empty() {
                msg.reply_to = Some(previous[previous.len() / 2]);
                msg.reply_to_author_id = Some(peer);
                msg.reply_to_text = Some("quoted parent text".into());
            }
            if convo == 0 && i % IMAGE_EVERY == 0 {
                let att_id = AttachmentId::new();
                let mut meta =
                    FileMetadata::new("photo.jpg".into(), "image/jpeg".into(), 2_400_000);
                meta.width = Some(3024);
                meta.height = Some(4032);
                // Sender-provided low-res placeholder, as the wire carries it.
                meta.thumbnail = Some(jpeg_bytes(64, 64));
                meta.thumbnail_width = Some(64);
                meta.thumbnail_height = Some(64);
                let att = Attachment {
                    id: att_id,
                    metadata: meta,
                    status: AttachmentStatus::Ready,
                    content_location: ContentLocation::NotDownloaded,
                    remote_url: None,
                    encryption_key: None,
                    encryption_iv: None,
                    created_at: chrono::Utc::now(),
                    secret_chat: Some(false),
                };
                attachments.insert(&att).unwrap();
                msg.attachments = vec![att_id];
            }
            if convo == 0 {
                previous.push(msg.id);
            }
            messages.insert(&msg).unwrap();
        }
    }

    // Deliberately no profile row for either participant: `get_display_name`
    // then walks its whole 4-query fallback chain, which is the real state for
    // any sender whose profile has not been fetched yet.
    Corpus {
        _tmp: tmp,
        db,
        photo: photo_path,
        conversation,
        conv_type,
    }
}

fn write_photo(path: &std::path::Path, w: u32, h: u32) {
    std::fs::write(path, jpeg_bytes(w, h)).unwrap();
}

fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 233) as u8])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .unwrap();
    out.into_inner()
}

#[test]
fn bench_conversation_open() {
    let t_corpus = Instant::now();
    let c = build_corpus();
    let corpus_ms = t_corpus.elapsed();

    let messages = MessageRepository::new(c.db.clone());
    let attachments = AttachmentRepository::new(c.db.clone());
    let users = UserRepository::new(c.db.clone());
    let anon_dm = AnonDmRepository::new(c.db.clone());
    let anon_group = AnonGroupRepository::new(c.db.clone());

    println!("\n=== Conversation open: cost breakdown ===");
    println!(
        "corpus: {} conversations x {} messages (built in {:?})",
        OTHER_CONVOS + 1,
        HISTORY,
        corpus_ms
    );

    // Warm the page cache the same way a running app would be warm.
    let _ = messages
        .list_for_channel_asc(&c.conversation, &c.conv_type, None, PAGE)
        .unwrap();

    // ── EXPLAIN QUERY PLAN for the one list query ─────────────────────────
    {
        let conn = c.db.read_connection().unwrap();
        let mut stmt = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 AND thread_id IS NULL \
                 ORDER BY sent_at DESC, sender_id DESC LIMIT ?3",
            )
            .unwrap();
        let rows = stmt
            .query_map(
                rusqlite::params![c.conversation.to_string(), c.conv_type.as_str(), PAGE],
                |r| r.get::<_, String>(3),
            )
            .unwrap();
        for r in rows {
            println!("  EXPLAIN: {}", r.unwrap());
        }
    }

    // ── P1: the list query + full row decode ──────────────────────────────
    let t = Instant::now();
    let page = messages
        .list_for_channel_asc(&c.conversation, &c.conv_type, None, PAGE)
        .unwrap();
    let p1 = t.elapsed();
    let replies = page.iter().filter(|m| m.reply_to.is_some()).count();
    let att_total: usize = page.iter().map(|m| m.attachments.len()).sum();
    println!(
        "P1 list_for_channel_asc({} rows, {} replies, {} attachments): {:?}   [1 query]",
        page.len(),
        replies,
        att_total,
        p1
    );

    // ── P1a: same rows, no serde/bincode decode — isolates SQL+SQLCipher I/O
    let t = Instant::now();
    {
        let conn = c.db.read_connection().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, content_json, attachments_blob, reaction_counts_blob, \
                 current_user_reactions_blob, mentioned_integrations_blob \
                 FROM messages WHERE conversation_id = ?1 AND conversation_type = ?2 \
                   AND thread_id IS NULL ORDER BY sent_at DESC, sender_id DESC LIMIT ?3",
            )
            .unwrap();
        let rows = stmt
            .query_map(
                rusqlite::params![c.conversation.to_string(), c.conv_type.as_str(), PAGE],
                |r| {
                    Ok(r.get::<_, String>(0)?.len()
                        + r.get::<_, String>(1)?.len()
                        + r.get::<_, Vec<u8>>(2)?.len()
                        + r.get::<_, Vec<u8>>(3)?.len()
                        + r.get::<_, Vec<u8>>(4)?.len()
                        + r.get::<_, Vec<u8>>(5)?.len())
                },
            )
            .unwrap();
        let mut bytes = 0usize;
        for r in rows {
            bytes += r.unwrap();
        }
        let p1a = t.elapsed();
        println!(
            "P1a raw fetch, no decode ({} payload bytes): {:?}",
            bytes, p1a
        );
        println!(
            "     → decode (serde_json + 4x bincode/row) ≈ {:?}",
            p1.saturating_sub(p1a)
        );
    }

    // ── P2: sender display name, per message ──────────────────────────────
    // Replays ChatterClient::message_sender_display_name →
    // UserService::get_display_name, whose fallback chain is up to 4 queries.
    let mut q2 = 0usize;
    let t = Instant::now();
    for m in &page {
        let Some(id) = m.sender_id else { continue };
        q2 += 1;
        if users.get_profile(&id).unwrap().is_some() {
            continue;
        }
        q2 += 1;
        if users.get(&id).unwrap().is_some() {
            continue;
        }
        q2 += 1;
        if anon_dm.get_conversation_by_peer(&id).unwrap().is_some() {
            continue;
        }
        q2 += 1;
        let _ = anon_group.find_display_name_for_user(&id).unwrap();
    }
    println!(
        "P2 sender display names ({} msgs): {:?}   [{} queries]",
        page.len(),
        t.elapsed(),
        q2
    );

    // ── P3: attachment metadata, per attachment ───────────────────────────
    let mut q3 = 0usize;
    let t = Instant::now();
    let mut resolved: Vec<Attachment> = Vec::new();
    for m in &page {
        for a in &m.attachments {
            q3 += 1;
            if let Some(att) = attachments.get(a).unwrap() {
                resolved.push(att);
            }
        }
    }
    println!(
        "P3 attachment lookups: {:?}   [{} queries]",
        t.elapsed(),
        q3
    );

    // ── P4: reply parents — resolved TWICE per reply ──────────────────────
    // reply_parent_missing_for → message_repo.get(parent)
    // reply_parent_media       → get_message_local(parent) (the same row again)
    //                            + get_attachment(first attachment)
    let mut q4 = 0usize;
    let t = Instant::now();
    for m in &page {
        let Some(parent_id) = m.reply_to else {
            continue;
        };
        q4 += 1;
        let _missing = match messages.get(&parent_id).unwrap() {
            Some(p) => p.is_deleted,
            None => true,
        };
        q4 += 1;
        if let Some(parent) = messages.get(&parent_id).unwrap() {
            if let Some(att) = parent.attachments.first() {
                q4 += 1;
                let _ = attachments.get(att).unwrap();
            }
        }
    }
    println!(
        "P4 reply-parent resolution ({} replies): {:?}   [{} queries]",
        replies,
        t.elapsed(),
        q4
    );

    // ── P5: thread reply counts (channel path only) ───────────────────────
    let t = Instant::now();
    let _ = messages
        .reply_counts_for_channel(&c.conversation, &c.conv_type)
        .unwrap();
    println!("P5 reply_counts_for_channel: {:?}   [1 query]", t.elapsed());

    // ── P6: thumbnail decode — full-res image::open + Lanczos3 downscale ──
    // This is exactly AttachmentDisplayData::decode_thumbnail's image branch.
    let mut decoded = 0usize;
    let mut t_open = std::time::Duration::ZERO;
    let mut t_lanczos = std::time::Duration::ZERO;
    let mut t_triangle = std::time::Duration::ZERO;
    let mut t_fast = std::time::Duration::ZERO;
    let mut t_placeholder = std::time::Duration::ZERO;
    for att in &resolved {
        let path = &c.photo;
        let t = Instant::now();
        let Ok(img) = image::open(path) else { continue };
        t_open += t.elapsed();

        let t = Instant::now();
        let rgba = img
            .resize(1024, 1024, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        t_lanczos += t.elapsed();
        std::hint::black_box(rgba.dimensions());

        let t = Instant::now();
        let rgba = img
            .resize(1024, 1024, image::imageops::FilterType::Triangle)
            .to_rgba8();
        t_triangle += t.elapsed();
        std::hint::black_box(rgba.dimensions());

        let t = Instant::now();
        let rgba = img.thumbnail(1024, 1024).to_rgba8();
        t_fast += t.elapsed();
        std::hint::black_box(rgba.dimensions());

        // What the cheap path costs: decode the sender-provided placeholder
        // already sitting in `metadata.thumbnail`.
        if let Some(bytes) = att.metadata.thumbnail.as_deref() {
            let t = Instant::now();
            if let Ok(img) = image::load_from_memory(bytes) {
                std::hint::black_box(img.to_rgba8().dimensions());
            }
            t_placeholder += t.elapsed();
        }
        decoded += 1;
    }
    println!("P6 thumbnail decode ({} images):", decoded);
    println!(
        "     NOW  stored 768px thumbnail (converters/attachment.rs): {:?}",
        t_placeholder
    );
    println!(
        "     WAS  image::open(full-res) {:?} + Lanczos3 resize {:?} = {:?}",
        t_open,
        t_lanczos,
        t_open + t_lanczos
    );
    println!(
        "     fallback when no stored thumbnail — thumbnail(): {:?}",
        t_open + t_fast
    );
    println!("     (rejected: Triangle resize {:?})", t_triangle);

    let total_queries = 1 + q2 + q3 + q4 + 1;
    println!(
        "--- one conversation open issues {} SQLite queries ---\n",
        total_queries
    );
}
