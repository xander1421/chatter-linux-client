use super::*;
use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::file_stream::{FileManifest, MAX_REQUESTED_CHUNKS};
use chatter_mesh::tlv::hex_encode;
use sha2::{Digest, Sha256};

const ID: TransferId = [1, 2, 3, 4, 5, 6, 7, 8];

fn spool() -> (FileStreamSpool, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().to_path_buf();
    crate::test_tempdir::hold(dir);
    (FileStreamSpool::new(path.clone()), path)
}

fn manifest_for(bytes: &[u8], chunk_size: u32) -> FileManifest {
    FileManifest::new(
        ID,
        Some("payload.bin".into()),
        Some("application/octet-stream".into()),
        bytes.len() as u64,
        chunk_size,
        Sha256::digest(bytes).into(),
    )
    .expect("valid manifest")
}

fn peer(id: &str) -> PeerId {
    PeerId::from_str(id)
}

fn body(len: usize) -> Vec<u8> {
    // Position-dependent, so a chunk written at the wrong offset changes the
    // digest. A run of one byte would not.
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn feed(
    spool: &mut FileStreamSpool,
    manifest: &FileManifest,
    bytes: &[u8],
    indices: impl IntoIterator<Item = u32>,
    sender: &PeerId,
) -> Vec<ChunkOutcome> {
    feed_at(spool, manifest, bytes, indices, sender, 100)
}

/// `feed` with the arrival time spelled out, for the tests that care when a
/// chunk landed rather than only that it did.
fn feed_at(
    spool: &mut FileStreamSpool,
    manifest: &FileManifest,
    bytes: &[u8],
    indices: impl IntoIterator<Item = u32>,
    sender: &PeerId,
    now_secs: u64,
) -> Vec<ChunkOutcome> {
    indices
        .into_iter()
        .map(|index| {
            let (offset, len) = manifest.chunk_span(index).expect("in range");
            let slice = &bytes[offset as usize..offset as usize + len];
            spool
                .accept_chunk(&ID, index, slice, sender, now_secs)
                .expect("accepted")
        })
        .collect()
}

// -- bitmap ------------------------------------------------------------

#[test]
fn a_manifest_for_a_finished_transfer_does_not_start_it_again() {
    // A sender repeats the manifest on the announce cadence for as long as it
    // holds the transfer open, so repeats keep arriving after the file is
    // complete. The transfer is gone from the map by then — finished and
    // cleared — so `begin` saw a manifest it did not recognise and downloaded
    // the whole file again.
    //
    // The `already_stored` guard downstream cannot catch this: it keys on the
    // manifest *packet's* dedup id, which differs for every re-broadcast. The
    // transfer id is the stable identity, so that is what is remembered here.
    //
    // Observed on hardware: the same 116 KB file received three times in four
    // minutes, and stored twice, while a 2.5 MB transfer waited for the air.
    let (mut spool, _dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1_024);
    let sender = peer("bbbbbbbbbbbbbbbb");

    spool
        .begin(manifest.clone(), sender.clone(), "WIRE-1".into(), 0, 10)
        .expect("first manifest starts it");
    feed(&mut spool, &manifest, &bytes, [0, 1, 2], &sender);
    spool.finish(&ID);

    // The same manifest again, with a *different* wire id — which is what a
    // re-broadcast looks like, and why keying on that id could never work.
    let repeat = spool
        .begin(manifest.clone(), sender.clone(), "WIRE-2".into(), 0, 20)
        .expect("a repeat is not an error");
    assert_eq!(
        repeat,
        ManifestOutcome::AlreadyComplete,
        "a finished transfer is not downloaded a second time"
    );
    assert!(
        spool.get(&ID).is_none(),
        "and no partial transfer is created for it"
    );
}

#[test]
fn the_request_gap_starts_short_and_only_grows_when_asking_stops_working() {
    // The tail of a transfer used to cost far more than the air did. Measured
    // on a real 60-chunk file: one round recovered 49 chunks in about 45
    // seconds, and the last handful then took several minutes — a full 45
    // second wait each, nearly all of it silence. The link was never the
    // limit, the fixed interval was.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spool = FileStreamSpool::new(dir.path().to_path_buf());
    let bytes = body(4_096);
    let manifest = manifest_for(&bytes, 1_024);
    let sender = PeerId::from_str("aaaaaaaaaaaaaaaa");
    spool
        .begin(manifest.clone(), sender.clone(), "WIRE".into(), 0, 0)
        .expect("begin");

    // First ask comes after the floor, not the ceiling.
    let mut now = CHUNK_REQUEST_MIN_QUIET_SECS;
    assert_eq!(
        spool.stalled(now, CHUNK_REQUEST_QUIET_SECS, 8).len(),
        1,
        "a transfer with nothing arriving is asked about within seconds"
    );

    // A request that produces nothing backs off, repeatedly, up to the ceiling.
    let mut gaps = Vec::new();
    let mut last_asked = now;
    for _ in 0..8 {
        now += 1;
        while spool.stalled(now, CHUNK_REQUEST_QUIET_SECS, 8).is_empty() {
            now += 1;
            assert!(now < 10_000, "it must keep asking eventually");
        }
        gaps.push(now - last_asked);
        last_asked = now;
    }
    assert!(
        gaps.windows(2).all(|w| w[1] >= w[0]),
        "each fruitless ask waits at least as long as the last: {gaps:?}"
    );
    assert_eq!(
        *gaps.last().expect("asked"),
        CHUNK_REQUEST_QUIET_SECS,
        "and it settles at the ceiling rather than growing without bound"
    );

    // A chunk arriving means the peer is answering, so go back to asking
    // promptly instead of waiting out a ceiling earned while it was silent.
    feed(&mut spool, &manifest, &bytes, [0], &sender);
    let after_progress = now + CHUNK_REQUEST_MIN_QUIET_SECS + 1;
    assert_eq!(
        spool
            .stalled(after_progress, CHUNK_REQUEST_QUIET_SECS, 8)
            .len(),
        1,
        "progress resets the gap to the floor — this is the several minutes saved"
    );
}

#[test]
fn resume_keeps_the_real_progress_clock_rather_than_restarting_it() {
    // Two failures live here, in opposite directions, and the persisted time
    // is what avoids both.
    //
    // Resume once stamped `last_progress_secs: 0`, which `abandon_stale` reads
    // as "no progress since the epoch" — so every resumed transfer was deleted
    // on the first tick after startup, and resumability failed at the one
    // thing it exists for.
    //
    // Stamping `now` instead fixed that and created the opposite leak: the
    // abandon deadline moved an hour further out on every restart, so a
    // transfer whose sender was long gone became immortal, asking for chunks
    // nobody holds every 45 seconds for as long as the app kept being
    // reopened.
    let now = 1_000_000u64;

    // Recent progress: still alive after a restart.
    let (mut recent, dir) = spool();
    let bytes = body(9_000);
    let manifest = manifest_for(&bytes, 1_024);
    let sender = peer("cccccccccccccccc");
    recent
        .begin(
            manifest.clone(),
            sender.clone(),
            "WIRE-R".into(),
            5,
            now - 60,
        )
        .unwrap();
    feed_at(&mut recent, &manifest, &bytes, [0, 1, 2], &sender, now - 60);
    drop(recent);

    let mut restarted = FileStreamSpool::new(dir);
    assert_eq!(restarted.restore(now), 1, "it comes back off disk");
    restarted.abandon_stale(now, TRANSFER_ABANDON_SECS);
    let transfer = restarted
        .get(&ID)
        .expect("a minute without progress is not an hour");
    assert_eq!(
        transfer.received.present(),
        3,
        "and its chunks are still there"
    );

    // Long-dead progress: abandoned on resume, however many restarts it took
    // to get here.
    let (mut stale_spool, old_dir) = spool();
    stale_spool
        .begin(manifest.clone(), sender.clone(), "WIRE-OLD".into(), 5, 10)
        .unwrap();
    feed_at(&mut stale_spool, &manifest, &bytes, [0], &sender, 10);
    drop(stale_spool);

    let mut reopened = FileStreamSpool::new(old_dir);
    assert_eq!(reopened.restore(now), 1);
    reopened.abandon_stale(now, TRANSFER_ABANDON_SECS);
    assert!(
        reopened.get(&ID).is_none(),
        "a transfer dead for a fortnight does not get another hour just for \
             being reopened"
    );
}

#[test]
fn a_transfer_that_never_answers_settles_at_the_quiet_period_ceiling() {
    // `stalled` used to consult one clock: the last chunk to *arrive*. A
    // transfer receiving nothing never advances that, so it was stalled
    // forever and the caller — which runs every tick — emitted a signed
    // 64-index request twice a second for as long as the transfer existed.
    // Measured on hardware at 2/s for minutes, against a link already too
    // busy to deliver the chunks being asked for.
    //
    // Nothing arrives here, so the backoff saturates almost immediately and
    // this covers the ceiling. The floor and the reset-on-progress are in
    // `the_request_gap_starts_short_and_only_grows_when_asking_stops_working`.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spool = FileStreamSpool::new(dir.path().to_path_buf());
    let bytes = vec![3u8; 4_096];
    let manifest = manifest_for(&bytes, 1_024);
    let sender = PeerId::from_str("aaaaaaaaaaaaaaaa");

    spool
        .begin(manifest.clone(), sender, "WIRE".into(), 0, 100)
        .expect("begin");

    // Nothing has arrived and nothing has been asked for: the first request
    // goes out.
    let first = spool.stalled(100 + CHUNK_REQUEST_QUIET_SECS, CHUNK_REQUEST_QUIET_SECS, 8);
    assert_eq!(first.len(), 1, "a quiet transfer is asked for once");

    // Every moment inside the quiet period after that asks for nothing,
    // however often the caller runs.
    for offset in [0, 1, 2, CHUNK_REQUEST_QUIET_SECS - 1] {
        let again = spool.stalled(
            100 + CHUNK_REQUEST_QUIET_SECS + offset,
            CHUNK_REQUEST_QUIET_SECS,
            8,
        );
        assert!(
            again.is_empty(),
            "a request at +{offset}s repeats one already in flight"
        );
    }

    // And once the period is out, it asks again — the transfer is still
    // incomplete, so giving up would be worse than asking twice.
    let later = spool.stalled(
        100 + CHUNK_REQUEST_QUIET_SECS * 2 + 1,
        CHUNK_REQUEST_QUIET_SECS,
        8,
    );
    assert_eq!(
        later.len(),
        1,
        "the quiet period bounds the rate, not the retries"
    );
}

#[test]
fn a_bitmap_tracks_presence_and_reports_the_earliest_gaps() {
    let mut map = ChunkBitmap::new(20);
    assert_eq!(map.missing(3), vec![0, 1, 2]);
    assert!(map.insert(0));
    assert!(!map.insert(0), "the second set is not news");
    assert!(map.insert(19));
    assert_eq!(map.present(), 2);
    assert!(map.contains(19));
    assert!(!map.contains(20), "an index past the end is never present");
    assert!(!map.insert(20), "and cannot be set");
    assert_eq!(map.missing(2), vec![1, 2]);
    assert!(!map.is_complete());
    for index in 1..19 {
        map.insert(index);
    }
    assert!(map.is_complete());
    assert!(map.missing(10).is_empty());
}

#[test]
fn a_bitmap_of_the_wrong_width_for_its_manifest_is_refused() {
    // Resuming with a mismatched bitmap would resume into a hole: bits would
    // name chunks of a differently chunked file.
    let mut map = ChunkBitmap::new(9);
    map.insert(3);
    let bytes = map.as_bytes().to_vec();
    assert_eq!(bytes.len(), 2);
    assert_eq!(ChunkBitmap::from_bytes(&bytes, 9), Some(map));
    assert_eq!(ChunkBitmap::from_bytes(&bytes, 8), None);
    assert_eq!(ChunkBitmap::from_bytes(&bytes, 17), None);
}

#[test]
fn a_bitmap_with_bits_set_past_its_count_is_refused() {
    // Otherwise `present` counts a chunk that cannot exist and the transfer
    // reads as complete with a hole in it.
    assert_eq!(ChunkBitmap::from_bytes(&[0xFF], 4), None);
    assert_eq!(
        ChunkBitmap::from_bytes(&[0x0F], 4).map(|m| m.present()),
        Some(4)
    );
}

// -- receive -----------------------------------------------------------

#[test]
fn a_chunked_transfer_assembles_the_original_bytes() {
    let (mut spool, _dir) = spool();
    let bytes = body(70_000);
    let manifest = manifest_for(&bytes, 32 * 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    assert_eq!(manifest.chunk_count, 3);

    assert_eq!(
        spool
            .begin(manifest.clone(), sender.clone(), "WIRE-1".into(), 42, 10)
            .unwrap(),
        ManifestOutcome::Started
    );

    let outcomes = feed(&mut spool, &manifest, &bytes, 0..3, &sender);
    assert_eq!(
        outcomes,
        vec![
            ChunkOutcome::Stored {
                present: 1,
                total: 3
            },
            ChunkOutcome::Stored {
                present: 2,
                total: 3
            },
            ChunkOutcome::Complete,
        ]
    );

    spool.verify(&ID).expect("the digest has to match");
    assert_eq!(
        std::fs::read(spool.part_path(&ID)).unwrap(),
        bytes,
        "the assembled file is the file that was sent"
    );
}

#[test]
fn chunks_arriving_out_of_order_still_assemble() {
    // Every chunk is written at its own offset, so order cannot matter — and
    // it must not, because a re-request delivers the gaps last.
    let (mut spool, _dir) = spool();
    let bytes = body(5_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();

    feed(&mut spool, &manifest, &bytes, [4, 0, 3, 1], &sender);
    assert_eq!(spool.get(&ID).unwrap().received.present(), 4);
    assert_eq!(
        feed(&mut spool, &manifest, &bytes, [2], &sender),
        vec![ChunkOutcome::Complete]
    );
    spool.verify(&ID).expect("digest");
    assert_eq!(std::fs::read(spool.part_path(&ID)).unwrap(), bytes);
}

#[test]
fn a_missing_chunk_is_named_by_the_spool_once_the_transfer_goes_quiet() {
    // The resume mechanism's input. Without this the sender would never learn
    // what to retransmit: a broadcast is not acknowledged.
    let (mut spool, _dir) = spool();
    let bytes = body(5_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();
    feed(&mut spool, &manifest, &bytes, [0, 1, 3, 4], &sender);

    assert!(
        spool.stalled(100, CHUNK_REQUEST_QUIET_SECS, 8).is_empty(),
        "a transfer that just made progress is not stalled"
    );
    let stalled = spool.stalled(100 + CHUNK_REQUEST_QUIET_SECS, CHUNK_REQUEST_QUIET_SECS, 8);
    assert_eq!(stalled, vec![(ID, vec![2])]);

    feed(&mut spool, &manifest, &bytes, [2], &sender);
    assert!(
        spool.stalled(1_000, CHUNK_REQUEST_QUIET_SECS, 8).is_empty(),
        "a complete transfer has nothing to ask for"
    );
}

#[test]
fn a_digest_mismatch_discards_and_leaves_nothing_to_retry() {
    // The rule the design is explicit about: a mismatch means some chunk
    // that passed its packet signature still produced the wrong file, so
    // there is nothing to re-request and asking again would loop.
    let (mut spool, _dir) = spool();
    let bytes = body(3_000);
    let mut manifest = manifest_for(&bytes, 1024);
    manifest.sha256 = [0xAB; 32];
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();
    assert_eq!(
        feed(&mut spool, &manifest, &bytes, 0..3, &sender).last(),
        Some(&ChunkOutcome::Complete)
    );

    let part = spool.part_path(&ID);
    assert!(matches!(
        spool.verify(&ID),
        Err(MeshError::TransferDigestMismatch { .. })
    ));
    assert!(spool.get(&ID).is_none(), "the transfer is gone");
    assert!(!part.exists(), "and so are its bytes");
    assert!(
        spool.stalled(10_000, 0, 8).is_empty(),
        "nothing is left that could be re-requested"
    );
    // A chunk arriving afterwards finds no transfer rather than restarting one.
    assert_eq!(
        spool
            .accept_chunk(&ID, 0, &bytes[..1024], &sender, 200)
            .unwrap(),
        ChunkOutcome::Unknown
    );
}

#[test]
fn a_restart_resumes_from_the_persisted_bitmap_instead_of_starting_over() {
    // The reason the bitmap is on disk at all. In memory, resume survives a
    // link drop and dies with the process — which is exactly the case it is
    // wanted for.
    let (mut spool, dir) = spool();
    let bytes = body(9_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "WIRE-9".into(), 77, 10)
        .unwrap();
    feed(
        &mut spool,
        &manifest,
        &bytes,
        [0, 1, 2, 3, 4, 5, 6],
        &sender,
    );
    drop(spool);

    // A fresh process over the same directory.
    let mut restarted = FileStreamSpool::new(dir);
    assert_eq!(restarted.restore(200), 1);
    let transfer = restarted.get(&ID).expect("resumed");
    assert_eq!(transfer.received.present(), 7);
    assert_eq!(transfer.wire_id, "WIRE-9", "the row identity survives too");
    assert_eq!(transfer.timestamp_ms, 77);
    assert_eq!(transfer.sender, sender);
    assert_eq!(restarted.stalled(10_000, 60, 8), vec![(ID, vec![7, 8])]);

    // And the tail completes rather than the whole file being resent.
    assert_eq!(
        feed(&mut restarted, &manifest, &bytes, [7, 8], &sender).last(),
        Some(&ChunkOutcome::Complete)
    );
    restarted.verify(&ID).expect("digest");
    assert_eq!(std::fs::read(restarted.part_path(&ID)).unwrap(), bytes);
}

#[test]
fn a_repeated_manifest_resumes_rather_than_truncating_what_arrived() {
    // Manifests repeat on the announce cadence so a peer joining mid-transfer
    // can pick one up. If a repeat restarted the transfer, that cadence would
    // guarantee no transfer ever finished.
    let (mut spool, _dir) = spool();
    let bytes = body(4_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();
    feed(&mut spool, &manifest, &bytes, [0, 1], &sender);

    assert_eq!(
        spool
            .begin(manifest.clone(), sender.clone(), "W2".into(), 2, 20)
            .unwrap(),
        ManifestOutcome::Unchanged {
            present: 2,
            total: 4
        }
    );
    assert_eq!(spool.get(&ID).unwrap().received.present(), 2);
    assert_eq!(
        spool.get(&ID).unwrap().wire_id,
        "W",
        "the row identity is the first manifest's, so a repeat cannot fork the row"
    );
}

#[test]
fn a_manifest_that_changes_its_chunking_mid_transfer_is_refused() {
    let (mut spool, _dir) = spool();
    let bytes = body(4_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();
    let mut reshaped = manifest_for(&bytes, 2048);
    reshaped.sha256 = manifest.sha256;
    assert!(matches!(
        spool.begin(reshaped, sender, "W".into(), 1, 11),
        Err(MeshError::TransferManifestChanged { .. })
    ));
}

#[test]
fn a_chunk_from_a_different_peer_is_refused() {
    // Otherwise a transfer is an unauthenticated write channel into a file:
    // the packet signature proves who sent *that packet*, not that they are
    // the peer whose manifest opened the file.
    let (mut spool, _dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender, "W".into(), 1, 10)
        .unwrap();
    assert!(matches!(
        spool
            .accept_chunk(&ID, 0, &bytes[..1024], &peer("cccccccccccccccc"), 20)
            .unwrap(),
        ChunkOutcome::Refused(_)
    ));
    assert_eq!(spool.get(&ID).unwrap().received.present(), 0);
}

#[test]
fn a_chunk_of_the_wrong_length_or_index_is_refused() {
    let (mut spool, _dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();

    // Short: would leave a gap inside its own span, which no later chunk
    // fills and which the digest would then catch far too late.
    assert!(matches!(
        spool
            .accept_chunk(&ID, 0, &bytes[..100], &sender, 20)
            .unwrap(),
        ChunkOutcome::Refused(_)
    ));
    // Long: would overwrite the head of the next chunk.
    assert!(matches!(
        spool
            .accept_chunk(&ID, 0, &bytes[..2000], &sender, 20)
            .unwrap(),
        ChunkOutcome::Refused(_)
    ));
    // Past the end: nothing to write at.
    assert!(matches!(
        spool
            .accept_chunk(&ID, 99, &bytes[..1024], &sender, 20)
            .unwrap(),
        ChunkOutcome::Refused(_)
    ));
    // The last chunk *is* short, and that one is fine.
    assert_eq!(
        spool
            .accept_chunk(&ID, 2, &bytes[2048..3000], &sender, 20)
            .unwrap(),
        ChunkOutcome::Stored {
            present: 1,
            total: 3
        }
    );
}

#[test]
fn a_duplicate_chunk_is_recognised_rather_than_rewritten() {
    // A re-request racing the original arrival is normal, and rewriting would
    // double the disk traffic of every recovered transfer.
    let (mut spool, _dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();
    feed(&mut spool, &manifest, &bytes, [0], &sender);
    assert_eq!(
        feed(&mut spool, &manifest, &bytes, [0], &sender),
        vec![ChunkOutcome::Duplicate]
    );
}

#[test]
fn a_chunk_for_an_unknown_transfer_is_ignored_not_started() {
    // A peer can hear chunks before the manifest that explains them. Starting
    // a transfer from a chunk would mean assembling a file with no declared
    // size and no digest to check it against.
    let (mut spool, _dir) = spool();
    assert_eq!(
        spool
            .accept_chunk(&ID, 0, b"orphan", &peer("bbbbbbbbbbbbbbbb"), 10)
            .unwrap(),
        ChunkOutcome::Unknown
    );
    assert!(!spool.part_path(&ID).exists());
}

#[test]
fn a_transfer_that_stops_making_progress_is_abandoned_with_its_bytes() {
    let (mut spool, _dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1024);
    let sender = peer("bbbbbbbbbbbbbbbb");
    spool
        .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
        .unwrap();
    feed(&mut spool, &manifest, &bytes, [0], &sender);
    let part = spool.part_path(&ID);

    spool.abandon_stale(100, TRANSFER_ABANDON_SECS);
    assert!(spool.get(&ID).is_some(), "still fresh");
    spool.abandon_stale(100 + TRANSFER_ABANDON_SECS + 1, TRANSFER_ABANDON_SECS);
    assert!(spool.is_empty());
    assert!(
        !part.exists(),
        "an abandoned transfer leaves nothing behind"
    );
}

#[test]
fn a_state_file_with_no_partial_file_is_not_resumed() {
    let (mut spool, dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1024);
    spool
        .begin(manifest, peer("bbbbbbbbbbbbbbbb"), "W".into(), 1, 10)
        .unwrap();
    std::fs::remove_file(spool.part_path(&ID)).unwrap();

    let mut restarted = FileStreamSpool::new(dir);
    assert_eq!(restarted.restore(200), 0);
    assert!(restarted.is_empty());
}

#[test]
fn a_corrupt_state_file_is_removed_rather_than_accumulated() {
    let (mut spool, dir) = spool();
    let bytes = body(3_000);
    let manifest = manifest_for(&bytes, 1024);
    spool
        .begin(manifest, peer("bbbbbbbbbbbbbbbb"), "W".into(), 1, 10)
        .unwrap();
    let state = dir.join(format!("{}.state", hex_encode(&ID)));
    std::fs::write(&state, b"not a transfer").unwrap();

    let mut restarted = FileStreamSpool::new(dir);
    assert_eq!(restarted.restore(200), 0);
    assert!(
        !state.exists(),
        "and it does not linger to be retried forever"
    );
}

// -- send --------------------------------------------------------------

#[test]
fn a_send_walks_the_file_in_windows_and_never_reads_it_whole() {
    let (_spool, dir) = spool();
    let bytes = body(5_000);
    let path = dir.join("source.bin");
    std::fs::write(&path, &bytes).unwrap();
    let manifest = manifest_for(&bytes, 1024);
    let mut send = OutboundTransfer::new(manifest.clone(), path, "MSG".into(), 0);

    let mut rebuilt = Vec::new();
    let mut emitted = Vec::new();
    while let Some(index) = send.next_chunk() {
        let chunk = send.read_chunk(index).expect("read");
        assert!(chunk.len() <= 1024, "one window at a time");
        emitted.push(index);
        rebuilt.extend_from_slice(&chunk);
    }
    assert_eq!(emitted, vec![0, 1, 2, 3, 4]);
    assert_eq!(rebuilt, bytes, "the windows tile the file exactly");
    assert!(send.is_drained());
    assert!(send.read_chunk(5).is_err(), "no chunk past the end");
}

#[test]
fn a_requested_chunk_is_retransmitted_before_the_first_pass_continues() {
    // The receiver that asked is blocked on that chunk; the first pass is
    // merely unfinished.
    let (_spool, dir) = spool();
    let bytes = body(5_000);
    let path = dir.join("source2.bin");
    std::fs::write(&path, &bytes).unwrap();
    let mut send = OutboundTransfer::new(manifest_for(&bytes, 1024), path, "MSG".into(), 0);

    assert_eq!(send.next_chunk(), Some(0));
    assert_eq!(send.next_chunk(), Some(1));
    send.note_requested(&[0, 99]);
    assert_eq!(send.next_chunk(), Some(0), "the re-request jumps the queue");
    assert_eq!(send.next_chunk(), Some(2), "then the pass resumes");
    assert_eq!(send.next_chunk(), Some(3));
    assert_eq!(send.next_chunk(), Some(4));
    assert_eq!(
        send.next_chunk(),
        None,
        "index 99 is not in the manifest and must not keep the transfer alive"
    );
    assert!(send.is_drained());
}

#[test]
fn a_requested_chunk_reopens_a_drained_transfer() {
    let (_spool, dir) = spool();
    let bytes = body(2_000);
    let path = dir.join("source3.bin");
    std::fs::write(&path, &bytes).unwrap();
    let mut send = OutboundTransfer::new(manifest_for(&bytes, 1024), path, "MSG".into(), 0);
    while send.next_chunk().is_some() {}
    assert!(send.is_drained());

    send.note_requested(&[1]);
    assert!(!send.is_drained(), "there is work again");
    assert_eq!(send.next_chunk(), Some(1));
    assert!(send.is_drained());
}

#[test]
fn the_request_cap_bounds_what_one_round_asks_for() {
    let mut map = ChunkBitmap::new(500);
    assert_eq!(
        map.missing(MAX_REQUESTED_CHUNKS).len(),
        MAX_REQUESTED_CHUNKS
    );
    for index in 0..500 {
        map.insert(index);
    }
    assert!(map.missing(MAX_REQUESTED_CHUNKS).is_empty());
}
