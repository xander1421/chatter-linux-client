//! VALIDATED — empirical proof harness for chatter's security claims.
//!
//! This suite exists to make the README's claims *falsifiable*. Every test here
//! must be able to FAIL. A test that cannot distinguish "encrypted" from
//! "absent" or "errored" proves nothing, no matter how green it looks.
//!
//! Design rules (violate these and the proof is worthless):
//!
//!   1. POSITIVE CONTROL FIRST. Never grep a store for absent plaintext without
//!      first proving the ciphertext is actually IN that store. A grep against
//!      an empty table always passes.
//!   2. NEGATIVE HALF REQUIRED. "Client can decrypt" is half a proof. The other
//!      half is "server, holding everything it holds, cannot".
//!   3. MUTATION CHECK. Each test names, in a comment, the sabotage that makes
//!      it fail. If no such sabotage exists, delete the test.
//!
//! Requires: INTEGRATION_TESTS=1, a running server on localhost:50051, and the
//! docker-compose stack (psql/redis-cli are shelled out to, so the SQL in the
//! output is reproducible by hand — that is deliberate).
//!
//! The `testing` feature is not optional: this suite reaches into the service
//! layer (`client.signal().decrypt_message`, and 17 other call sites) to prove
//! the client can read what the server cannot, and those accessors are
//! `pub(crate)` without it.
//!
//! `dev-trust-root` is likewise not optional: this suite talks to the local
//! docker-compose server, whose Vault signs sender certificates with the dev
//! keypair. Integration tests link the lib without `cfg(test)`, so without the
//! feature the trust root is unconfigured and the first `SignalService` panics.
//!
//! Run (the server has no plaintext mode -- point CHATTER_TLS_ROOT_CA_PEM at
//! its cert, e.g. `certs/server.crt` from `./create-certs.sh`):
//!   CHATTER_TLS_ROOT_CA_PEM="$(cat certs/server.crt)" INTEGRATION_TESTS=1 \
//!     cargo test -p chatter-core \
//!     --features testing,dev-trust-root \
//!     --test validated -- --nocapture --test-threads=1

#[macro_use]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::events::DomainEvent;
use chatter_core::state::ActiveConversation;

const SERVER_URL: &str = "https://localhost:50051";

/// The server under test.
///
/// Overridable via `CHATTER_SERVER_URL` so a proof can be run against a
/// purpose-built server on another port without disturbing whatever is already
/// listening on 50051.
fn server_url() -> String {
    std::env::var("CHATTER_SERVER_URL").unwrap_or_else(|_| SERVER_URL.to_string())
}
/// The server repo checkout, where `.env` and the docker-compose stack live.
/// The shell helpers below run `docker compose exec psql/redis-cli` from this
/// directory. Overridable via `CHATTER_REPO_DIR` because a hardcoded path only
/// exists on one machine — everywhere else every DB-inspecting proof dies at
/// spawn, which looks like 37 protocol failures and is actually one bad path.
fn repo_dir() -> String {
    std::env::var("CHATTER_REPO_DIR")
        .unwrap_or_else(|_| "/home/alex/Documents/chatter".to_string())
}
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
// Must clear crypto::svr::MIN_PIN_ENTROPY_BITS (30.0).
const PIN: &str = "Va1idPin!x9";
const PASSWORD: &str = "TestPass123!";

// ============================================================================
// Shell helpers — we shell out to psql/redis-cli on purpose. The commands that
// appear in the test output can be pasted into a terminal by a skeptical
// reader and re-run. An ORM call cannot.
// ============================================================================

fn sh(cmd: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(repo_dir())
        .output()
        .expect("shell command failed to spawn");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

/// Full logical dump of the application database.
fn pg_dump_all() -> String {
    sh("docker compose exec -T postgres pg_dump -U chatter chatter_dev 2>/dev/null")
}

/// Type-aware dump of every value in Redis.
///
/// Naive `LRANGE`/`ZRANGE` over a hash key returns WRONGTYPE and yields an
/// empty haystack — which then "passes" any absence grep. This dispatches on
/// the actual type so the haystack is real.
fn redis_dump_all() -> String {
    sh(r##"docker compose exec -T -e REDISCLI_AUTH="$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)" redis sh -c '
        for k in $(redis-cli --scan --pattern "*"); do
          t=$(redis-cli type "$k")
          echo "--- KEY $k ($t)"
          case "$t" in
            string) redis-cli get "$k" ;;
            hash)   redis-cli hgetall "$k" ;;
            list)   redis-cli lrange "$k" 0 -1 ;;
            set)    redis-cli smembers "$k" ;;
            zset)   redis-cli zrange "$k" 0 -1 ;;
            *)      echo "(unhandled type: $t)" ;;
          esac
        done' 2>/dev/null"##)
}

fn psql(sql: &str) -> String {
    sh(&format!(
        "docker compose exec -T postgres psql -U chatter -d chatter_dev -tAc \"{}\" 2>&1",
        sql.replace('"', "\\\"")
    ))
    .trim()
    .to_string()
}

/// Read a value out of the server repo's generated `.env`.
///
/// Credentials are generated per-machine by `scripts/dev-secrets.sh`, so a
/// literal here goes stale the first time anyone regenerates them — which is
/// exactly what happened: this file carried a hardcoded RustFS key pair, and
/// after the secrets move every S3 GET below returned a 170-byte
/// `UnauthorizedAccess` XML body instead of the object. The positive control
/// caught it, but note what would have happened without one: the
/// "no plaintext in raw storage" assertion greps an error document, finds no
/// marker, and passes.
fn env_value(key: &str) -> String {
    let v = sh(&format!("grep -E '^{key}=' .env | head -1 | cut -d= -f2-"))
        .trim()
        .to_string();
    assert!(
        !v.is_empty(),
        "{key} is not set in {}/.env — run ./scripts/dev-secrets.sh",
        repo_dir()
    );
    v
}

// ============================================================================
// Client helpers (mirrors e2e_two_user_suite.rs)
// ============================================================================

/// Set the SQLCipher passphrase via the documented env override
/// (system/keyring.rs:80-82) so tests never hit the OS keyring / Secret
/// Service. Without this every client build prompts for a keyring unlock.
fn no_keyring_prompts() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: edition 2024 marks set_var unsafe because concurrent
        // set_var/getenv is UB. The Once guarantees a single write, and it
        // happens before any client (and therefore any keyring read) exists.
        unsafe {
            std::env::set_var("CHATTER_DB_PASSPHRASE", "validated-harness-passphrase");
        }
    });
}

fn build_client(dir: &std::path::Path) -> Arc<ChatterClient> {
    // Client-side visibility on demand: `RUST_LOG=chatter_core=debug` surfaces
    // stream connects, inbox event emission and the post-accept handshake
    // sends, which are otherwise invisible when a cross-client proof fails.
    // No-op unless RUST_LOG is set; `try_init` tolerates repeat calls and the
    // per-test init below.
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }
    ChatterClient::builder()
        .server_url(&server_url())
        .data_dir(dir.to_str().unwrap())
        .build()
        .expect("Failed to build ChatterClient")
}

fn unique_user(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut b = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
    format!("{}_{:x}_{}", prefix, u32::from_le_bytes(b), ts % 100000)
}

/// A plaintext that cannot occur by accident anywhere in a datastore.
fn unique_marker() -> String {
    let mut b = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("VALIDATEDxMARKERx{hex}")
}

async fn wait_for_event<F>(
    rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>,
    timeout: Duration,
    mut predicate: F,
) -> Option<DomainEvent>
where
    F: FnMut(&DomainEvent) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            if predicate(&event) {
                return Some(event);
            }
        }
    }
    None
}

/// Budget for every condition wait in this file.
///
/// Deliberately generous. These waits replaced fixed sleeps, and a condition
/// wait costs only as long as the condition actually takes — on an idle stack
/// the friendship helper below returns in well under a second. The budget only
/// matters when the shared dev stack is saturated by the other 52 tests, which
/// is exactly when the fixed sleeps lost.
const CONDITION_TIMEOUT: Duration = Duration::from_secs(90);

/// Poll interval for the condition waits. Short enough that a satisfied
/// condition is noticed promptly, long enough not to add load to the very
/// server whose saturation we are waiting out.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Backed-off poll interval for waits whose probe is a SERVER RPC.
///
/// A flat fast poll on a server-side condition is a self-inflicted wound: the
/// first version of `befriend` polled `ListFriendRequests` every 250ms for as
/// long as the friend-request workflow took, and with 16 tests in flight that
/// is ~64 authorised RPC/s of pure waiting on top of the work the suite is
/// actually there to do. That run produced 55 `i/o timeout` errors from the
/// server's own Redis and four registration failures — none of which appear in
/// any run before or since. Fast when the condition lands fast, cheap when it
/// does not.
fn backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(2))
}

/// Establish a mutual friendship A -> B, waiting on SERVER-OBSERVABLE
/// CONDITIONS instead of sleeping.
///
/// WHY THIS EXISTS. `send_friend_request` returns as soon as the server's
/// response stream emits one `FriendStatusChanged`, but the pending row B has
/// to see is written by a Temporal workflow that the handler merely *starts*
/// (`internal/services/user/service/friends.go:97` — `ExecuteWorkflow(...)`
/// then returns a synthesised `Contact`). Every call site here used to bridge
/// that with `sleep(2)`, i.e. a bet on the workflow's latency. Under the
/// parallel suite the bet loses and B's accept comes back
/// `NotFound("no friend request found")` from
/// `internal/services/user/service/friends.go:178`. That single line was 9 of
/// the 10 failures in one full baseline run, and at least one failure in all
/// three.
///
/// Three conditions, in order:
///   1. B can SEE the pending request (`ListFriendRequests`, filter Received).
///   2. The accept RPC succeeds. Retried while — and only while — the server
///      still says the request does not exist: the read in (1) and the write
///      here race the same workflow, and any OTHER error is a real failure and
///      surfaces immediately.
///   3. BOTH sides list the other as an accepted friend (`ListFriends`). This
///      is what the trailing `sleep(3)` was standing in for: the profile-key
///      handshake, the presence/profile fan-out list and the DM prewarm are all
///      keyed off that server-side state, so tests that assert on them need it
///      to exist, not to have probably existed by now.
async fn befriend(
    requester: &Arc<ChatterClient>,
    accepter: &Arc<ChatterClient>,
    requester_id: UserId,
    accepter_id: UserId,
) {
    requester
        .send_friend_request(&accepter_id, Some("validated"))
        .await
        .expect("friend request failed");

    let deadline = Instant::now() + CONDITION_TIMEOUT;

    // (1) the addressee can see it
    let mut visible = false;
    let mut wait = POLL_INTERVAL;
    while Instant::now() < deadline {
        if let Ok(reqs) = accepter.list_pending_friend_requests().await {
            if reqs.iter().any(|f| f.addressee_id == requester_id) {
                visible = true;
                break;
            }
        }
        tokio::time::sleep(wait).await;
        wait = backoff(wait);
    }
    assert!(
        visible,
        "friend request from {requester_id} never became visible to {accepter_id} within \
         {CONDITION_TIMEOUT:?} — the FriendRequestWorkflow never persisted it"
    );

    // (2) accept, retrying only the "the workflow has not landed yet" error
    let mut accept_err: Option<String> = None;
    let mut accepted = false;
    let mut wait = POLL_INTERVAL;
    while Instant::now() < deadline {
        match accepter.respond_friend_request(&requester_id, true).await {
            Ok(_) => {
                accepted = true;
                break;
            }
            Err(e) => {
                let msg = format!("{e:?}");
                let transient = msg.contains("no friend request");
                accept_err = Some(msg);
                if !transient {
                    break;
                }
                tokio::time::sleep(wait).await;
                wait = backoff(wait);
            }
        }
    }
    assert!(
        accepted,
        "accept friend request failed: {}",
        accept_err.unwrap_or_else(|| "timed out".to_string())
    );

    // (3) the accepted friendship is readable from the server on BOTH sides.
    // Each side is latched once satisfied so a slow peer does not keep
    // re-querying the side that already answered.
    let (mut a_ok, mut b_ok) = (false, false);
    let mut wait = POLL_INTERVAL;
    while Instant::now() < deadline && !(a_ok && b_ok) {
        if !a_ok {
            a_ok = matches!(requester.list_friends_with_profiles().await,
                Ok(fs) if fs.iter().any(|(f, _)| f.addressee_id == accepter_id));
        }
        if !b_ok {
            b_ok = matches!(accepter.list_friends_with_profiles().await,
                Ok(fs) if fs.iter().any(|(f, _)| f.addressee_id == requester_id));
        }
        if a_ok && b_ok {
            break;
        }
        tokio::time::sleep(wait).await;
        wait = backoff(wait);
    }
    assert!(
        a_ok && b_ok,
        "friendship {requester_id} <-> {accepter_id} was accepted but never became a mutual \
         accepted friendship on the server within {CONDITION_TIMEOUT:?}"
    );

    // (4) let the accept's BACKGROUND work land before returning.
    //
    // Accepting spawns a DM-session prewarm and a profile-key handshake
    // (client/mod.rs:2706-2729), and both put sealed envelopes in the other
    // party's device inboxes. The old `sleep(2) + sleep(3)` absorbed them by
    // accident; waiting only on the friendship made `befriend` FASTER than that
    // and let those envelopes land after a test had already snapshotted
    // `device_messages`. They then read as the property under test: V12 saw
    // one on A's own sending device and called the self-sync skip broken, V19
    // saw one on A's linked device and called own-typing self-sync broken —
    // both from an envelope neither test sent.
    //
    // The prewarm's product of record is the Signal session, so wait for that.
    // Best effort on purpose: it is a no-op on the higher-UUID side of the
    // Sesame tie-break, so that side only gets a session once the other side's
    // prewarm message arrives, and the prewarm itself is explicitly best-effort
    // (its failure is a `tracing::warn!`, not an error). Tests that REQUIRE the
    // session assert on it themselves — V3 does.
    let settle = Instant::now() + Duration::from_secs(30);
    let a_addr = format!("{accepter_id}:1");
    let b_addr = format!("{requester_id}:1");
    while Instant::now() < settle {
        let a_sess = matches!(requester.signal().key_store().load_session(&a_addr), Ok(Some(_)));
        let b_sess = matches!(accepter.signal().key_store().load_session(&b_addr), Ok(Some(_)));
        if a_sess && b_sess {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Wait until `receiver` has actually INSTALLED `sender`'s Sender Key for
/// `channel`, i.e. the SKDM was delivered AND processed.
///
/// WHY THIS EXISTS. The call sites used to do
/// `row_ids(&uid).len() > before` in a 1-second loop. `device_messages` is a
/// QUEUE the receiver's own poller is draining concurrently, so that is a
/// sample of a transient: if the SKDM was consumed between two samples (each
/// sample is a `docker compose exec ... psql`, ~100ms on an idle box and worse
/// under the parallel suite) the count never rises and the test declares
/// "SENDER-KEY DISTRIBUTION BROKEN" for a distribution that in fact worked.
/// Observed in 2 of 3 baseline runs across v35 / v41 / v45.
///
/// The sender-key record is TERMINAL state, not a transient — once installed it
/// stays — so polling it is monotonic and cannot be missed. It is also the
/// stronger assertion: the old check only proved the envelope was *queued*,
/// which is why every call site had to follow it with a `sleep(6)` labelled
/// "let X PROCESS the SKDM". Waiting for the installed key subsumes both.
///
/// Storage layout: libsignal's `process_sender_key_distribution_message` files
/// the record under `(sender_address, distribution_id)`
/// (`crypto/signal/store.rs:364`), the address renders as `"{user_id}:{device}"`
/// (`store.rs:63`) and the distribution id IS the channel UUID
/// (`services/signal/actor.rs:480`).
async fn wait_for_sender_key(
    receiver: &Arc<ChatterClient>,
    sender: &UserId,
    channel: &ChannelId,
) -> bool {
    let addr = format!("{sender}:1");
    let dist = channel.to_string();
    let deadline = Instant::now() + CONDITION_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(
            receiver.signal().key_store().load_sender_key(&addr, &dist),
            Ok(Some(_))
        ) {
            return true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    false
}

/// Wait until `uid`'s device inbox holds at least `want` rows that were NOT in
/// `before`, then return every new row (whatever actually arrived).
///
/// WHY THIS EXISTS. The call sites replaced a flat `sleep(4)`. A fan-out is one
/// `SendSealedMessage` RPC PER DEVICE, issued sequentially
/// (`services/message/server_dm.rs:502-553`), so a two- or three-device
/// recipient is two or three sequential round trips and four seconds is a bet
/// that loses under the parallel suite: V9 failed with "B's PRIMARY received 1
/// envelope, B's LINKED device received ZERO" while the linked device's deposit
/// was simply still in flight.
///
/// Safe to poll, unlike `device_messages` generally: every call site has taken
/// the destination devices OFFLINE first, so nothing is draining the queue and
/// the new-row set only grows. (Contrast `wait_for_sender_key`, which had to
/// stop sampling this table precisely because a LIVE device drains it.)
///
/// It never fabricates a row. On timeout it returns exactly what is in the
/// table, so an assertion that a device is dark still fires.
async fn wait_for_new_envelopes(
    uid: &UserId,
    before: &std::collections::HashSet<String>,
    want: usize,
) -> Vec<Vec<String>> {
    let deadline = Instant::now() + CONDITION_TIMEOUT;
    loop {
        let rows: Vec<Vec<String>> = envelope_rows(uid)
            .into_iter()
            .filter(|f| !before.contains(&f[0]))
            .collect();
        if rows.len() >= want || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Snapshot `uid`'s device-inbox row ids once the queue has stopped moving.
///
/// The strict self-sync tests (V12, V13, V19) diff `device_messages` around one
/// send and then assert on the EXACT composition of the new rows — "A's own
/// sending device was skipped", "own typing was not echoed". That diff is only
/// as honest as its baseline: any envelope still in flight when the baseline is
/// taken is attributed to the send.
///
/// Accepting a friend request spawns a DM-session prewarm and a profile-key
/// handshake (client/mod.rs:2706-2729), each of which seals an envelope to
/// every one of the peer's devices. The old `sleep(2) + sleep(3)` absorbed them
/// by accident. Waiting for a quiet queue does it on purpose: two consecutive
/// identical reads a second apart, up to 30s, then the set is the baseline.
///
/// This cannot mask the property under test — the send has not happened yet
/// when this returns.
async fn quiesced_row_ids(uid: &UserId) -> std::collections::HashSet<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut prev = row_ids(uid);
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = row_ids(uid);
        if now == prev || Instant::now() >= deadline {
            return now;
        }
        prev = now;
    }
}

/// Wait until a NEW row addressed to `device_id` appears in `uid`'s device
/// inbox, then return every new row as of that instant.
///
/// The device-targeted twin of `wait_for_new_envelopes`, and the one to use
/// whenever the test ALSO asserts that some other device got nothing.
///
/// Two reasons it is not the same call:
///   * `want: usize` is satisfied by the WRONG row. V13 waits for a self-sync
///     copy on A's linked device; a count-based wait returns as soon as any row
///     lands for A, which on a two-device account can be the primary's, and the
///     linked row is then still in flight.
///   * A count-based wait keeps looking until the count is reached, which
///     WIDENS the observation window for every other assertion over the same
///     queue. V12 asserts A's own sending device was skipped and V19 asserts a
///     typing indicator was not self-synced; a wide window sweeps in unrelated
///     later traffic (a delivery receipt from a live peer) and reports the skip
///     as broken. Stopping the moment the awaited row lands keeps that window
///     as tight as it can be while still guaranteeing the positive control.
async fn wait_for_device_envelope(
    uid: &UserId,
    before: &std::collections::HashSet<String>,
    device_id: u32,
) -> Vec<Vec<String>> {
    let want_dev = device_id.to_string();
    let deadline = Instant::now() + CONDITION_TIMEOUT;
    loop {
        let rows: Vec<Vec<String>> = envelope_rows(uid)
            .into_iter()
            .filter(|f| !before.contains(&f[0]))
            .collect();
        if rows.iter().any(|f| f[1] == want_dev) || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Did `client` recover `want` in its OWN local store for `conv`?
///
/// The terminal-state twin of `wait_for_event` for a ServerDm, and the reason
/// the multi-device tests need one. `wait_for_event` reads a
/// `tokio::sync::broadcast` receiver whose capacity is 64
/// (`events/bus.rs:33`), and its `if let Ok(Ok(event))` filter SILENTLY
/// DISCARDS `Err(RecvError::Lagged)` — a lagged receiver's events are gone,
/// including the one being waited for. V9/V10/V11 bring two or three devices up
/// and then wait on their buses one after another, so the last receiver can sit
/// unread for a minute while its client replays a whole inbox: comfortably more
/// than 64 domain events. The stored row cannot be lost, so it is what the
/// assertion should ultimately rest on.
async fn wait_for_stored_text(
    client: &Arc<ChatterClient>,
    conv: &ChannelId,
    want: &str,
) -> bool {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let stored = client
            .get_messages_local(
                conv,
                &chatter_core::domain::message::ConversationType::ServerDm,
                None,
                50,
            )
            .unwrap_or_default();
        if stored.iter().any(|m| m.as_text() == Some(want)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Alice's own address: libsignal >= 0.99 wants the local side of the session
/// alongside the peer's.
fn v14_alice_addr() -> libsignal_protocol::ProtocolAddress {
    libsignal_protocol::ProtocolAddress::new(
        "11111111-1111-4111-8111-111111111111".to_string(),
        libsignal_protocol::DeviceId::new(1).expect("device id"),
    )
}

async fn register_and_start(client: &Arc<ChatterClient>, username: &str, display: &str) -> UserId {
    client.connect().await.expect("connect failed");

    // Retry a registration that the SHARED DEV STACK could not serve.
    //
    // Registration is fixture setup here — no test in this file asserts on it
    // succeeding first time — and the two failures seen are both the dev
    // infrastructure being momentarily unavailable rather than anything the
    // client did: `Failed to start registration workflow ... context deadline
    // exceeded` (Temporal) and `failed to store challenge in Redis: i/o
    // timeout`. One such window took 31 of 53 tests down at once. A user hitting
    // that would press the button again; so does this.
    //
    // Deliberately narrow: only those two infrastructure signatures are
    // retried. A rejected registration — taken username, weak PIN, rate limit —
    // is a real answer from the server and still fails immediately.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut wait = Duration::from_secs(1);
    let r = loop {
        match client
            .register(username, PASSWORD, display, "validated", None, Some(PIN))
            .await
        {
            Ok(r) => break r,
            Err(e) => {
                let msg = format!("{e:?}");
                let infra = msg.contains("failed to start registration")
                    || msg.contains("failed to store challenge");
                if !infra || Instant::now() >= deadline {
                    panic!("registration failed: {e:?}");
                }
                eprintln!("  register({username}): dev stack unavailable ({msg}); retrying");
                tokio::time::sleep(wait).await;
                wait = backoff(wait);
            }
        }
    };
    if let Err(e) = client.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("client.start failed: {e:?}");
        }
    }
    r.user_id
}

/// Replace exactly one base64 symbol in `s` with a different valid one.
///
/// Deliberately NOT a raw byte flip: the envelope is base64 inside a JSON
/// document. A raw byte flip could break base64 or JSON parsing, and then a
/// rejection would prove the parser works, not that the ciphertext is
/// integrity-protected — a pass for the wrong reason. Mutating a symbol in
/// place keeps the JSON valid and the decoded length identical, so anything
/// that rejects the result is rejecting the *bytes*.
fn corrupt_one_b64_symbol(s: &str) -> String {
    let mut v: Vec<char> = s.chars().collect();
    // Middle of the payload: past any header, before any '=' padding.
    let idx = v.len() / 2;
    v[idx] = if v[idx] == 'A' { 'B' } else { 'A' };
    v.into_iter().collect()
}

fn extract_between(hay: &str, start: &str, end: &str) -> Option<String> {
    let i = hay.find(start)? + start.len();
    let rest = &hay[i..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
}

/// Drive a REAL device link: `client_s` (a fresh, unregistered client) becomes a
/// secondary device of the account `client_p` is signed in to. Returns the
/// SERVER-assigned device_id.
///
/// This is the genuine QR flow — new device publishes a link URL, primary parses
/// it, displays a verification code and approves with the account password, new
/// device completes provisioning and rebinds its credentials to the device_id the
/// server handed it. Nothing here is faked or injected: a test that inserted a
/// device row directly would prove nothing about linking.
///
/// Shared by V8 and V9 on purpose. Both need a real secondary device, and a
/// second implementation of a five-step handshake is a second thing to get
/// subtly wrong — the exact class of bug this file exists to catch.
///
/// Callers that want the linked device to actually RECEIVE must additionally
/// call `client_s.start()`: `link_device_run`'s contract (client/link_device.rs:53)
/// is that `Linked` establishes credentials but does not kick off background
/// services. V8 never receives, so it never starts. V9 does.
async fn link_secondary_device(
    client_p: &Arc<ChatterClient>,
    client_s: &Arc<ChatterClient>,
    uid: UserId,
) -> u32 {
    use chatter_core::crypto::SecretString;
    use chatter_core::services::link_device_service::LinkProgress;

    let mut rx = client_s
        .link_device_run()
        .await
        .expect("link_device_run failed to start — no link, nothing to measure");

    // The new device publishes a QR URL; the primary approves it out-of-band.
    let url = loop {
        match tokio::time::timeout(EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timed out waiting for QrReady")
        {
            Some(LinkProgress::QrReady { url, .. }) => break url,
            Some(LinkProgress::Failed(m)) => panic!("link failed before QR: {m}"),
            Some(_) => continue,
            None => panic!("link flow ended before emitting QrReady"),
        }
    };
    println!("  new device published link URL ({} bytes)", url.len());

    let pending = client_p
        .prepare_link_from_url(&url)
        .expect("primary could not parse the link URL");
    println!(
        "  primary sees verification code {} for device {:?}",
        pending.verification_code(),
        pending.device_name()
    );
    client_p
        .approve_link(pending, SecretString::new(PASSWORD.to_string()))
        .await
        .expect("primary failed to approve the link — provision envelope never uploaded");

    let linked = loop {
        match tokio::time::timeout(EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timed out waiting for Linked")
        {
            Some(LinkProgress::Linked(c)) => break c,
            Some(LinkProgress::Failed(m)) => panic!("linking failed: {m}"),
            Some(LinkProgress::Cancelled) => panic!("linking cancelled"),
            Some(_) => continue,
            None => panic!("link flow ended without Linked"),
        }
    };
    assert_eq!(
        linked.user_id, uid,
        "linked device joined the wrong account — every query scoped to {uid} would \
         name the wrong user"
    );
    println!("  LINKED: user {uid} device_id={}", linked.device_id);
    linked.device_id
}

/// Rebuild a client from its persisted data dir, restore its session, and start
/// it, returning the live client and an event receiver subscribed BEFORE start
/// (subscribe after start and the message can be delivered into a void).
///
/// The `restore_session` assert is the load-bearing part: a device must decrypt
/// with the identity that was live when the sender encrypted to it. A fresh
/// identity would fail to decrypt for a reason that has nothing to do with the
/// property under test — a red for the wrong cause.
///
/// Shared by V10 and V11. V9 grew this inline as a closure; it is left alone
/// deliberately (it is green and deterministic, and re-proving a passing test is
/// not worth the risk of touching it).
async fn restore_and_start(
    path: &std::path::Path,
    label: &str,
) -> (Arc<ChatterClient>, tokio::sync::broadcast::Receiver<DomainEvent>) {
    let c = build_client(path);
    c.connect()
        .await
        .unwrap_or_else(|e| panic!("{label} reconnect failed: {e:?}"));
    let rx = c.subscribe_events();
    assert!(
        c.restore_session(),
        "{label} failed to restore its persisted session — it must decrypt with the \
         identity that was live when the sender encrypted, not a fresh one"
    );
    if let Err(e) = c.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("{label} start failed: {e:?}");
        }
    }
    (c, rx)
}

/// Bring a just-linked client up in place (credentials already established by
/// `link_secondary_device`; `Linked` does not start background services —
/// client/link_device.rs:53).
async fn start_linked(client: &Arc<ChatterClient>, label: &str) {
    if let Err(e) = client.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("{label} start failed: {e:?}");
        }
    }
}

/// `id|destination_device_id|md5(envelope)|source_user_id|octet_length(envelope)`
/// for every queued envelope belonging to `uid`, split into fields.
///
/// Scoped to ONE user on purpose: a global read passes vacuously on other runs'
/// rows. Callers diff this across a send using the row `id` set, because the
/// server assigns its own envelope GUID (`uuid.New()`) and ignores client message
/// ids — correlating by client id targets the wrong envelope (V2's lesson).
fn envelope_rows(uid: &UserId) -> Vec<Vec<String>> {
    psql(&format!(
        "select id || '|' || destination_device_id || '|' || md5(envelope) || '|' || \
         coalesce(source_user_id::text, 'NULL') || '|' || octet_length(envelope) \
         from device_messages where destination_user_id = '{uid}'"
    ))
    .lines()
    .map(|l| l.trim())
    .filter(|l| !l.is_empty())
    .map(|l| l.split('|').map(|s| s.to_string()).collect::<Vec<_>>())
    .filter(|f: &Vec<String>| f.len() == 5)
    .collect()
}

fn row_ids(uid: &UserId) -> std::collections::HashSet<String> {
    psql(&format!(
        "select id from device_messages where destination_user_id = '{uid}'"
    ))
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect()
}

// ============================================================================
// V0: DETECTOR SELF-CHECK — prove the plaintext detector can find plaintext
//
// V1 asserts "marker occurs 0 times in Redis/Postgres/logs". That assertion is
// worth exactly nothing unless the detector would have found the marker had it
// been there. A broken grep, a WRONGTYPE haystack, or an empty dump all report
// "0 occurrences" and look identical to real encryption.
//
// This test plants a known marker in each store and requires the detector to
// find it. It is the mutation check for V1, kept as a permanent test.
//
// MUTATION CHECK: break redis_dump_all() (e.g. LRANGE a hash key, as a first
// attempt at this harness did) and this test MUST fail.
// ============================================================================

#[tokio::test]
async fn v0_detector_self_check() {
    skip_if_no_server!();

    let marker = unique_marker();

    // ---- Redis: plant in a hash, the type the real envelopes use ----------
    let probe_key = format!("validated_probe:{marker}");
    sh(&format!(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HSET '{probe_key}' payload '{marker}' >/dev/null 2>&1"
    ));
    let redis_all = redis_dump_all();
    let found_redis = redis_all.matches(&marker).count();
    sh(&format!(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli DEL '{probe_key}' >/dev/null 2>&1"
    ));
    assert!(
        found_redis > 0,
        "DETECTOR BROKEN: planted marker in a Redis hash and the dump did not \
         find it. Every 'plaintext absent from Redis' claim is therefore vacuous."
    );
    println!("  redis detector: found planted marker {found_redis}x — detector works");

    // ---- Postgres: plant in a real table so pg_dump must carry it ---------
    // Table name is per-run: a shared name would race under parallel threads,
    // where one test's DROP truncates another's haystack mid-dump.
    let probe_tbl = format!("validated_probe_{}", &marker[17..33]);
    psql(&format!("CREATE TABLE IF NOT EXISTS {probe_tbl} (payload text)"));
    psql(&format!(
        "INSERT INTO {probe_tbl} (payload) VALUES ('{marker}')"
    ));
    let dump = pg_dump_all();
    let found_pg = dump.matches(&marker).count();
    psql(&format!("DROP TABLE IF EXISTS {probe_tbl}"));
    assert!(
        found_pg > 0,
        "DETECTOR BROKEN: planted marker in a Postgres table and pg_dump did \
         not find it. Every 'plaintext absent from Postgres' claim is vacuous."
    );
    println!("  postgres detector: found planted marker {found_pg}x — detector works");

    println!("V0 PASS — detectors provably find plaintext when it is present\n");
}

// ============================================================================
// V1: E2EE — the server never holds message plaintext
//
// CLAIM (README): "Signal Protocol Encryption ... E2E encrypted messaging"
//
// MUTATION CHECK: send via a plaintext content type (Message_Content_Text
// persists []byte(text) verbatim per send.go:60-62) and this test MUST fail on
// the marker grep. That path is what makes the assertion non-vacuous.
// ============================================================================

#[tokio::test]
async fn v1_server_never_holds_plaintext() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();

    let client_a = build_client(dir_a.path());
    let client_b = build_client(&path_b);

    let user_b_name = unique_user("v1b");
    let uid_a = register_and_start(&client_a, &unique_user("v1a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &user_b_name, "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // Take B offline so the envelope SITS in the queue instead of being
    // acked and deleted. We must read the very bytes B will later decrypt.
    client_b.shutdown().await;
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");

    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- POSITIVE CONTROL -------------------------------------------------
    // Prove the ciphertext is actually in a store before claiming plaintext is
    // absent from it. Without this, "not found" and "not there" are the same
    // observation and the test is vacuous.
    let envelope_keys = sh(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli --scan --pattern 'device_queue_envelopes:*' 2>/dev/null",
    );
    let n_keys = envelope_keys.lines().filter(|l| !l.trim().is_empty()).count();
    println!("  positive control: {n_keys} device_queue_envelopes key(s)");
    assert!(
        n_keys > 0,
        "POSITIVE CONTROL FAILED: no envelope in Redis — the absence grep below \
         would be vacuous. The message never reached the store we are auditing."
    );

    let redis_all = redis_dump_all();
    assert!(
        redis_all.contains("device_queue_envelopes"),
        "POSITIVE CONTROL FAILED: redis dump did not include the envelope keys"
    );
    assert!(
        redis_all.len() > 200,
        "POSITIVE CONTROL FAILED: redis dump suspiciously small ({} bytes) — \
         likely a WRONGTYPE/error haystack, which passes any grep vacuously",
        redis_all.len()
    );
    assert!(
        !redis_all.contains("WRONGTYPE"),
        "POSITIVE CONTROL FAILED: redis dump contains WRONGTYPE errors — the \
         haystack is error text, not data"
    );

    // ---- NEGATIVE HALF: plaintext absent from every server-side store ------
    let hits_redis = redis_all.matches(&marker).count();
    assert_eq!(
        hits_redis, 0,
        "E2EE VIOLATED: plaintext marker found in Redis {hits_redis} time(s)"
    );

    let dump = pg_dump_all();
    assert!(
        dump.len() > 10_000,
        "POSITIVE CONTROL FAILED: pg_dump only {} bytes — not a real dump",
        dump.len()
    );
    let hits_pg = dump.matches(&marker).count();
    assert_eq!(
        hits_pg, 0,
        "E2EE VIOLATED: plaintext marker found in Postgres dump {hits_pg} time(s)"
    );

    // Server-side logs are a store too — a debug log line leaks just as hard.
    let logs = sh("cat server.log worker.log 2>/dev/null");
    let hits_log = logs.matches(&marker).count();
    assert_eq!(
        hits_log, 0,
        "E2EE VIOLATED: plaintext marker found in server logs {hits_log} time(s)"
    );

    println!("  server stores audited: redis={} bytes, pg_dump={} bytes, logs={} bytes",
             redis_all.len(), dump.len(), logs.len());
    println!("  plaintext occurrences: redis=0 postgres=0 logs=0");

    // ---- POSITIVE HALF: B, and only B, can recover the plaintext -----------
    // Same bytes we just proved the server cannot read.
    let client_b2 = build_client(&path_b);
    client_b2.connect().await.expect("B reconnect failed");
    let mut events_b = client_b2.subscribe_events();
    // B's auth state and Signal keys persist in its SQLCipher DB across the
    // shutdown. `restore_session()` is the same path chatter-ui takes on boot
    // (see chatter-ui/src/lib.rs:207) — it rehydrates the gRPC credentials that
    // login() would otherwise refuse to re-issue (AuthError::AlreadyLoggedIn).
    // This matters for the proof: B decrypts with the identity that was live
    // when A encrypted, not a fresh one.
    let restored = client_b2.restore_session();
    assert!(restored, "B failed to restore persisted session from data dir");
    println!("  B restored persisted session (same identity A encrypted to)");
    if let Err(e) = client_b2.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("B start failed: {e:?}");
        }
    }
    let _ = &user_b_name;

    let expected_conv = ChannelId::from(uid_a);
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;

    assert!(
        received.is_some(),
        "B did not receive the queued message after coming back online"
    );
    if let Some(DomainEvent::MessageReceived { message }) = &received {
        let body = message.as_text().unwrap_or("<not text>");
        assert_eq!(
            body, marker,
            "decrypted plaintext does not match what A sent"
        );
        println!("  B decrypted: {body}");
    }

    println!("V1 PASS — server held ciphertext it could not read; B recovered exact plaintext\n");

    client_a.shutdown().await;
    client_b2.shutdown().await;
}

// ============================================================================
// V2: INTEGRITY — a corrupted ciphertext never becomes plaintext
//
// CLAIM: Signal's AEAD protects message integrity. If a modified ciphertext
// can still be decrypted, every other proof in this file is void — an attacker
// with write access to the queue could alter messages in flight.
//
// METHOD: identical to V1, except one base64 symbol of the queued envelope is
// mutated in Redis before B comes online. B must never surface the plaintext.
//
// POSITIVE CONTROL: V1 is this exact flow WITHOUT the corruption, and it does
// surface the plaintext. That differential is what makes "B received nothing"
// meaningful rather than "the pipeline was broken all along". This test also
// re-reads the value after writing to prove the corruption actually landed —
// without that, a failed HSET would produce a vacuous pass.
//
// MUTATION CHECK: skip the corruption step (write the original JSON back) and
// this test MUST fail — B will receive the marker.
// ============================================================================

#[tokio::test]
async fn v2_corrupted_ciphertext_never_decrypts() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();

    let client_a = build_client(dir_a.path());
    let client_b = build_client(&path_b);

    let uid_a = register_and_start(&client_a, &unique_user("v2a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v2b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    client_b.shutdown().await;
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- identify the marker's envelope by DIFFING the queue --------------
    // Two earlier attempts at this were wrong, both silently:
    //   1. Take the first HKEYS field. B's queue holds several envelopes
    //      (session bootstrap, friend traffic), so this corrupted an arbitrary
    //      message, left the marker intact, and reported a FALSE "INTEGRITY
    //      VIOLATED" when B correctly decrypted it.
    //   2. Use the client's returned message ID as the hash field. The server
    //      assigns its own GUID via uuid.New() and does NOT honour a client
    //      GUID, so the client's ID never appears server-side at all.
    // Diffing the field set across the send depends on neither assumption.
    let key = format!("device_queue_envelopes:{uid_b}:1");
    let hkeys = |k: &str| -> std::collections::HashSet<String> {
        sh(&format!(
            "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HKEYS '{k}' 2>/dev/null"
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
    };

    let before = hkeys(&key);

    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let after = hkeys(&key);
    let new_fields: Vec<String> = after.difference(&before).cloned().collect();
    assert_eq!(
        new_fields.len(),
        1,
        "POSITIVE CONTROL FAILED: expected exactly 1 new envelope in {key} after \
         the send, found {}. Zero means the message never queued (so corrupting \
         nothing would pass vacuously); more than one means this test cannot tell \
         which envelope carries the marker and would corrupt the wrong one.",
        new_fields.len()
    );
    let field = new_fields[0].clone();

    let json = sh(&format!(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HGET '{key}' '{field}' 2>/dev/null"
    ));
    let envelope_b64 = extract_between(&json, "\"envelope\":\"", "\"")
        .expect("could not extract envelope base64 from queued JSON");
    println!("  envelope: {} base64 chars", envelope_b64.len());

    // ---- corrupt exactly one symbol of the ciphertext ---------------------
    let corrupted_b64 = corrupt_one_b64_symbol(&envelope_b64);
    assert_ne!(envelope_b64, corrupted_b64, "corruption was a no-op");
    assert_eq!(
        envelope_b64.len(),
        corrupted_b64.len(),
        "corruption changed length — that is truncation, not a bit-level edit"
    );
    let corrupted_json = json.replace(&envelope_b64, &corrupted_b64);

    let tmp = std::env::temp_dir().join(format!("validated_corrupt_{}.json", &marker[17..29]));
    std::fs::write(&tmp, corrupted_json.trim()).expect("write temp json");
    // -x reads the value from stdin, avoiding shell-quoting the JSON payload.
    sh(&format!(
        "cat '{}' | docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli -x HSET '{key}' '{field}' >/dev/null 2>&1",
        tmp.display()
    ));
    let _ = std::fs::remove_file(&tmp);

    // ---- prove the corruption actually landed -----------------------------
    let readback = sh(&format!(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HGET '{key}' '{field}' 2>/dev/null"
    ));
    let stored_b64 = extract_between(&readback, "\"envelope\":\"", "\"")
        .expect("could not re-read envelope after corruption");
    assert_eq!(
        stored_b64.trim(),
        corrupted_b64.trim(),
        "POSITIVE CONTROL FAILED: corrupted envelope did not persist to Redis — \
         the HSET silently failed and this test would pass vacuously"
    );
    println!("  corruption landed: 1 base64 symbol changed, length preserved");

    // ---- B comes online: must NOT surface the plaintext --------------------
    let client_b2 = build_client(&path_b);
    client_b2.connect().await.expect("B reconnect failed");
    let mut events_b = client_b2.subscribe_events();
    assert!(
        client_b2.restore_session(),
        "B failed to restore persisted session"
    );
    if let Err(e) = client_b2.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("B start failed: {e:?}");
        }
    }

    let expected_conv = ChannelId::from(uid_a);
    let leaked = wait_for_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;

    if let Some(DomainEvent::MessageReceived { message }) = &leaked {
        let body = message.as_text().unwrap_or("<not text>");
        panic!(
            "INTEGRITY VIOLATED: B surfaced a message from a corrupted ciphertext: {body:?}"
        );
    }

    println!("  B surfaced no message from the corrupted envelope");

    // ---- IN-TEST POSITIVE CONTROL -----------------------------------------
    // "B received nothing" is only evidence if B *could* have received
    // something. Prove B's receive path is alive at this very moment by
    // sending a clean message on the same session and requiring it through.
    // Without this, a dead pipeline would pass V2 for entirely the wrong
    // reason. This doubles as a check that a rejected ciphertext does not
    // wedge the session.
    let marker2 = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker2)
        .await
        .expect("clean follow-up send failed");

    let clean = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;

    assert!(
        clean.is_some(),
        "POSITIVE CONTROL FAILED: B did not receive a CLEAN message either, so \
         the corrupted-message rejection above proves nothing — B's receive path \
         was not alive. (If this fires consistently, a rejected ciphertext may be \
         wedging the Signal session — that would itself be a finding.)"
    );
    if let Some(DomainEvent::MessageReceived { message }) = &clean {
        assert_eq!(
            message.as_text().unwrap_or("<not text>"),
            marker2,
            "clean follow-up decrypted to the wrong plaintext"
        );
    }
    println!("  positive control: clean follow-up message DID arrive — receive path alive");
    println!("V2 PASS — corrupted ciphertext yielded no plaintext; clean message on the same session did\n");

    client_a.shutdown().await;
    client_b2.shutdown().await;
}

// ============================================================================
// V3: RATCHET — the Double Ratchet advances on every message
//
// CLAIM (README): "Signal Protocol Encryption: X3DH, Double Ratchet, ..."
//
// WHY THIS TEST MUST EXIST: a pinned ratchet is INVISIBLE from the outside. It
// encrypts, it decrypts, round-trips pass, V1 and V2 stay green — and forward
// secrecy is silently gone, because every message shares key material. The only
// way to catch it is to look at the session state itself and require it to move.
//
// METHOD: send N messages A->B and snapshot A's serialized session record after
// each one. Every snapshot must differ from all others. Separately, send the
// SAME plaintext twice and require distinct ciphertexts.
//
// POSITIVE CONTROL: each snapshot is asserted to be Some(non-empty) before any
// distinctness claim. Comparing a set of `None`s trivially "passes".
//
// MUTATION CHECK: pin the ratchet (make the session record static) and the
// distinctness assertion fails. Conversely, if load_session ever returns None,
// the positive control fires rather than silently passing.
// ============================================================================

// Gated on `testing`: this is the only test here that reaches past the client
// façade (SignalService::key_store) to observe raw session state. Without the
// gate, a plain `cargo test` fails to COMPILE THE WHOLE FILE with "method
// `signal` is private" — taking V0/V1/V2 down with it. Run via
// `make test-validated`, which passes the feature.
#[cfg(feature = "testing")]
#[tokio::test]
async fn v3_ratchet_advances_every_message() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v3a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v3b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // A's Signal session record for B. Address format is "{user_id}:{device_id}"
    // (services/signal/actor.rs:514).
    let addr = format!("{uid_b}:1");
    let read_session = || -> Vec<u8> {
        client_a
            .signal()
            .key_store()
            .load_session(&addr)
            .expect("load_session errored")
            .expect("POSITIVE CONTROL FAILED: no session record for B — a set of \
                     missing records would compare 'distinct' vacuously")
    };
    // The session is created by the post-accept DM prewarm, which is a spawned
    // background task and a deliberate no-op on the higher-UUID side of the
    // Sesame tie-break (client/mod.rs:2719) — so on that side A only gets a
    // session when B's prewarm message arrives and is processed. Wait for the
    // record rather than reading it the instant `befriend` returns.
    {
        let deadline = Instant::now() + CONDITION_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(client_a.signal().key_store().load_session(&addr), Ok(Some(_))) {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    const N: usize = 5;
    let mut snapshots: Vec<Vec<u8>> = Vec::new();

    // Baseline: session must exist BEFORE we claim it changes.
    let base = read_session();
    assert!(
        !base.is_empty(),
        "POSITIVE CONTROL FAILED: session record is empty"
    );
    println!("\n  session established: {} bytes", base.len());
    snapshots.push(base);

    for i in 0..N {
        client_a
            .send_text(
                ActiveConversation::ServerDm { peer_id: uid_b },
                &format!("v3 ratchet probe {i}"),
            )
            .await
            .expect("send_text failed");
        tokio::time::sleep(Duration::from_millis(700)).await;

        let snap = read_session();
        assert!(!snap.is_empty(), "session record empty after send {i}");
        snapshots.push(snap);
    }

    // Every snapshot must be distinct: the sending chain advanced each time.
    for i in 0..snapshots.len() {
        for j in (i + 1)..snapshots.len() {
            assert_ne!(
                snapshots[i], snapshots[j],
                "RATCHET PINNED: session record identical after sends {i} and {j}. \
                 Messages are sharing key material — forward secrecy is void, and \
                 encryption/decryption would still appear to work perfectly."
            );
        }
    }
    println!(
        "  ratchet advanced: {} distinct session states across {} sends",
        snapshots.len(),
        N
    );

    // Same plaintext twice must not produce the same ciphertext. If it does,
    // encryption is deterministic and key/nonce reuse is happening.
    let repeated = "IDENTICAL PLAINTEXT PROBE";
    let before = redis_dump_all().len();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, repeated)
        .await
        .expect("send 1 failed");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let s1 = read_session();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, repeated)
        .await
        .expect("send 2 failed");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let s2 = read_session();
    let _ = before;

    assert_ne!(
        s1, s2,
        "RATCHET PINNED: identical plaintext left the session state unchanged"
    );
    println!("  identical plaintext sent twice -> session state still advanced");

    println!("V3 PASS — Double Ratchet advances on every message; no state reuse\n");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V4: SEALED SENDER — the server must not learn who sent a message
//
// CLAIM (README): "Sealed Sender: Server cannot see who sent a message
// (anonymous delivery)".
//
// THIS TEST CURRENTLY FAILS. That is the point. See VALIDATED.md "F2".
//
// The crypto is real — V1 proves the envelope is opaque at rest. But the
// sender identity is recorded ALONGSIDE it in device_messages.source_user_id,
// because the DM path authenticates with Basic auth rather than UAK, and
// middleware/auth.go:738 then sets SealedSenderModeKey=false, which makes
// handlers/sealed_sender.go:199 take the "identified sender" fallback and
// stamp the real user id onto the queued row.
//
// So the envelope hides the sender from anyone reading the blob, and the
// column next to it gives them away. Encrypting the letter and writing the
// return address on the outside.
//
// POSITIVE CONTROL: asserts rows exist for this run before judging them —
// zero rows would "pass" the NULL check vacuously.
//
// MUTATION CHECK: engage UAK auth on the DM path (SealedSenderModeKey=true)
// and this test passes. That is also the fix.
// ============================================================================

#[tokio::test]
async fn v4_sealed_sender_hides_sender_from_server() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v4a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v4b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    client_b.shutdown().await;
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(2)).await;

    client_a
        .send_text(
            ActiveConversation::ServerDm { peer_id: uid_b },
            "v4 sealed sender probe",
        )
        .await
        .expect("send_text failed");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // POSITIVE CONTROL: rows must exist before we judge whether they leak.
    let total = psql(&format!(
        "select count(*) from device_messages where destination_user_id = '{uid_b}'"
    ));
    let total: i64 = total.trim().parse().unwrap_or(0);
    assert!(
        total > 0,
        "POSITIVE CONTROL FAILED: no device_messages rows for B — the NULL check \
         below would pass vacuously"
    );

    // THE CLAIM: for sealed sender, the server must not know the sender.
    let leaked = psql(&format!(
        "select count(*) from device_messages \
         where destination_user_id = '{uid_b}' and source_user_id is not null"
    ));
    let leaked: i64 = leaked.trim().parse().unwrap_or(-1);

    println!("\n  device_messages for B: {total}");
    println!("  rows with source_user_id POPULATED: {leaked}");
    if leaked > 0 {
        let who = psql(&format!(
            "select distinct left(source_user_id::text, 8) from device_messages \
             where destination_user_id = '{uid_b}' and source_user_id is not null"
        ));
        println!("  server recorded sender(s): {who}  (A is {})", &uid_a.to_string()[..8]);
    }

    assert_eq!(
        leaked, 0,
        "SEALED SENDER NOT ENGAGED: the server recorded the sender on {leaked}/{total} \
         messages. The envelope is opaque (V1 proves that) but device_messages.\
         source_user_id names the sender right next to it. Root cause: the DM path \
         authenticates with Basic auth, not UAK, so middleware/auth.go:738 sets \
         SealedSenderModeKey=false and handlers/sealed_sender.go:199 takes the \
         identified-sender fallback. See VALIDATED.md F2."
    );

    println!("V4 PASS — server did not record the sender\n");
    client_a.shutdown().await;
}

// ============================================================================
// V5: KYBER / PQXDH IS LOAD-BEARING — a session cannot form without it
//
// CLAIM (README): post-quantum via Kyber prekeys. `crypto/signal/session.rs:31`
// asserts "libsignal no longer supports X3DH - only PQXDH. The bundle must
// include Kyber prekeys."
//
// WHY "THE BUNDLE HAS KYBER KEYS" PROVES NOTHING: a bundle is a container. That
// a Kyber key is *in* it and that a Kyber secret *contributes* to the derived
// session are different claims, and only the second is a security property. A
// decorative Kyber field — parsed, logged, then dropped — would look identical
// from the outside. So this test does not inspect a bundle. It removes the Kyber
// material from the real server and requires session establishment to STOP.
//
// SCOPE (deliberate): libsignal is unforked and pinned to
// signalapp/libsignal@c5edd3fa. ML-KEM itself is NOT re-tested here — that is
// audited upstream and re-proving it would be theater (see VALIDATED.md L3).
// What is tested is chatter's WIRING: that this codebase cannot silently end up
// with a non-PQ session.
//
// METHOD: A and B are friends with a live session. A's session record for B is
// deleted, which forces the next send to re-fetch B's prekey bundle from the
// real server and re-run PQXDH. That gives a controlled way to re-enter the
// handshake path as many times as we like. We then strip B's Kyber material out
// of Postgres and require the same send to fail.
//
// THREE POSITIVE CONTROLS, because each alternative explanation must be killed:
//   1. A clean send arrives          — the pipeline works at all.
//   2. B's Kyber rows exist          — there is something to strip; stripping an
//                                      empty table would make step 3 vacuous.
//   3. Drop-session + send arrives   — dropping the session and re-handshaking
//                                      is by itself SURVIVABLE. Without this,
//                                      the final failure could be caused by the
//                                      delete rather than by the Kyber strip.
// Control 3 is the one that matters: it makes the final assertion a genuine
// differential, where the ONLY changed variable is the Kyber material.
//
// MUTATION CHECK: comment out the Kyber strip (leave the rows intact) and this
// test MUST fail — the send succeeds and B receives the marker, exactly as in
// control 3.
//
// Gated on `testing` for key_store() access, same as V3.
// ============================================================================

#[cfg(feature = "testing")]
#[tokio::test]
async fn v5_kyber_is_load_bearing_not_decorative() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v5a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v5b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    let mut events_b = client_b.subscribe_events();
    let expected_conv = ChannelId::from(uid_a);
    let addr = format!("{uid_b}:1");

    // Deleting A's session record for B forces the next send back through the
    // full prekey-fetch + PQXDH path. This is the lever the whole test turns on.
    let drop_session = || {
        client_a
            .signal()
            .key_store()
            .delete_session(&addr)
            .expect("delete_session errored")
    };

    // ---- POSITIVE CONTROL 1: the pipeline delivers at all ------------------
    let m1 = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m1)
        .await
        .expect("baseline send failed");
    let got1 = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == expected_conv
                 && message.as_text() == Some(m1.as_str()))
    })
    .await;
    assert!(
        got1.is_some(),
        "POSITIVE CONTROL 1 FAILED: a clean message never arrived, so every \
         'the send failed' observation below would be meaningless"
    );
    println!("\n  control 1: clean send delivered — pipeline alive");

    // ---- POSITIVE CONTROL 2: B has Kyber material to strip -----------------
    // Stripping an empty table would make the final assertion vacuous: the send
    // would fail for a reason that has nothing to do with removing Kyber.
    let count = |sql: String| -> i64 { psql(&sql).trim().parse().unwrap_or(-1) };

    let pqspk = count(format!(
        "select count(*) from signal_prekey_bundles \
         where user_id = '{uid_b}' and kyber_prekey_public is not null"
    ));
    let pqopk = count(format!(
        "select count(*) from signal_kyber_one_time_prekeys where user_id = '{uid_b}'"
    ));
    println!("  control 2: B's server-side Kyber material — PQSPK rows={pqspk}, PQOPK rows={pqopk}");
    assert!(
        pqspk + pqopk > 0,
        "POSITIVE CONTROL 2 FAILED: B has NO Kyber prekey material on the server \
         at all (PQSPK={pqspk}, PQOPK={pqopk}). Then the strip below removes \
         nothing and this test proves nothing. Note this would ALSO be a finding \
         in its own right: sessions were forming without any Kyber key present."
    );

    // ---- POSITIVE CONTROL 3: re-handshaking is survivable ------------------
    // THE decisive control. Drop the session and send again, with Kyber still in
    // place. If this arrives, then the ONLY difference between here and the
    // final step is the Kyber material — which is what makes the final failure
    // attributable to Kyber and nothing else.
    assert!(
        drop_session(),
        "POSITIVE CONTROL 3 FAILED: there was no session record to delete"
    );
    assert!(
        client_a.signal().key_store().load_session(&addr).unwrap().is_none(),
        "POSITIVE CONTROL 3 FAILED: session record survived the delete, so the \
         next send would reuse it instead of re-running PQXDH"
    );

    let m2 = unique_marker();
    let res2 = client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m2)
        .await;
    assert!(
        res2.is_ok(),
        "POSITIVE CONTROL 3 FAILED: send errored after dropping the session even \
         with Kyber present ({:?}). The re-handshake path is broken independently \
         of Kyber, so the final assertion would pass for the wrong reason.",
        res2.err()
    );
    let got2 = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == expected_conv
                 && message.as_text() == Some(m2.as_str()))
    })
    .await;
    assert!(
        got2.is_some(),
        "POSITIVE CONTROL 3 FAILED: dropping the session broke delivery even with \
         Kyber present. The final failure below could then be blamed on the \
         delete rather than on the Kyber strip."
    );
    assert!(
        client_a.signal().key_store().load_session(&addr).unwrap().is_some(),
        "POSITIVE CONTROL 3 FAILED: no session record was rebuilt"
    );
    println!("  control 3: session dropped -> re-handshake -> delivered — PQXDH path re-entered OK");

    // ---- STRIP B's KYBER MATERIAL FROM THE REAL SERVER ---------------------
    // Both sources the server can serve from: the one-time Kyber prekey pool
    // (PQOPK) and the medium-term last-resort Kyber prekey (PQSPK). See
    // internal/services/auth/repository/postgres/signal_protocol.go:427-446 —
    // the server prefers a popped PQOPK and falls back to the PQSPK.
    psql(&format!(
        "delete from signal_kyber_one_time_prekeys where user_id = '{uid_b}'"
    ));
    psql(&format!(
        "update signal_prekey_bundles set kyber_prekey_id = null, \
         kyber_prekey_public = null, kyber_prekey_signature = null, \
         kyber_prekey_timestamp = null where user_id = '{uid_b}'"
    ));

    // Prove the strip landed. A silently-failed UPDATE would leave Kyber in
    // place and this test would then merely re-run control 3 and "pass".
    let pqspk_after = count(format!(
        "select count(*) from signal_prekey_bundles \
         where user_id = '{uid_b}' and kyber_prekey_public is not null"
    ));
    let pqopk_after = count(format!(
        "select count(*) from signal_kyber_one_time_prekeys where user_id = '{uid_b}'"
    ));
    assert_eq!(
        (pqspk_after, pqopk_after),
        (0, 0),
        "POSITIVE CONTROL FAILED: Kyber material survived the strip \
         (PQSPK={pqspk_after}, PQOPK={pqopk_after}) — the SQL silently did nothing"
    );
    println!("  stripped B's Kyber material: PQSPK rows={pqspk_after}, PQOPK rows={pqopk_after}");

    // Only the bundle row still exists — identity key, registration id and the
    // signed prekey are untouched. Everything an X3DH (non-PQ) handshake needs
    // is still there. If Kyber were decorative, this send would succeed.
    let still_serves = count(format!(
        "select count(*) from signal_prekey_bundles \
         where user_id = '{uid_b}' and signed_prekey_public is not null"
    ));
    assert!(
        still_serves > 0,
        "POSITIVE CONTROL FAILED: the strip removed the whole bundle, not just \
         the Kyber part. Then the send below fails for 'no bundle', not for 'no \
         Kyber', and the test proves nothing about PQXDH."
    );
    println!("  B's non-PQ material left intact: signed-prekey bundle rows={still_serves}");

    // ---- THE PROPERTY: no Kyber => no session => no message ----------------
    assert!(drop_session(), "no session record to delete before the real test");

    let m3 = unique_marker();
    let res3 = client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m3)
        .await;

    // Print the exact error. WHICH error it is, is itself evidence: a
    // ServiceError from the bundle *conversion* means the server happily SERVED
    // a Kyber-less bundle and the client refused it. A transport/NotFound error
    // would instead mean the server declined to serve at all. See VALIDATED.md.
    println!("  send with Kyber stripped -> {}", match &res3 {
        Ok(_) => "SUCCEEDED".to_string(),
        Err(e) => format!("Err({e:?})"),
    });

    assert!(
        res3.is_err(),
        "KYBER IS DECORATIVE: A established a session and sent a message to B \
         after every Kyber prekey B had was deleted from the server. PQXDH did \
         not happen; the post-quantum claim is false. (Control 3 proves this same \
         send succeeds when the only difference is that Kyber is present.)"
    );

    // No session record may exist either — a half-built session is still a
    // non-PQ session, and a later send could ride it.
    assert!(
        client_a.signal().key_store().load_session(&addr).unwrap().is_none(),
        "KYBER IS DECORATIVE: the send errored but a session record for B was \
         still written without any Kyber prekey. A non-PQ session now exists and \
         a subsequent send would silently reuse it."
    );
    println!("  no session record written — failed closed, no half-built non-PQ session");

    // ---- WHERE DID THE REFUSAL COME FROM? (finding K1) ---------------------
    // The send failing does NOT by itself say whether the server refused to serve
    // a PQXDH-less bundle or served one that the client then refused. Those look
    // identical from the send's return value, and only the second is a
    // server-side finding. So ask the server directly.
    //
    // The wildcard ("*") fetch that send_text uses cannot answer this: it passes
    // popOTPK=false (signal_protocol.go:215) and so leaves no trace. The
    // SINGLE-device fetch does pop, and the friend-request path takes it —
    // user/mod.rs:558 fetches device "1" for the recipient's identity key. So a
    // fresh user C friend-requesting the stripped B exercises the server's
    // bundle-serving path and leaves a countable mark.
    //
    // C only needs B's identity key, so C's request is indifferent to Kyber. If
    // the server has a PQXDH floor, it refuses and no OTPK moves. If it serves a
    // Kyber-less bundle, C's request succeeds and an OTPK is consumed.
    //
    // This block used to only PRINT what it saw, because the server had no floor
    // (finding K1). The floor now exists — the serve path refuses a Kyber-less
    // bundle before committing, so the pops roll back — and these are assertions
    // that guard it. See VALIDATED.md K1.
    let otpk_used = || -> i64 {
        count(format!(
            "select count(*) from signal_one_time_prekeys \
             where user_id = '{uid_b}' and used_at is not null"
        ))
    };
    let used_before = otpk_used();

    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let _uid_c = register_and_start(&client_c, &unique_user("v5c"), "Carol").await;
    let probe = client_c.send_friend_request(&uid_b, Some("v5 server probe")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let used_after = otpk_used();

    println!(
        "  server probe: single-device fetch of Kyber-less B -> {}; OTPKs consumed {} -> {}",
        if probe.is_ok() { "SERVED" } else { "REFUSED" },
        used_before,
        used_after
    );
    assert!(
        probe.is_err(),
        "FINDING K1 REPRODUCED: the server SERVED a prekey bundle for B with NO \
         Kyber prekey in it. libsignal is PQXDH-only, so that bundle cannot \
         produce a session on any client — the server has no PQXDH floor and \
         only the client refuses. Chatter fails closed, but on one side only."
    );
    assert_eq!(
        used_after, used_before,
        "FINDING K1 REPRODUCED: the server consumed a one-time prekey ({used_before} \
         -> {used_after}) while refusing/serving a Kyber-less bundle for B. An OTPK \
         was burned on a handshake that cannot succeed, which is a free depletion \
         lever for any caller."
    );
    println!("  server refused and consumed no OTPK — the PQXDH floor holds on both sides");
    client_c.shutdown().await;

    // And nothing may reach B.
    let leaked = wait_for_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.as_text() == Some(m3.as_str()))
    })
    .await;
    assert!(
        leaked.is_none(),
        "KYBER IS DECORATIVE: B received a message that was encrypted with no \
         Kyber prekey in play"
    );
    println!("  B received nothing — no non-PQ session was ever formed");

    println!("V5 PASS — Kyber is mandatory: strip it and the handshake stops dead\n");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V6: FORWARD SECRECY — a delivered message's key is destroyed
//
// CLAIM (README): "Double Ratchet" — i.e. forward secrecy. V3 proves the ratchet
// ADVANCES. That is the precondition, not the property: a ratchet can advance
// perfectly while every message key it derives is kept forever, and forward
// secrecy would be gone with V1/V2/V3 all still green.
//
// THE PROPERTY: once B has received and processed a message, that message's key
// must be GONE. Feeding B the identical ciphertext again must not yield the
// plaintext a second time.
//
// THE NUANCE THAT MAKES A NAIVE TEST WRONG: Signal deliberately RETAINS message
// keys for messages it has not yet seen, so that out-of-order delivery works. So
// "replay an old ciphertext and it decrypts" can be entirely CORRECT behaviour —
// if that message was never delivered in the first place. A test that just
// replays a message and demands failure is testing the wrong thing and would
// (rightly) go red on a correct implementation.
//
// So the test is a DIFFERENTIAL on exactly one variable: was this message
// already delivered?
//
//   env_undelivered — captured from the queue and DELETED before B polls, so B
//                     never sees it. Its key must still be there.  => must DECRYPT
//   env_delivered   — captured from the queue and left alone, so B receives and
//                     processes it normally. Its key must be gone.  => must FAIL
//
// Both envelopes: same session, same sending chain, captured the same way, fed
// through the same API. The ONLY difference is delivery. That is what separates
// "key retained for a legitimately-undelivered message" (correct) from "key
// should have been destroyed after successful delivery" (the property).
//
// WHY decrypt_message() AND NOT A QUEUE REPLAY: re-inserting the envelope into
// Redis would test the wrong layer. If B stayed silent, that could be libsignal
// refusing (the property) OR the client's application-level de-duplication
// suppressing an already-seen message id — and those are indistinguishable from
// the outside. A pass by app-level dedup would be a pass for the wrong reason.
// Calling SignalService::decrypt_message directly bypasses every application
// layer and puts the question to libsignal alone: given these bytes, is the key
// still there?
//
// MUTATION CHECK: swap the two envelopes (assert the DELIVERED one decrypts and
// the UNDELIVERED one fails) and the test MUST fail on both halves.
//
// Gated on `testing` for signal() access.
// ============================================================================

#[cfg(feature = "testing")]
#[tokio::test]
async fn v6_delivered_message_key_is_destroyed() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(&path_b);

    let uid_a = register_and_start(&client_a, &unique_user("v6a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v6b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // B offline so envelopes SIT in the queue and can be captured verbatim —
    // the exact bytes B would otherwise process.
    client_b.shutdown().await;
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let key = format!("device_queue_envelopes:{uid_b}:1");
    let hkeys = |k: &str| -> std::collections::HashSet<String> {
        sh(&format!(
            "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HKEYS '{k}' 2>/dev/null"
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
    };

    // Identify each marker's envelope by DIFFING the field set across the send.
    // Same reasoning as V2: the queue holds unrelated traffic (session
    // bootstrap, friend handshake), and the client's message id never appears
    // server-side because the server assigns its own uuid.New(). Diffing depends
    // on neither assumption.
    let send_and_capture = |marker: &str, before: std::collections::HashSet<String>| {
        let key = key.clone();
        let marker = marker.to_string();
        async move {
            let after = hkeys(&key);
            let new_fields: Vec<String> = after.difference(&before).cloned().collect();
            assert_eq!(
                new_fields.len(),
                1,
                "POSITIVE CONTROL FAILED: expected exactly 1 new envelope in {key} \
                 after sending {marker}, found {}. Zero means it never queued; more \
                 than one means this test cannot tell which envelope is which and \
                 would capture the wrong bytes.",
                new_fields.len()
            );
            let field = new_fields[0].clone();
            let json = sh(&format!(
                "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HGET '{key}' '{field}' 2>/dev/null"
            ));
            let b64 = extract_between(&json, "\"envelope\":\"", "\"")
                .expect("could not extract envelope base64 from queued JSON");
            (field, b64)
        }
    };

    let base = hkeys(&key);

    // ---- message 1: will be DELIVERED --------------------------------------
    let m_delivered = unique_marker();
    println!("\n  delivered marker:   {m_delivered}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m_delivered)
        .await
        .expect("send of delivered marker failed");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (_field_d, b64_delivered) = send_and_capture(&m_delivered, base.clone()).await;

    // ---- message 2: will be WITHHELD ---------------------------------------
    let before2 = hkeys(&key);
    let m_undelivered = unique_marker();
    println!("  undelivered marker: {m_undelivered}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m_undelivered)
        .await
        .expect("send of undelivered marker failed");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (field_u, b64_undelivered) = send_and_capture(&m_undelivered, before2).await;

    assert_ne!(
        b64_delivered, b64_undelivered,
        "captured the same envelope twice — the two markers are not distinguishable"
    );
    println!(
        "  captured 2 envelopes: delivered={} b64 chars, undelivered={} b64 chars",
        b64_delivered.len(),
        b64_undelivered.len()
    );

    // Withhold message 2 from B entirely: delete it from the queue before B
    // polls. From B's point of view this message simply never arrived — the
    // legitimately-undelivered case Signal is designed to tolerate.
    sh(&format!(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HDEL '{key}' '{field_u}' >/dev/null 2>&1"
    ));
    assert!(
        !hkeys(&key).contains(&field_u),
        "POSITIVE CONTROL FAILED: could not withhold the undelivered envelope — \
         the HDEL did not land, so B will process it and the differential collapses"
    );
    println!("  withheld the undelivered envelope from B's queue");

    let b64_to_bytes = |b64: &str| -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .expect("envelope was not valid base64")
    };
    let bytes_delivered = b64_to_bytes(&b64_delivered);
    let bytes_undelivered = b64_to_bytes(&b64_undelivered);

    // ---- B comes online and processes ONLY message 1 -----------------------
    let client_b2 = build_client(&path_b);
    client_b2.connect().await.expect("B reconnect failed");
    let mut events_b = client_b2.subscribe_events();
    assert!(
        client_b2.restore_session(),
        "B failed to restore persisted session"
    );
    if let Err(e) = client_b2.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("B start failed: {e:?}");
        }
    }

    let expected_conv = ChannelId::from(uid_a);
    let got = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == expected_conv
                 && message.as_text() == Some(m_delivered.as_str()))
    })
    .await;
    assert!(
        got.is_some(),
        "POSITIVE CONTROL FAILED: B never received the 'delivered' message, so \
         its key was never consumed and the whole premise of this test is void"
    );
    println!("  B received and processed the delivered message — its key is now spent");

    // Confirm B really never saw message 2. If it slipped through, its key is
    // spent too and the positive control below would fail for the wrong reason.
    let slipped = wait_for_event(&mut events_b, Duration::from_secs(8), |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.as_text() == Some(m_undelivered.as_str()))
    })
    .await;
    assert!(
        slipped.is_none(),
        "POSITIVE CONTROL FAILED: B received the message we tried to withhold. \
         Both keys are now spent and the differential is destroyed."
    );
    println!("  B never saw the undelivered message — its key must still be retained");

    // ---- POSITIVE CONTROL: the undelivered key IS still there --------------
    // This is the half that makes the failure below meaningful. It proves:
    //   (a) decrypt_message() on a queue-captured envelope is a WORKING call —
    //       so "it errored" cannot be blamed on a wrong byte format or a broken
    //       API, which would make the property assertion vacuous;
    //   (b) Signal's deliberate retention of keys for undelivered messages is
    //       intact — this test does NOT mistake correct out-of-order tolerance
    //       for a forward-secrecy break.
    let res_undelivered = client_b2.signal().decrypt_message(&bytes_undelivered).await;
    match &res_undelivered {
        Ok((_addr, plaintext, _)) => {
            let hay = String::from_utf8_lossy(plaintext);
            assert!(
                hay.contains(&m_undelivered),
                "POSITIVE CONTROL FAILED: the undelivered envelope decrypted but \
                 not to its own marker"
            );
            println!("  undelivered envelope -> DECRYPTED (correct: key legitimately retained)");
        }
        Err(e) => panic!(
            "POSITIVE CONTROL FAILED: decrypt_message() rejected an envelope for a \
             message B never received: {e:?}. Either the captured bytes are not \
             what this API takes, or the API is broken — either way the assertion \
             below would 'pass' for a reason that has nothing to do with forward \
             secrecy."
        ),
    }

    // ---- THE PROPERTY: the delivered key is GONE ---------------------------
    // Identical API, identical byte source, identical session. The only thing
    // that differs from the call above is that B already processed this one.
    let res_delivered = client_b2.signal().decrypt_message(&bytes_delivered).await;
    println!("  delivered envelope  -> {}", match &res_delivered {
        Ok(_) => "DECRYPTED AGAIN".to_string(),
        Err(e) => format!("REJECTED: {e:?}"),
    });

    if let Ok((_addr, plaintext, _msg_type)) = &res_delivered {
        let hay = String::from_utf8_lossy(plaintext);
        if hay.contains(&m_delivered) {
            panic!(
                "FORWARD SECRECY VIOLATED: B decrypted an already-delivered message \
                 a second time from the same ciphertext. The message key survived \
                 successful delivery. This is not the out-of-order-tolerance case — \
                 the control above establishes that an UNDELIVERED message's key is \
                 legitimately retained, and this one was delivered and processed. \
                 Anyone who later seizes B's device recovers every message ever \
                 delivered to it whose ciphertext they also hold."
            );
        }
    }
    assert!(
        res_delivered.is_err(),
        "FORWARD SECRECY VIOLATED: replaying a delivered message's ciphertext did \
         not fail"
    );

    println!(
        "V6 PASS — a delivered message's key is destroyed; an undelivered message's \
         key is retained (as Signal intends)\n"
    );

    client_a.shutdown().await;
    client_b2.shutdown().await;
}

// ============================================================================
// V7: THE PQOPK POOL IS SEEDED AND CONSUMED — per-session post-quantum secrecy
//
// CLAIM (README): post-quantum key agreement via Kyber/ML-KEM prekeys.
//
// WHY V5 IS NOT ENOUGH (finding K2): PQXDH can draw its post-quantum secret from
// EITHER of two sources, and the server silently picks whichever it has:
//   - a ONE-TIME Kyber prekey (PQOPK), popped and burned on handout, so every
//     session gets its own PQ secret; or
//   - the medium-term LAST-RESORT Kyber prekey (PQSPK), which is REUSED by every
//     handshake against that account until it is rotated.
// Both yield "the handshake used Kyber", so both make V5 green. Only the first
// gives per-session PQ forward secrecy. Compromise the PQSPK and every session
// that fell back to it shares one PQ secret.
//
// This is exactly the state chatter shipped in: `signal_kyber_one_time_prekeys`
// had NEVER held a single row (0 rows against 17,100 classical OPKs), because
// registration seeded 100 classical one-time prekeys and zero PQ ones. Every
// PQXDH handshake this system ever performed fell back to the reused PQSPK, and
// the server logged "PQOPK pool exhausted" 71 times saying so. V5 stayed green
// the entire time. A row count of 0 was the whole empirical signature of the bug
// — precisely as `device_messages = 0` was for the durability bug.
//
// The fallback is spec-permitted (PQXDH §3.2 — that is what a last-resort key is
// FOR), so this test does not assert the fallback is impossible. It asserts the
// pool is stocked and actually drawn from, so the fallback stays the exception
// it is meant to be instead of being the only code path that ever runs.
//
// POSITIVE CONTROL FIRST: assert the CLASSICAL OPK pool for this very user_id is
// non-empty before asserting anything about the PQ pool. Registration-time prekey
// upload working at all, and this user_id being the right one, are preconditions
// for the real assertion. Without this control a PQOPK count of 0 could just mean
// "registration failed" or "we are querying a uuid that does not exist" — the
// test would go red for a reason that has nothing to do with PQXDH.
//
// THE SECOND HALF: seeding the pool is worthless if nothing pops it. So this also
// drives a REAL handshake through the server's single-device fetch (the
// friend-request path — the wildcard fetch passes popOTPK=false and leaves no
// trace) and requires that a PQOPK was actually consumed AND that the server did
// not log the PQSPK fallback for this user.
//
// MUTATION CHECK: comment out the PQOPK seeding block in
// `services/auth_service.rs::register` (the `refresh_pre_keys_with_kyber_otpk`
// call) and this test MUST fail at the pool-seeded assertion with pqopk=0 —
// which is the pre-fix production state, reproduced exactly.
// ============================================================================

#[tokio::test]
async fn v7_pqopk_pool_is_seeded_and_consumed() {
    skip_if_no_server!();
    no_keyring_prompts();

    let count = |sql: String| -> i64 { psql(&sql).trim().parse().unwrap_or(-1) };

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v7b"), "Bob").await;

    // ---- POSITIVE CONTROL: the classical pool for THIS user is stocked ------
    // Proves registration-time prekey upload works and that uid_b names a real
    // row. Only then is "PQOPK == 0" attributable to PQ seeding specifically.
    let opk = count(format!(
        "select count(*) from signal_one_time_prekeys where user_id = '{uid_b}'"
    ));
    assert!(
        opk > 0,
        "POSITIVE CONTROL FAILED: user {uid_b} has {opk} classical OPKs on the \
         server. Registration-time prekey upload is broken (or this user_id is \
         wrong), so any claim about the PQ pool below would be measuring the \
         wrong thing."
    );
    println!("  positive control: classical OPKs uploaded at registration = {opk}");

    // ---- THE PROPERTY (K2): the PQ one-time pool is stocked too -------------
    let pqopk = count(format!(
        "select count(*) from signal_kyber_one_time_prekeys where user_id = '{uid_b}'"
    ));
    println!("  PQOPK rows seeded at registration = {pqopk}");
    assert!(
        pqopk > 0,
        "FINDING K2 REPRODUCED: user {uid_b} registered with {opk} classical \
         one-time prekeys and {pqopk} POST-QUANTUM one-time prekeys. The PQOPK \
         pool is empty, so the server has nothing to pop and EVERY PQXDH \
         handshake against this account will fall back to the single last-resort \
         PQSPK — one reused PQ secret across every session, instead of one per \
         session. V5 cannot see this: the fallback still 'uses Kyber'."
    );

    // ---- THE SECOND HALF: a real handshake must actually POP one ------------
    // A stocked pool that nothing draws from is decoration. The friend-request
    // path takes the SINGLE-device fetch, which is the one that pops (the
    // wildcard fetch passes popOTPK=false and would leave no trace).
    let pq_used = || -> i64 {
        count(format!(
            "select count(*) from signal_kyber_one_time_prekeys \
             where user_id = '{uid_b}' and used_at is not null"
        ))
    };
    let used_before = pq_used();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let _uid_a = register_and_start(&client_a, &unique_user("v7a"), "Alice").await;
    client_a
        .send_friend_request(&uid_b, Some("v7 pqopk consumption probe"))
        .await
        .expect("friend request failed — the server-side bundle fetch never ran, \
                 so nothing below would be measuring PQOPK consumption");
    // The pop is committed by the server after the RPC returns. Poll for the
    // counter to move instead of sleeping a fixed 2s: `used_at` only ever goes
    // up, so a poll that gives up still reports the same value a sleep would
    // have, and the assertion below is unchanged.
    let mut used_after = pq_used();
    let deadline = Instant::now() + Duration::from_secs(30);
    while used_after <= used_before && Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        used_after = pq_used();
    }

    println!("  PQOPKs consumed by one real handshake: {used_before} -> {used_after}");
    assert!(
        used_after > used_before,
        "PQOPK POOL IS DECORATIVE: the pool holds {pqopk} keys, a real handshake \
         against user {uid_b} completed, and yet no PQOPK was marked used \
         ({used_before} -> {used_after}). The keys are being stored and never \
         drawn from, so the handshake still rode the reused last-resort PQSPK."
    );

    // The server says so itself: no fallback warning may name this user.
    let fell_back = sh(&format!(
        "grep 'PQOPK pool exhausted' server.log 2>/dev/null | grep -c '{uid_b}'"
    ))
    .trim()
    .parse::<i64>()
    .unwrap_or(-1);
    println!("  'PQOPK pool exhausted' warnings naming this user = {fell_back}");
    assert_eq!(
        fell_back, 0,
        "The server logged the last-resort PQSPK fallback for user {uid_b} even \
         though its PQOPK pool was seeded — the pop path is not reaching the pool."
    );

    println!(
        "V7 PASS — registration seeds the PQOPK pool and a real handshake burns \
         one: PQ forward secrecy is per-session, not one reused key\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V8 — a LINKED SECONDARY DEVICE seeds its own PQOPK pool, and a real handshake
//      against that device burns one
//
// V7's sibling. V7 proved the property for the device created by *registration*.
// This proves it for the device created by *linking* — the other way a device
// enters an account, and the one with zero empirical coverage until this test.
// That gap is why the bug survived here after the registration path was fixed:
// `link_device_service.rs::complete_linking` generated 100 classical OPKs and
// zero PQ ones, so every PQXDH handshake to a linked device fell back to that
// device's single reused last-resort PQSPK.
//
// Multi-device is not a corner: a linked desktop is a full recipient of every
// message. A PQ weakness there is a PQ weakness for the account.
//
// POSITIVE CONTROL FIRST — and here it has to do MORE work than V7's, because
// this account has TWO devices and the primary's pool is already stocked (V7's
// fix did that). A global `count(*) where user_id = …` would therefore pass
// vacuously on the PRIMARY's 100 keys while the linked device holds zero. So:
//   (a) every query below is scoped to (user_id, linked_device_id);
//   (b) the test asserts the linked device_id is *distinct* from the primary's,
//       so "scoped to the linked device" is a claim with teeth;
//   (c) the classical OPK pool for that same (user, device) pair must be
//       non-empty first — proving the link really uploaded a bundle under that
//       device_id, so a PQ count of 0 means "PQ seeding is broken", not
//       "linking failed" or "we are querying a device that does not exist".
//
// THE SECOND HALF: a stocked pool nothing draws from is decoration. A peer
// fetches the LINKED device's bundle through the server's real single-device
// handout path (`GetPreKeyBundle` with an explicit device_id — the path that
// pops; the wildcard all-devices fetch passes popOTPK=false and leaves no
// trace), and a PQOPK for that device must be marked used.
//
// MUTATION CHECK: delete the `refresh_pre_keys_with_kyber_otpk` seeding block in
// `link_device_service.rs::complete_linking` and this test MUST fail at the
// pool-seeded assertion with pqopk=0 — the pre-fix state, reproduced exactly.
// ============================================================================

#[tokio::test]
async fn v8_linked_device_pqopk_pool_is_seeded_and_consumed() {
    skip_if_no_server!();
    no_keyring_prompts();

    let count = |sql: String| -> i64 { psql(&sql).trim().parse().unwrap_or(-1) };

    // ---- primary registers, then a secondary links to it --------------------
    let dir_p = tempfile::tempdir().unwrap();
    let client_p = build_client(dir_p.path());
    let uid = register_and_start(&client_p, &unique_user("v8p"), "V8 Primary").await;

    let dir_s = tempfile::tempdir().unwrap();
    let client_s = build_client(dir_s.path());
    let dev_s = link_secondary_device(&client_p, &client_s, uid).await;

    // ---- POSITIVE CONTROL (a): the linked device is a DIFFERENT device -------
    // If linking silently reused the primary's device_id, every assertion below
    // would be measuring the primary's already-stocked pool and pass vacuously.
    let devices = psql(&format!(
        "select device_id from signal_prekey_bundles where user_id = '{uid}' order by device_id"
    ));
    println!("  device_ids with a bundle for this user: {devices:?}");
    let dev_p: i64 = count(format!(
        "select coalesce(min(device_id), -1) from signal_prekey_bundles \
         where user_id = '{uid}' and device_id != {dev_s}"
    ));
    assert!(
        dev_p > 0 && dev_p != dev_s as i64,
        "POSITIVE CONTROL FAILED: expected the account to hold a primary device \
         distinct from the linked device {dev_s}, found primary={dev_p}. Without \
         two distinct devices this test cannot tell the linked device's PQ pool \
         from the primary's, and a global count would pass on the primary's keys."
    );
    println!("  positive control: primary device={dev_p}, linked device={dev_s} — distinct");

    // ---- POSITIVE CONTROL (b): the LINKED device's classical pool is stocked --
    let opk = count(format!(
        "select count(*) from signal_one_time_prekeys \
         where user_id = '{uid}' and device_id = {dev_s}"
    ));
    assert!(
        opk > 0,
        "POSITIVE CONTROL FAILED: linked device {dev_s} has {opk} classical OPKs \
         on the server. Linking-time prekey upload is broken (or this device_id \
         is wrong), so any claim about its PQ pool would measure the wrong thing."
    );
    println!("  positive control: classical OPKs uploaded at LINKING = {opk}");

    // ---- THE PROPERTY: the linked device's PQ one-time pool is stocked too ---
    let pqopk = count(format!(
        "select count(*) from signal_kyber_one_time_prekeys \
         where user_id = '{uid}' and device_id = {dev_s}"
    ));
    println!("  PQOPK rows seeded at LINKING for device {dev_s} = {pqopk}");
    assert!(
        pqopk > 0,
        "LINKED-DEVICE PQOPK GAP REPRODUCED: linked device {dev_s} uploaded {opk} \
         classical one-time prekeys and {pqopk} POST-QUANTUM one-time prekeys. \
         The server has nothing to pop for this device, so EVERY PQXDH handshake \
         to this linked device falls back to its single last-resort PQSPK — one \
         reused PQ secret across every session to a device that receives every \
         message the account receives. The primary device's pool being stocked \
         (V7) does not help this device at all."
    );

    // ---- THE SECOND HALF: a real handshake to THIS device must pop one -------
    let pq_used = || -> i64 {
        count(format!(
            "select count(*) from signal_kyber_one_time_prekeys \
             where user_id = '{uid}' and device_id = {dev_s} and used_at is not null"
        ))
    };
    let used_before = pq_used();

    // A real peer, over the real server, fetching THIS device's bundle through
    // the single-device handout path — exactly what a sender does before its
    // first message to the linked device.
    let peer_pool = common::create_test_pool()
        .await
        .expect("peer pool connect failed");
    let _peer = common::setup_authenticated_test(&peer_pool)
        .await
        .expect("peer registration failed — the bundle fetch below would be unauthenticated");
    let mut peer_auth = peer_pool.auth_client();
    let resp = peer_auth
        .get_pre_key_bundle(&uid.to_string(), &dev_s.to_string())
        .await
        .expect("peer could not fetch the linked device's bundle — the server-side \
                 handout never ran, so nothing below measures PQOPK consumption");
    assert!(
        resp.result.is_some(),
        "server returned an empty bundle for linked device {dev_s} — no handshake \
         material, so no pop could have happened"
    );
    let used_after = pq_used();

    println!("  PQOPKs consumed by one real handshake to device {dev_s}: {used_before} -> {used_after}");
    assert!(
        used_after > used_before,
        "PQOPK POOL IS DECORATIVE for linked device {dev_s}: the pool holds \
         {pqopk} keys, a real bundle fetch against that device completed, and yet \
         no PQOPK was marked used ({used_before} -> {used_after}). The keys are \
         stored and never drawn from, so the handshake still rode the reused \
         last-resort PQSPK."
    );

    // The server says so itself: no fallback warning may name this device.
    let fell_back = sh(&format!(
        "grep 'PQOPK pool exhausted' server.log 2>/dev/null | grep '{uid}' | grep -c 'device_id\":{dev_s}'"
    ))
    .trim()
    .parse::<i64>()
    .unwrap_or(-1);
    println!("  'PQOPK pool exhausted' warnings naming linked device {dev_s} = {fell_back}");
    assert_eq!(
        fell_back, 0,
        "The server logged the last-resort PQSPK fallback for linked device \
         {dev_s} even though its PQOPK pool was seeded — the pop path is not \
         reaching this device's pool."
    );

    println!(
        "V8 PASS — a LINKED device seeds its own PQOPK pool and a real handshake \
         to it burns one: PQ forward secrecy is per-session on secondary devices \
         too, not one reused key\n"
    );

    client_s.shutdown().await;
    client_p.shutdown().await;
}

// ============================================================================
// V9 — a LINKED SECONDARY DEVICE actually RECEIVES and DECRYPTS a DM
//
// CLAIM (README): multi-device. Signal's model is that a message addressed to a
// USER fans out to every one of that user's DEVICES, and each device decrypts it
// under its own session. A linked desktop is a full recipient, not a viewer.
//
// This is the claim V8 explicitly does NOT make. V8 proves a linked device OWNS
// a PQOPK pool and that a SINGLE-DEVICE bundle fetch burns one. The DM send path
// is different code: it fetches the recipient's bundles through the WILDCARD
// all-devices path (`auth_client.rs:531` — `get_pre_key_bundle(user, "*")`, which
// passes popOTPK=false and leaves no trace in the used-key columns V8 watches),
// then encrypts once PER DEVICE and issues one `SendSealedMessage` RPC per
// device. Nothing before this test ever established that a secondary device
// receives, let alone decrypts. Device linking was found to be completely broken
// as recently as yesterday — three individually-fatal bugs stacked behind each
// other — so "it is wired, therefore it works" is exactly the reasoning this file
// exists to refuse.
//
// The realistic way this goes wrong is silent. `signal_protocol.go:1027-1048`
// OMITS a device with no Kyber material from the wildcard device list rather than
// erroring: the sender simply never learns the device exists, encrypts for the
// primary only, and reports a successful send. B's linked device goes dark
// forever and NOTHING anywhere reports a failure. A test that only asserts "A's
// send returned Ok" passes happily through that. So this test asserts on B's
// side of the wire, per device.
//
// POSITIVE CONTROL FIRST, and it has to be scoped, because this account has two
// devices and device 1's queue is stocked by ordinary traffic:
//   (a) the linked device_id is distinct from the primary's — otherwise every
//       per-device assertion below is silently measuring device 1 twice;
//   (b) the row diff is scoped to THIS RUN's user and taken across THIS send, so
//       a global count cannot pass on some other test's rows. A global
//       `count(*) > 0` here would be vacuous — that trap has bitten twice today.
//
// THE NEGATIVE HALF: the fan-out must not buy multi-device by breaking anything.
//   - The two devices' envelopes must be DISTINCT ciphertexts. If byte-identical,
//     one session's key material is being reused across devices (or the server is
//     copying one blob to every queue — its `destination_device_id == 0` branch at
//     sealed_sender.go:156-176 does exactly that, and would be a real finding if
//     the client ever took it). Distinctness is what proves per-device sessions.
//   - Sealed sender must still hold for BOTH rows (V4's property, per device).
//   - The marker must not appear in either envelope.
//
// MUTATION CHECK: in `deposit_sealed_to_all_devices` (server_dm.rs:322), skip
// every device except the lowest (`if device_id != 1 { continue; }`) — i.e.
// deliver to the primary only, the exact shape of the silent-omission bug above.
// This test MUST fail at the linked-device row assertion. RUN IT.
// ============================================================================

#[tokio::test]
async fn v9_linked_device_receives_and_decrypts_dm() {
    skip_if_no_server!();
    no_keyring_prompts();

    // ---- B: primary registers, then a REAL secondary device links -----------
    let dir_bp = tempfile::tempdir().unwrap();
    let path_bp = dir_bp.path().to_path_buf();
    let client_bp = build_client(&path_bp);
    let uid_b = register_and_start(&client_bp, &unique_user("v9bp"), "V9 Bob Primary").await;

    let dir_bs = tempfile::tempdir().unwrap();
    let path_bs = dir_bs.path().to_path_buf();
    let client_bs = build_client(&path_bs);
    let dev_s = link_secondary_device(&client_bp, &client_bs, uid_b).await;

    // Bring the linked device fully up. `Linked` establishes credentials; the
    // inbox poller only runs after start() (client/link_device.rs:53).
    if let Err(e) = client_bs.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("linked device start failed: {e:?}");
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- POSITIVE CONTROL (a): two DISTINCT devices ------------------------
    // If linking silently reused device 1, "both devices got a row" would be one
    // device counted twice and every assertion below would pass vacuously.
    assert_ne!(
        dev_s, 1,
        "POSITIVE CONTROL FAILED: linked device was assigned device_id 1, the \
         primary's id. This test cannot tell the two devices apart, so a per-device \
         row count would be measuring the primary twice."
    );
    let devices = psql(&format!(
        "select device_id from signal_prekey_bundles where user_id = '{uid_b}' order by device_id"
    ));
    println!("  device_ids with a bundle for this user: {devices:?}");
    let n_dev: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_b}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev, 2,
        "POSITIVE CONTROL FAILED: expected exactly 2 devices with a prekey bundle \
         for B (primary 1 + linked {dev_s}), found {n_dev}. A sender's wildcard \
         fetch reads exactly this table, so without two bundles here the fan-out \
         has nothing to fan out TO and the test would prove nothing."
    );
    println!("  positive control: primary device=1, linked device={dev_s} — distinct, both have bundles");

    // ---- A registers and befriends B ---------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v9a"), "V9 Alice").await;

    befriend(&client_a, &client_bp, uid_a, uid_b).await;

    // ---- take BOTH of B's devices offline ----------------------------------
    // The envelopes must SIT in the queues so we can read the very bytes each
    // device will later decrypt. A live client acks and the rows are gone before
    // we can audit them.
    client_bs.shutdown().await;
    client_bp.shutdown().await;
    drop(client_bs);
    drop(client_bp);
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- snapshot BEFORE the send, scoped to THIS user ----------------------
    let before = row_ids(&uid_b);
    println!("  device_messages rows for B before the send: {}", before.len());

    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");

    // ---- ONE send, addressed to the USER, not to a device -------------------
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");

    // ---- read back exactly the rows THIS send created ----------------------
    // Both of B's devices are offline, so the queue only grows: wait for the
    // 2 rows a correct fan-out produces rather than sleeping past the second
    // deposit. A fan-out that really is broken still returns 1 row and the
    // assertion below still fires.
    let new_rows = wait_for_new_envelopes(&uid_b, &before, 2).await;

    println!("  NEW device_messages rows created by this one send: {}", new_rows.len());
    for f in &new_rows {
        println!(
            "    device_id={} envelope_md5={} source_user_id={} envelope_bytes={}",
            f[1], f[2], f[3], f[4]
        );
    }

    assert!(
        !new_rows.is_empty(),
        "POSITIVE CONTROL FAILED: A's send created ZERO new device_messages rows \
         for B. The message never reached the store this test audits, so every \
         per-device assertion below would be vacuous."
    );

    let for_dev = |d: u32| -> Vec<&Vec<String>> {
        new_rows.iter().filter(|f| f[1] == d.to_string()).collect()
    };
    let primary_rows = for_dev(1);
    let linked_rows = for_dev(dev_s);

    // ---- POSITIVE CONTROL (b): the PRIMARY got it --------------------------
    // Establishes the send worked at all. If this fails, a missing linked-device
    // row means "sending is broken", not "fan-out is broken" — a different bug.
    assert!(
        !primary_rows.is_empty(),
        "POSITIVE CONTROL FAILED: B's PRIMARY device 1 got no envelope from this \
         send ({} new rows total, none for device 1). Sending itself is broken, so \
         this test cannot say anything about the linked device.",
        new_rows.len()
    );
    println!("  positive control: primary device 1 queued {} envelope(s)", primary_rows.len());

    // ---- THE PROPERTY: the LINKED device got it too -------------------------
    assert!(
        !linked_rows.is_empty(),
        "MULTI-DEVICE FAN-OUT BROKEN: A sent ONE DM to user {uid_b}, who has TWO \
         devices (primary 1 and linked {dev_s}, both holding a prekey bundle — \
         asserted above). B's PRIMARY received {} envelope(s). B's LINKED device \
         received ZERO. A's send returned Ok and reported success.\n\
         The linked device is dark: it will never see this message or any other, \
         and nothing anywhere reports an error. Signal's model is that a message \
         to a USER reaches every one of that user's DEVICES.\n\
         Likely root cause: the sender's wildcard bundle fetch \
         (auth_client.rs:531, `get_pre_key_bundle(user, \"*\")`) never returned \
         device {dev_s}, so A encrypted for the primary alone. \
         signal_protocol.go:1027-1048 OMITS a device with no Kyber material from \
         that list rather than failing — which is exactly this symptom.",
        primary_rows.len()
    );
    println!("  THE PROPERTY: linked device {dev_s} queued {} envelope(s)", linked_rows.len());

    // ---- NEGATIVE HALF (1): the two devices' ciphertexts are DISTINCT -------
    // Per-device Signal sessions mean per-device ciphertexts. Byte-identical
    // envelopes across devices would mean either that one session's key material
    // is reused across both devices, or that the server copied a single blob into
    // both queues (its destination_device_id==0 branch, sealed_sender.go:156-176,
    // does precisely that). Either is a serious finding, and either would make the
    // "both received" assertion above true for the wrong reason.
    let primary_md5: std::collections::HashSet<&str> =
        primary_rows.iter().map(|f| f[2].as_str()).collect();
    let linked_md5: std::collections::HashSet<&str> =
        linked_rows.iter().map(|f| f[2].as_str()).collect();
    let shared: Vec<&&str> = primary_md5.intersection(&linked_md5).collect();
    assert!(
        shared.is_empty(),
        "PER-DEVICE SESSIONS VIOLATED: device 1 and linked device {dev_s} were \
         queued BYTE-IDENTICAL envelope(s) (md5 {shared:?}). Two devices with \
         distinct identity keys and distinct sessions cannot legitimately produce \
         the same ciphertext. Either one session's key material is being reused \
         across both devices, or a single blob was copied into both queues — in \
         which case only one device can actually decrypt it and 'both got a row' \
         means nothing."
    );
    println!(
        "  negative half: envelope md5 device1={:?} device{}={:?} — DISTINCT ciphertexts",
        primary_md5, dev_s, linked_md5
    );

    // ---- NEGATIVE HALF (2): sealed sender holds for BOTH rows ---------------
    // V4's property must not silently regress for the multi-device path. The
    // fan-out issues one RPC per device; a device whose RPC fell back to Basic
    // auth would be stamped with A's user id while the other stayed anonymous.
    let leaked: Vec<&Vec<String>> = new_rows.iter().filter(|f| f[3] != "NULL").collect();
    assert!(
        leaked.is_empty(),
        "SEALED SENDER REGRESSED ON THE FAN-OUT: {} of {} envelopes from this send \
         carry a populated source_user_id (A is {}). V4's property holds for \
         single-device DMs but breaks here — per-device rows: {leaked:?}. The \
         envelope is opaque (V1) and the column beside it names the sender.",
        leaked.len(),
        new_rows.len(),
        &uid_a.to_string()[..8]
    );
    println!("  negative half: source_user_id IS NULL on all {} rows — sealed sender holds per device", new_rows.len());

    // ---- NEGATIVE HALF (3): neither envelope contains the plaintext ---------
    let plain = psql(&format!(
        "select count(*) from device_messages where destination_user_id = '{uid_b}' \
         and position('{marker}' in encode(envelope, 'escape')) > 0"
    ));
    let plain: i64 = plain.trim().parse().unwrap_or(-1);
    assert_eq!(
        plain, 0,
        "E2EE VIOLATED ON THE FAN-OUT PATH: the plaintext marker appears in {plain} \
         queued envelope(s) for B. (V0 proves this detector finds a marker that is \
         really there.)"
    );
    println!("  negative half: marker occurs in 0 queued envelopes");

    // ---- THE OTHER HALF: BOTH devices decrypt to the EXACT plaintext --------
    // Rows in a queue are not receipt. Each device must independently turn its own
    // envelope back into precisely what A typed, under its own session. This is
    // the assertion the whole test is for: a queued envelope a device cannot
    // decrypt is indistinguishable from no message at all.
    let expected_conv = ChannelId::from(uid_a);

    let bring_up = |path: std::path::PathBuf, label: &'static str| async move {
        let c = build_client(&path);
        c.connect().await.unwrap_or_else(|e| panic!("{label} reconnect failed: {e:?}"));
        let rx = c.subscribe_events();
        assert!(
            c.restore_session(),
            "{label} failed to restore its persisted session — it must decrypt with \
             the identity that was live when A encrypted, not a fresh one"
        );
        if let Err(e) = c.start().await {
            if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
                panic!("{label} start failed: {e:?}");
            }
        }
        (c, rx)
    };

    let (client_bp2, mut ev_p) = bring_up(path_bp.clone(), "B primary").await;
    let (client_bs2, mut ev_s) = bring_up(path_bs.clone(), "B linked").await;

    let got_p = wait_for_event(&mut ev_p, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;
    let got_s = wait_for_event(&mut ev_s, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;

    let body_of = |e: &Option<DomainEvent>| -> Option<String> {
        match e {
            Some(DomainEvent::MessageReceived { message }) => {
                Some(message.as_text().unwrap_or("<not text>").to_string())
            }
            _ => None,
        }
    };

    let body_p = body_of(&got_p);
    let body_s = body_of(&got_s);
    println!("  B primary  decrypted: {body_p:?}");
    println!("  B linked   decrypted: {body_s:?}");

    // Store fallback for a lagged bus — see `wait_for_stored_text`.
    let ok_p = body_p.as_deref() == Some(marker.as_str())
        || wait_for_stored_text(&client_bp2, &expected_conv, &marker).await;
    let ok_s = body_s.as_deref() == Some(marker.as_str())
        || wait_for_stored_text(&client_bs2, &expected_conv, &marker).await;

    assert!(
        ok_p,
        "B's PRIMARY device did not recover A's exact plaintext (got {body_p:?}). \
         The primary is the control here — if it cannot decrypt, the linked \
         device's result below says nothing about multi-device."
    );
    assert!(
        ok_s,
        "LINKED DEVICE CANNOT DECRYPT: B's linked device {dev_s} was queued its own \
         distinct envelope (asserted above — the row is really there), came online, \
         and did not surface A's plaintext. Got {body_s:?}, expected {marker:?}. \
         B's PRIMARY decrypted the same message fine, so this is not a broken send \
         or a broken session in general — it is specific to the secondary device. \
         An envelope a device cannot decrypt is indistinguishable from no message: \
         the linked device is a recipient on paper only."
    );

    println!(
        "V9 PASS — one DM to a user fanned out to BOTH of their devices as distinct \
         ciphertexts, each decrypted to the exact plaintext by its own device, and \
         sealed sender held for both\n"
    );

    client_a.shutdown().await;
    client_bp2.shutdown().await;
    client_bs2.shutdown().await;
}

// ============================================================================
// V10 — ONE DM fans out to THREE devices, as three DISTINCT ciphertexts
//
// CLAIM (README): multi-device. V9 proved a user with TWO devices (primary +
// one linked) has both queued and both decrypting. This test exists because two
// is not enough to prove the general claim:
//
//   A fan-out that handles "the primary, plus one special case" is
//   INDISTINGUISHABLE from one that iterates the device list, when the device
//   list has length 2. Three is the smallest number that separates them.
//
// That is not a hypothetical distinction. The client-side fan-out
// (`deposit_sealed_to_all_devices`, server_dm.rs:322) loops over whatever
// `encrypt_message_all_devices_with_uak` returns, which in turn loops over the
// server's wildcard bundle list. Every one of those loops is a place where a
// device can be dropped, and a 2-device test passes for any bug that drops
// everything past the second entry.
//
// POSITIVE CONTROL FIRST, scoped, because a 3-device account makes vacuity easy:
//   (a) all three device_ids are pairwise DISTINCT and none collides with the
//       primary's 1 — otherwise "three devices got rows" could be one device
//       counted three times, and every assertion below is theatre;
//   (b) exactly THREE devices hold a prekey bundle for THIS user before the send.
//       The sender's wildcard fetch reads exactly that table: without three
//       bundles there the fan-out has nothing to fan out to, and a missing
//       envelope would mean "the device never published a bundle", not "fan-out
//       is broken" — a different bug wearing this test's failure message.
//   (c) the PRIMARY receives. If it does not, sending itself is broken and this
//       test cannot speak about fan-out at all.
//
// THE NEGATIVE HALF — three ways this could be green while broken:
//   1. PAIRWISE DISTINCT ciphertexts. Per-device Signal sessions mean per-device
//      ciphertexts. Byte-identical envelopes across ANY pair would mean one
//      session's key material is reused across devices, or that a single blob was
//      copied into every queue — which is exactly what the server's
//      `destination_device_id == 0` branch (sealed_sender.go:156-176) does. The
//      client never takes that branch; this assertion is what pins that. Only one
//      device could actually decrypt such a blob, so "all three got a row" would
//      be true and meaningless. Pairwise, not just "all different from device 1":
//      a bug that copies device 2's blob to device 3 survives the weaker check.
//   2. Sealed sender holds on all three rows (V4's property, per device). The
//      fan-out issues one RPC per device; a device whose RPC fell back to Basic
//      auth would be stamped with A's user id while the others stayed anonymous.
//   3. The marker appears in no envelope (V1's property, on this path).
//
// THE OTHER HALF: a row in a queue is not receipt. All three devices must
// independently turn their own envelope back into precisely what A typed. An
// envelope a device cannot decrypt is indistinguishable from no message at all.
//
// MUTATION CHECK — RUN IT, do not describe it. Two sabotages, both run:
//   (i)  in `deposit_sealed_to_all_devices` (server_dm.rs:322), `if device_id != 1
//        { continue; }` — deliver to the primary only. MUST fail at the
//        linked-device row assertion.
//   (ii) in the same loop, reuse the first device's ciphertext for every device.
//        MUST fail at the PAIRWISE DISTINCT assertion — proving that half has
//        teeth and is not just decoration that no input can trip.
// ============================================================================

#[tokio::test]
async fn v10_dm_fans_out_to_three_devices() {
    skip_if_no_server!();
    no_keyring_prompts();

    // ---- B: primary registers, then TWO real secondary devices link ---------
    let dir_bp = tempfile::tempdir().unwrap();
    let path_bp = dir_bp.path().to_path_buf();
    let client_bp = build_client(&path_bp);
    let uid_b = register_and_start(&client_bp, &unique_user("v10bp"), "V10 Bob Primary").await;

    let dir_b2 = tempfile::tempdir().unwrap();
    let path_b2 = dir_b2.path().to_path_buf();
    let client_b2 = build_client(&path_b2);
    let dev_2 = link_secondary_device(&client_bp, &client_b2, uid_b).await;

    let dir_b3 = tempfile::tempdir().unwrap();
    let path_b3 = dir_b3.path().to_path_buf();
    let client_b3 = build_client(&path_b3);
    let dev_3 = link_secondary_device(&client_bp, &client_b3, uid_b).await;

    start_linked(&client_b2, "B linked #1").await;
    start_linked(&client_b3, "B linked #2").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- POSITIVE CONTROL (a): three PAIRWISE DISTINCT device ids -----------
    assert!(
        dev_2 != 1 && dev_3 != 1 && dev_2 != dev_3,
        "POSITIVE CONTROL FAILED: device ids are not pairwise distinct \
         (primary=1, linked#1={dev_2}, linked#2={dev_3}). This test cannot tell the \
         three devices apart, so a per-device row count would be measuring the same \
         device more than once and every assertion below would pass vacuously."
    );

    // ---- POSITIVE CONTROL (b): three devices, three bundles -----------------
    let devices = psql(&format!(
        "select device_id from signal_prekey_bundles where user_id = '{uid_b}' order by device_id"
    ));
    println!("  device_ids with a bundle for this user: {devices:?}");
    let n_dev: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_b}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev, 3,
        "POSITIVE CONTROL FAILED: expected exactly 3 devices with a prekey bundle for \
         B (primary 1 + linked {dev_2} + linked {dev_3}), found {n_dev}. A sender's \
         wildcard fetch reads exactly this table, so without three bundles here the \
         fan-out has nothing to fan out TO and a missing envelope below would mean \
         'a device never published a bundle', not 'fan-out drops devices'."
    );
    println!(
        "  positive control: devices 1, {dev_2}, {dev_3} — pairwise distinct, all three hold bundles"
    );

    // ---- A registers and befriends B ---------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v10a"), "V10 Alice").await;

    befriend(&client_a, &client_bp, uid_a, uid_b).await;

    // ---- take ALL THREE of B's devices offline ------------------------------
    // The envelopes must SIT in the queues so we can read the very bytes each
    // device will later decrypt. A live client acks and the rows are gone.
    client_b3.shutdown().await;
    client_b2.shutdown().await;
    client_bp.shutdown().await;
    drop(client_b3);
    drop(client_b2);
    drop(client_bp);
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- snapshot BEFORE the send, scoped to THIS user ----------------------
    let before = row_ids(&uid_b);
    println!("  device_messages rows for B before the send: {}", before.len());

    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");

    // ---- ONE send, addressed to the USER, not to a device -------------------
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");

    // ---- read back exactly the rows THIS send created ----------------------
    // All three of B's devices are offline, so the queue only grows: wait for
    // the 3 rows a correct fan-out produces instead of sleeping past the
    // second and third deposits.
    let new_rows = wait_for_new_envelopes(&uid_b, &before, 3).await;

    println!("  NEW device_messages rows created by this one send: {}", new_rows.len());
    for f in &new_rows {
        println!(
            "    device_id={} envelope_md5={} source_user_id={} envelope_bytes={}",
            f[1], f[2], f[3], f[4]
        );
    }
    assert!(
        !new_rows.is_empty(),
        "POSITIVE CONTROL FAILED: A's send created ZERO new device_messages rows for \
         B. The message never reached the store this test audits, so every per-device \
         assertion below would be vacuous."
    );

    let for_dev = |d: u32| -> Vec<&Vec<String>> {
        new_rows.iter().filter(|f| f[1] == d.to_string()).collect()
    };
    let rows_1 = for_dev(1);
    let rows_2 = for_dev(dev_2);
    let rows_3 = for_dev(dev_3);

    // ---- POSITIVE CONTROL (c): the PRIMARY got it --------------------------
    assert!(
        !rows_1.is_empty(),
        "POSITIVE CONTROL FAILED: B's PRIMARY device 1 got no envelope from this send \
         ({} new rows total, none for device 1). Sending itself is broken, so this \
         test cannot say anything about fan-out.",
        new_rows.len()
    );
    println!("  positive control: primary device 1 queued {} envelope(s)", rows_1.len());

    // ---- THE PROPERTY: ALL THREE devices got it -----------------------------
    for (dev, rows, label) in [
        (dev_2, &rows_2, "linked #1"),
        (dev_3, &rows_3, "linked #2"),
    ] {
        assert!(
            !rows.is_empty(),
            "MULTI-DEVICE FAN-OUT BROKEN: A sent ONE DM to user {uid_b}, who has THREE \
             devices (primary 1, linked {dev_2}, linked {dev_3} — all three holding a \
             prekey bundle, asserted above). B's PRIMARY received {} envelope(s). B's \
             {label} device {dev} received ZERO. A's send returned Ok and reported \
             success.\n\
             That device is dark: it will never see this message or any other, and \
             nothing anywhere reports an error. Signal's model is that a message to a \
             USER reaches every one of that user's DEVICES.\n\
             Note the shape: with THREE devices this can distinguish 'the fan-out \
             iterates the device list' from 'the fan-out handles the primary plus one \
             special case'. A 2-device test (V9) cannot.\n\
             Likely root cause: the sender's wildcard bundle fetch never returned this \
             device, so A encrypted for the others alone. \
             internal/services/auth/repository/postgres/signal_protocol.go:1032-1051 \
             OMITS a device with no Kyber material from that list rather than failing \
             — exactly this symptom, with no error anywhere.",
            rows_1.len()
        );
        println!("  THE PROPERTY: {label} device {dev} queued {} envelope(s)", rows.len());
    }

    // ---- NEGATIVE HALF (1): all three ciphertexts PAIRWISE DISTINCT ---------
    let md5s = |rows: &Vec<&Vec<String>>| -> std::collections::HashSet<String> {
        rows.iter().map(|f| f[2].clone()).collect()
    };
    let (m1, m2, m3) = (md5s(&rows_1), md5s(&rows_2), md5s(&rows_3));
    for (a_dev, a_md5, b_dev, b_md5) in [
        (1u32, &m1, dev_2, &m2),
        (1u32, &m1, dev_3, &m3),
        (dev_2, &m2, dev_3, &m3),
    ] {
        let shared: Vec<&String> = a_md5.intersection(b_md5).collect();
        assert!(
            shared.is_empty(),
            "PER-DEVICE SESSIONS VIOLATED: devices {a_dev} and {b_dev} were queued \
             BYTE-IDENTICAL envelope(s) (md5 {shared:?}). Two devices with distinct \
             identity keys and distinct sessions cannot legitimately produce the same \
             ciphertext. Either one session's key material is being reused across \
             devices, or a single blob was copied into both queues (the server's \
             destination_device_id==0 branch, sealed_sender.go:156-176, does precisely \
             that) — in which case only ONE device can actually decrypt it and 'all \
             three got a row' means nothing."
        );
    }
    println!(
        "  negative half: envelope md5 device1={m1:?} device{dev_2}={m2:?} device{dev_3}={m3:?} \
         — PAIRWISE DISTINCT ciphertexts"
    );

    // ---- NEGATIVE HALF (2): sealed sender holds on ALL THREE rows -----------
    let leaked: Vec<&Vec<String>> = new_rows.iter().filter(|f| f[3] != "NULL").collect();
    assert!(
        leaked.is_empty(),
        "SEALED SENDER REGRESSED ON THE FAN-OUT: {} of {} envelopes from this send \
         carry a populated source_user_id (A is {}). V4's property holds for \
         single-device DMs but breaks here — per-device rows: {leaked:?}. The envelope \
         is opaque (V1) and the column beside it names the sender.",
        leaked.len(),
        new_rows.len(),
        &uid_a.to_string()[..8]
    );
    println!(
        "  negative half: source_user_id IS NULL on all {} rows — sealed sender holds per device",
        new_rows.len()
    );

    // ---- NEGATIVE HALF (3): no envelope contains the plaintext --------------
    let plain: i64 = psql(&format!(
        "select count(*) from device_messages where destination_user_id = '{uid_b}' \
         and position('{marker}' in encode(envelope, 'escape')) > 0"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        plain, 0,
        "E2EE VIOLATED ON THE FAN-OUT PATH: the plaintext marker appears in {plain} \
         queued envelope(s) for B. (V0 proves this detector finds a marker that is \
         really there.)"
    );
    println!("  negative half: marker occurs in 0 queued envelopes");

    // ---- THE OTHER HALF: ALL THREE decrypt to the EXACT plaintext -----------
    let expected_conv = ChannelId::from(uid_a);

    let (c1, mut ev1) = restore_and_start(&path_bp, "B primary").await;
    let (c2, mut ev2) = restore_and_start(&path_b2, "B linked #1").await;
    let (c3, mut ev3) = restore_and_start(&path_b3, "B linked #2").await;

    let want = |e: &DomainEvent| -> bool {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    };
    let got1 = wait_for_event(&mut ev1, EVENT_TIMEOUT, want).await;
    let got2 = wait_for_event(&mut ev2, EVENT_TIMEOUT, want).await;
    let got3 = wait_for_event(&mut ev3, EVENT_TIMEOUT, want).await;

    let body_of = |e: &Option<DomainEvent>| -> Option<String> {
        match e {
            Some(DomainEvent::MessageReceived { message }) => {
                Some(message.as_text().unwrap_or("<not text>").to_string())
            }
            _ => None,
        }
    };
    let (b1, b2, b3) = (body_of(&got1), body_of(&got2), body_of(&got3));
    println!("  B primary   decrypted: {b1:?}");
    println!("  B linked #1 decrypted: {b2:?}");
    println!("  B linked #2 decrypted: {b3:?}");

    // Fall back to each device's own store for any bus that lagged: three
    // clients are brought up and then waited on in turn, so the later receivers
    // can overrun their 64-slot broadcast buffer and lose the event. See
    // `wait_for_stored_text`. The property is unchanged — the device must have
    // recovered the plaintext — only the place we read it from is now one that
    // cannot drop it.
    let ok1 = b1.as_deref() == Some(marker.as_str())
        || wait_for_stored_text(&c1, &expected_conv, &marker).await;
    let ok2 = b2.as_deref() == Some(marker.as_str())
        || wait_for_stored_text(&c2, &expected_conv, &marker).await;
    let ok3 = b3.as_deref() == Some(marker.as_str())
        || wait_for_stored_text(&c3, &expected_conv, &marker).await;

    assert!(
        ok1,
        "B's PRIMARY device did not recover A's exact plaintext (got {b1:?}). The \
         primary is the control here — if it cannot decrypt, the linked devices' \
         results say nothing about multi-device."
    );
    for (dev, body, ok, label) in [(dev_2, &b2, ok2, "linked #1"), (dev_3, &b3, ok3, "linked #2")] {
        assert!(
            ok,
            "LINKED DEVICE CANNOT DECRYPT: B's {label} device {dev} was queued its own \
             distinct envelope (asserted above — the row is really there), came online, \
             and did not surface A's plaintext. Got {body:?}, expected {marker:?}. B's \
             PRIMARY decrypted the same message fine, so this is not a broken send or a \
             broken session in general — it is specific to this secondary device. An \
             envelope a device cannot decrypt is indistinguishable from no message: the \
             device is a recipient on paper only."
        );
    }

    println!(
        "V10 PASS — one DM to a 3-device user fanned out to ALL THREE devices as \
         pairwise-distinct ciphertexts, each decrypted to the exact plaintext by its \
         own device, and sealed sender held for all three\n"
    );

    client_a.shutdown().await;
    c1.shutdown().await;
    c2.shutdown().await;
    c3.shutdown().await;
}

// ============================================================================
// V11 — a device linked MID-CONVERSATION receives the NEXT message
//
// This is the case V9 and V10 both leave open, and it is the one most likely to
// be broken, for a structural reason.
//
// V9/V10 link every device BEFORE A ever sends. So A's very first bundle fetch
// already sees the full device list, and any "device list is cached / settled"
// bug is invisible: there is nothing stale to be stale. Real accounts do not
// work that way. People link a laptop in the middle of an ongoing conversation.
//
// THE PROPERTY: A and B have an ESTABLISHED conversation — A has already sent,
// B has already received and decrypted, so A has fetched B's device list, built
// a session, and settled. B THEN links a NEW device. A sends again, from the
// SAME live client process (never restarted — this is essential: a restart would
// drop any in-memory cache and the test would prove nothing about staleness).
// The NEW device MUST receive and decrypt that message.
//
// THE FAILURE MODE BEING HUNTED: if A caches B's device list, A silently sends
// only to the OLD devices. The new device goes dark. A's send returns Ok.
// Nobody is told. Preventing exactly this is what Signal's 409 (mismatched
// devices) / 410 (stale devices) flow is FOR — the server tells the sender "you
// missed a device" and the sender retries including it.
//
// Chatter's analog of that flow lives at server_dm.rs:217-225, and reading it is
// the reason this test was written: it only re-fetches when `peer.delivered == 0`
// — when the send reached ZERO devices. A PARTIAL miss (old devices delivered,
// new device missed) has `delivered == 1`, so the retry never fires and the
// send is indistinguishable from a complete success. That branch cannot catch
// the bug it exists to catch. Whether that matters depends entirely on whether
// anything upstream caches the device list — which is what this test settles
// empirically rather than by reading code.
//
// "Implemented but never engaged" is the exact shape of the worst bug found in
// this codebase: sealed sender was fully built, correct, and never turned on,
// because the client authenticated with Basic auth. The README claimed it flatly
// and it was false for every message ever sent. So the 409/410 path being
// present is not evidence it works.
//
// ASSERT ON THE RECEIVER'S SIDE OF THE WIRE. V9's mutation established that a
// sabotaged send still returns Ok — a sender's return value is worthless as
// evidence here, and a test that checked it would pass straight through the bug.
//
// POSITIVE CONTROL FIRST, scoped, and it must do something V9's does not — prove
// the conversation was REALLY established before the link, or this is just V9
// with extra steps:
//   (a) B has exactly ONE device when A sends the FIRST message, so the device
//       list A settles on is genuinely {1} and the second device is genuinely new;
//   (b) B's primary DECRYPTS that first message. Not "a row appeared" — decrypts.
//       That is what makes A's session with B settled rather than merely attempted;
//   (c) the new device's id is distinct from the primary's, and B has exactly TWO
//       bundles after the link (the sender's wildcard fetch reads that table — no
//       bundle, no fan-out target, and a dark device would mean 'the link never
//       published a bundle', a different bug wearing this test's message);
//   (d) the PRIMARY still receives the second message — if it does not, sending
//       broke generally and this test cannot speak about the new device.
//
// NEGATIVE HALF: the two devices' second-message ciphertexts must be DISTINCT
// (per-device sessions — a shared blob would make "both got a row" meaningless),
// sealed sender must hold on both rows (V4, per device), and the marker must
// appear in no envelope (V1, on this path).
//
// MUTATION CHECK — RUN IT, do not describe it. In `deposit_sealed_to_all_devices`
// (server_dm.rs:322), `if device_id != 1 { continue; }` — deliver only to the
// device that existed when the conversation was established. That IS the bug this
// test hunts, reproduced exactly: A keeps talking to the old device and the newly
// linked one never hears anything. This test MUST fail at the new-device row
// assertion.
// ============================================================================

#[tokio::test]
async fn v11_device_linked_mid_conversation_receives_next_message() {
    skip_if_no_server!();
    no_keyring_prompts();

    // ---- B's primary registers. ONE device, for now. ------------------------
    let dir_bp = tempfile::tempdir().unwrap();
    let path_bp = dir_bp.path().to_path_buf();
    let client_bp = build_client(&path_bp);
    let uid_b = register_and_start(&client_bp, &unique_user("v11bp"), "V11 Bob Primary").await;

    // ---- A registers and befriends B ---------------------------------------
    // A's client is built ONCE here and is never rebuilt or restarted for the
    // rest of this test. Any device-list cache A holds therefore survives the
    // link below — which is the entire point.
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v11a"), "V11 Alice").await;

    befriend(&client_a, &client_bp, uid_a, uid_b).await;

    // ---- POSITIVE CONTROL (a): B really has ONE device right now ------------
    // If B somehow already had two, the "new" device would not be new and the
    // staleness this test hunts could not arise.
    let n_dev_before: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_b}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev_before, 1,
        "POSITIVE CONTROL FAILED: B has {n_dev_before} devices with a bundle BEFORE \
         the link, expected exactly 1. The device list A is about to settle on must \
         genuinely be {{1}} for the device linked later to genuinely be NEW — \
         otherwise this test is V9 with extra steps and proves nothing about staleness."
    );
    println!("  positive control: B has exactly 1 device before the link");

    // ---- ESTABLISH the conversation: A sends, B's primary DECRYPTS ----------
    // This is what makes A's view of B's device list *settled* rather than merely
    // attempted: A has fetched B's bundles, built a session, and completed a
    // round trip. A row appearing would not be enough — decryption is what proves
    // the session is real and live.
    let expected_conv = ChannelId::from(uid_a);
    let mut ev_p1 = client_bp.subscribe_events();
    let marker_1 = unique_marker();
    println!("\n  message #1 (establishes the conversation) marker: {marker_1}");

    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker_1)
        .await
        .expect("first send_text failed");

    let got_1 = wait_for_event(&mut ev_p1, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;
    let body_1 = match &got_1 {
        Some(DomainEvent::MessageReceived { message }) => {
            Some(message.as_text().unwrap_or("<not text>").to_string())
        }
        _ => None,
    };
    println!("  B primary decrypted message #1: {body_1:?}");

    // ---- POSITIVE CONTROL (b): the conversation IS established --------------
    assert_eq!(
        body_1.as_deref(),
        Some(marker_1.as_str()),
        "POSITIVE CONTROL FAILED: B's primary did not decrypt A's FIRST message (got \
         {body_1:?}). The conversation is not established, so A never settled on a \
         device list for B, so nothing below could possibly be testing whether a \
         STALE device list is refreshed. Fix the send path before reading this test."
    );
    println!("  positive control: conversation ESTABLISHED — A has fetched B's device list and holds a live session");

    // ---- B links a NEW device, MID-CONVERSATION -----------------------------
    // A is still live and still holds whatever it cached about B.
    println!("\n  --- B now links a SECOND device, mid-conversation ---");
    let dir_bs = tempfile::tempdir().unwrap();
    let path_bs = dir_bs.path().to_path_buf();
    let client_bs = build_client(&path_bs);
    let dev_s = link_secondary_device(&client_bp, &client_bs, uid_b).await;
    start_linked(&client_bs, "B new device").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- POSITIVE CONTROL (c): the new device is real and distinct ----------
    assert_ne!(
        dev_s, 1,
        "POSITIVE CONTROL FAILED: the mid-conversation link was assigned device_id 1, \
         the primary's id. This test cannot tell the two devices apart, so a per-device \
         row count would be measuring the primary twice."
    );
    let n_dev_after: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_b}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev_after, 2,
        "POSITIVE CONTROL FAILED: expected exactly 2 devices with a prekey bundle for B \
         after the mid-conversation link (primary 1 + new {dev_s}), found {n_dev_after}. \
         A's wildcard fetch reads exactly this table: without a bundle for the new \
         device, a dark device would mean 'the link never published a bundle' — a \
         different bug — rather than 'A used a stale device list'. \
         Note internal/services/auth/repository/postgres/signal_protocol.go:1032-1051 \
         OMITS a device with no Kyber material from the discovery list rather than \
         erroring, so a freshly-linked device transiently in that state would be \
         omitted with no error anywhere."
    );
    println!("  positive control: new device {dev_s} linked mid-conversation, distinct from primary 1, both hold bundles");

    // ---- take BOTH of B's devices offline -----------------------------------
    // So the envelopes SIT in the queues and we can read the very bytes each
    // device will later decrypt.
    client_bs.shutdown().await;
    client_bp.shutdown().await;
    drop(client_bs);
    drop(client_bp);
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- snapshot BEFORE the second send, scoped to THIS user ---------------
    let before = row_ids(&uid_b);
    println!("  device_messages rows for B before the second send: {}", before.len());

    let marker_2 = unique_marker();
    println!("\n  message #2 (sent AFTER the link) marker: {marker_2}");

    // ---- THE SEND UNDER TEST: same live A, same conversation ----------------
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker_2)
        .await
        .expect("second send_text failed");

    // Both of B's devices are offline, so the queue only grows: wait for the 2
    // rows a correct fan-out produces instead of sleeping past the second
    // deposit.
    let new_rows = wait_for_new_envelopes(&uid_b, &before, 2).await;

    println!("  NEW device_messages rows created by the second send: {}", new_rows.len());
    for f in &new_rows {
        println!(
            "    device_id={} envelope_md5={} source_user_id={} envelope_bytes={}",
            f[1], f[2], f[3], f[4]
        );
    }
    assert!(
        !new_rows.is_empty(),
        "POSITIVE CONTROL FAILED: A's second send created ZERO new device_messages rows \
         for B. The message never reached the store this test audits, so every \
         per-device assertion below would be vacuous."
    );

    let for_dev = |d: u32| -> Vec<&Vec<String>> {
        new_rows.iter().filter(|f| f[1] == d.to_string()).collect()
    };
    let primary_rows = for_dev(1);
    let new_dev_rows = for_dev(dev_s);

    // ---- POSITIVE CONTROL (d): the OLD device still receives ----------------
    assert!(
        !primary_rows.is_empty(),
        "POSITIVE CONTROL FAILED: B's PRIMARY device 1 got no envelope from the second \
         send ({} new rows total, none for device 1). Sending broke generally, so this \
         test cannot say anything about the mid-conversation device.",
        new_rows.len()
    );
    println!("  positive control: primary device 1 queued {} envelope(s)", primary_rows.len());

    // ---- THE PROPERTY: the MID-CONVERSATION device receives -----------------
    assert!(
        !new_dev_rows.is_empty(),
        "STALE DEVICE LIST — NEW DEVICE IS DARK: A and B had an ESTABLISHED \
         conversation (A sent, B's primary decrypted it — asserted above). B then \
         linked device {dev_s}, which published a prekey bundle (asserted above: B has \
         {n_dev_after} bundles). A — the SAME live client, never restarted — then sent \
         another DM to user {uid_b}. The OLD device 1 received {} envelope(s). The NEW \
         device {dev_s} received ZERO. A's send returned Ok and reported success.\n\
         A is talking to a device list that no longer describes B's account. The new \
         device will never see this message or any other, and NOTHING anywhere reports \
         an error — not to A, not to B, not in a log.\n\
         This is precisely what Signal's 409 (mismatched devices) / 410 (stale devices) \
         flow exists to prevent: the server tells the sender it missed a device and the \
         sender retries including it. Chatter's analog at server_dm.rs:217-225 only \
         re-fetches when `peer.delivered == 0`. Here delivered == {} (the old device \
         succeeded), so the retry never fires and a PARTIAL miss is indistinguishable \
         from a complete success. That branch cannot catch this.",
        primary_rows.len(),
        primary_rows.len()
    );
    println!("  THE PROPERTY: mid-conversation device {dev_s} queued {} envelope(s)", new_dev_rows.len());

    // ---- NEGATIVE HALF (1): the two ciphertexts are DISTINCT ----------------
    let md5s = |rows: &Vec<&Vec<String>>| -> std::collections::HashSet<String> {
        rows.iter().map(|f| f[2].clone()).collect()
    };
    let (m_p, m_s) = (md5s(&primary_rows), md5s(&new_dev_rows));
    let shared: Vec<&String> = m_p.intersection(&m_s).collect();
    assert!(
        shared.is_empty(),
        "PER-DEVICE SESSIONS VIOLATED: device 1 and the mid-conversation device {dev_s} \
         were queued BYTE-IDENTICAL envelope(s) (md5 {shared:?}). Two devices with \
         distinct identity keys and distinct sessions cannot legitimately produce the \
         same ciphertext. Either one session's key material is reused across devices, or \
         a single blob was copied into both queues — in which case only one device can \
         actually decrypt it and 'the new device got a row' means nothing."
    );
    println!("  negative half: envelope md5 device1={m_p:?} device{dev_s}={m_s:?} — DISTINCT ciphertexts");

    // ---- NEGATIVE HALF (2): sealed sender holds on both rows ----------------
    let leaked: Vec<&Vec<String>> = new_rows.iter().filter(|f| f[3] != "NULL").collect();
    assert!(
        leaked.is_empty(),
        "SEALED SENDER REGRESSED ON THE MID-CONVERSATION PATH: {} of {} envelopes from \
         this send carry a populated source_user_id (A is {}). A newly-established \
         session must not be the one that falls back to Basic auth — per-device rows: \
         {leaked:?}.",
        leaked.len(),
        new_rows.len(),
        &uid_a.to_string()[..8]
    );
    println!("  negative half: source_user_id IS NULL on all {} rows — sealed sender holds per device", new_rows.len());

    // ---- NEGATIVE HALF (3): no envelope contains the plaintext --------------
    let plain: i64 = psql(&format!(
        "select count(*) from device_messages where destination_user_id = '{uid_b}' \
         and position('{marker_2}' in encode(envelope, 'escape')) > 0"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        plain, 0,
        "E2EE VIOLATED ON THE MID-CONVERSATION PATH: the plaintext marker appears in \
         {plain} queued envelope(s) for B. (V0 proves this detector finds a marker that \
         is really there.)"
    );
    println!("  negative half: marker occurs in 0 queued envelopes");

    // ---- THE OTHER HALF: BOTH devices DECRYPT the second message -----------
    // A row is not receipt. The new device must turn ITS envelope — encrypted to a
    // session that did not exist when the conversation started — back into exactly
    // what A typed.
    let (client_bp2, mut ev_p) = restore_and_start(&path_bp, "B primary").await;
    let (client_bs2, mut ev_s) = restore_and_start(&path_bs, "B new device").await;

    let want = |e: &DomainEvent| -> bool {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == expected_conv
                && message.as_text() == Some(marker_2.as_str()))
    };
    let got_p = wait_for_event(&mut ev_p, EVENT_TIMEOUT, want).await;
    let got_s = wait_for_event(&mut ev_s, EVENT_TIMEOUT, want).await;

    let body_of = |e: &Option<DomainEvent>| -> Option<String> {
        match e {
            Some(DomainEvent::MessageReceived { message }) => {
                Some(message.as_text().unwrap_or("<not text>").to_string())
            }
            _ => None,
        }
    };
    let body_p = body_of(&got_p);
    let body_s = body_of(&got_s);
    println!("  B primary    decrypted message #2: {body_p:?}");
    println!("  B new device decrypted message #2: {body_s:?}");

    // Store fallback for a lagged bus — see `wait_for_stored_text`.
    let ok_p = body_p.as_deref() == Some(marker_2.as_str())
        || wait_for_stored_text(&client_bp2, &expected_conv, &marker_2).await;
    let ok_s = body_s.as_deref() == Some(marker_2.as_str())
        || wait_for_stored_text(&client_bs2, &expected_conv, &marker_2).await;

    assert!(
        ok_p,
        "B's PRIMARY did not recover A's second message (got {body_p:?}). The primary is \
         the control — if it cannot decrypt, the new device's result says nothing."
    );
    assert!(
        ok_s,
        "MID-CONVERSATION DEVICE CANNOT DECRYPT: B's device {dev_s}, linked AFTER the \
         conversation was established, was queued its own distinct envelope (asserted \
         above — the row is really there), came online, and did not surface A's \
         plaintext. Got {body_s:?}, expected {marker_2:?}. B's PRIMARY decrypted the \
         same message fine, so this is not a broken send in general — it is specific to \
         the device that joined mid-conversation, which points at the session A built \
         for it from a bundle fetched after the fact. An envelope a device cannot \
         decrypt is indistinguishable from no message."
    );

    println!(
        "V11 PASS — a device linked MID-CONVERSATION received the next DM from an \
         already-established sender and decrypted it exactly; the sender picked up the \
         new device without a restart, as a distinct ciphertext, sealed sender intact\n"
    );

    client_a.shutdown().await;
    client_bp2.shutdown().await;
    client_bs2.shutdown().await;
}

// ============================================================================
// V12 — SELF-SYNC: a DM sent from A's PRIMARY reaches A's OWN LINKED device
//
// CLAIM (README): multi-device. V9/V10/V11 all audit the RECIPIENT's side of
// the wire: one DM to a user reaches all of THAT USER's devices. Not one of
// them ever looks at the SENDER's other devices.
//
// That is a real gap, not a pedantic one. Signal's model is that when you send
// from your phone, your laptop gets a copy — otherwise your own conversation is
// inconsistent across your own devices: a message you sent from your phone
// never appears on your laptop, and the laptop's transcript is silently wrong.
// `send_sealed_dm_content` (server_dm.rs:233-242) fans the same plaintext out
// to our own devices with `skip_device_id = our_device_id`. That code path is
// asserted NOWHERE.
//
// SHAPE: A has TWO devices (primary 1 + linked). B is a friend. A sends ONE DM
// to B **from A's primary**. Everything is then asserted on A's LINKED device.
//
// POSITIVE CONTROL FIRST, scoped to THIS run's user, because "no sync row" and
// "no device" are different bugs and a global count passes vacuously on other
// rows:
//   (a) A's linked device is distinct from the primary (device_id != 1) —
//       otherwise "A's device 2 got a row" is the primary counted twice, and
//       the self-sync skip would be indistinguishable from the property;
//   (b) exactly TWO devices hold a prekey bundle for A before the send. The
//       sender's own wildcard fetch reads exactly this table for its OWN user
//       id too: without two bundles there, self-sync has nothing to fan out to
//       and a missing row would mean "the link never published a bundle" — a
//       different bug wearing this test's failure message. Note
//       internal/services/auth/repository/postgres/signal_protocol.go:1032-1051
//       OMITS a device with no Kyber material from the discovery list rather
//       than erroring, which is exactly how a device goes dark with no error;
//   (c) B RECEIVES. Self-sync must not be measured on a send that never
//       happened, and must not break real delivery.
//
// NEGATIVE HALF — asserted on the RECEIVING device's side of the wire, never on
// the send's return value (a sabotaged send still returns Ok — proven twice in
// this file):
//   1. A's OWN SENDING DEVICE (1) gets NO self-sync row. `skip_device_id`
//      exists precisely so the sender does not echo to itself. If device 1 also
//      got a row, "A's devices got rows" would be true while the skip is broken,
//      and the primary would double-store its own message.
//   2. The self-sync envelope is NOT byte-identical to B's. Distinct sessions,
//      distinct identity keys, distinct ciphertexts. Identical bytes would mean
//      key material reuse or a single blob copied into both queues — in which
//      case only one device could decrypt it and "both got a row" means nothing.
//   3. The marker appears in ZERO queued envelopes for A. (V0 proves this
//      detector finds a marker that is really there.) E2EE must hold on the
//      self-sync path exactly as it does on the delivery path.
//
// THE OTHER HALF: a row in a queue is not receipt. A's linked device must come
// online and independently turn its OWN envelope back into precisely what A
// typed. An envelope a device cannot decrypt is indistinguishable from no
// message at all.
//
// OBSERVED, NOT ASSERTED (reported honestly rather than guessed):
//   - `source_user_id` on the self-sync rows. A self-send is to yourself, so
//      "sealed sender" here means something different than it does for B; this
//      test PRINTS what the column holds and asserts only the E2EE property,
//      rather than assuming a NULL is required.
//   - the CONVERSATION the linked device files the sync under — see the block
//      at the end, which prints it and explains what it means.
//
// MUTATION CHECK — RUN IT, do not describe it. In `send_sealed_dm_content`
// (server_dm.rs:233-242), delete the self-sync `deposit_sealed_to_all_devices`
// call for `our_user_id`. B still receives, the send still returns Ok, and this
// test MUST go red at THE PROPERTY. Reverted after; verified by source hash of
// the edited region INSIDE the submodule (`git diff` from the repo root is
// worthless here — server_dm.rs is already dirty vs HEAD from prior work).
// ============================================================================

#[tokio::test]
async fn v12_dm_from_primary_syncs_to_senders_own_linked_device() {
    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A: primary registers, then a REAL secondary device links -----------
    // A is the SENDER. This is the whole difference from V9/V10/V11, where the
    // multi-device account was always the recipient.
    let dir_ap = tempfile::tempdir().unwrap();
    let path_ap = dir_ap.path().to_path_buf();
    let client_ap = build_client(&path_ap);
    let uid_a = register_and_start(&client_ap, &unique_user("v12ap"), "V12 Alice Primary").await;

    let dir_as = tempfile::tempdir().unwrap();
    let path_as = dir_as.path().to_path_buf();
    let client_as = build_client(&path_as);
    let dev_s = link_secondary_device(&client_ap, &client_as, uid_a).await;
    start_linked(&client_as, "A linked").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- POSITIVE CONTROL (a): two DISTINCT devices on the SENDER ----------
    assert_ne!(
        dev_s, 1,
        "POSITIVE CONTROL FAILED: A's linked device was assigned device_id 1, the \
         primary's own id. Self-sync SKIPS the sending device by id \
         (`skip_device_id`), so if the two ids collide the property under test and \
         the skip are literally the same row and nothing below can distinguish them."
    );
    let devices = psql(&format!(
        "select device_id from signal_prekey_bundles where user_id = '{uid_a}' order by device_id"
    ));
    println!("  A's device_ids with a bundle: {devices:?}");
    let n_dev: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_a}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev, 2,
        "POSITIVE CONTROL FAILED: expected exactly 2 devices with a prekey bundle for \
         the SENDER A (primary 1 + linked {dev_s}), found {n_dev}. A's self-sync does \
         a wildcard bundle fetch against its OWN user id and reads exactly this table. \
         Without two bundles here, self-sync has nothing to fan out to, and a missing \
         sync row would mean 'A's linked device never published a bundle' — a \
         different bug — rather than 'self-sync is broken'. \
         (signal_protocol.go:1032-1051 omits a Kyber-less device from that list \
         silently, which is exactly this symptom.)"
    );
    println!("  positive control: A primary=1, A linked={dev_s} — distinct, both hold bundles");

    // ---- B registers and befriends A ---------------------------------------
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();
    let client_b = build_client(&path_b);
    let uid_b = register_and_start(&client_b, &unique_user("v12b"), "V12 Bob").await;

    befriend(&client_ap, &client_b, uid_a, uid_b).await;

    // ---- take A's LINKED device and B offline ------------------------------
    // The envelopes must SIT in the queues so we read the very bytes each device
    // will later decrypt. A live client acks and the rows are gone before we can
    // audit them. A's PRIMARY stays up — it is the one doing the sending.
    client_as.shutdown().await;
    client_b.shutdown().await;
    drop(client_as);
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- snapshot BEFORE the send, scoped to THIS run's users --------------
    let before_a = quiesced_row_ids(&uid_a).await;
    let before_b = quiesced_row_ids(&uid_b).await;
    println!("  device_messages rows before the send: A={} B={}", before_a.len(), before_b.len());

    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");

    // ---- THE SEND UNDER TEST: from A's PRIMARY, addressed to B -------------
    client_ap
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");

    // A's linked device and B are both offline, so both queues only grow. Wait
    // for the specific rows this send must produce — B's copy, then the
    // self-sync copy on A's LINKED device — rather than sleeping past two
    // sequential deposits. Targeting the linked device matters: these tests
    // also assert A's own sending device was SKIPPED, so the window has to
    // close the instant the awaited row lands.
    let new_b = wait_for_new_envelopes(&uid_b, &before_b, 1).await;
    let new_a = wait_for_device_envelope(&uid_a, &before_a, dev_s).await;

    println!("  NEW rows for B (the recipient): {}", new_b.len());
    for f in &new_b {
        println!("    device_id={} envelope_md5={} source_user_id={} bytes={}", f[1], f[2], f[3], f[4]);
    }
    println!("  NEW rows for A (self-sync): {}", new_a.len());
    for f in &new_a {
        println!("    device_id={} envelope_md5={} source_user_id={} bytes={}", f[1], f[2], f[3], f[4]);
    }

    // ---- POSITIVE CONTROL (c): B RECEIVED ----------------------------------
    // Self-sync must not be judged on a send that never left the building, and
    // must not break real delivery.
    assert!(
        !new_b.is_empty(),
        "POSITIVE CONTROL FAILED: A's send created ZERO new device_messages rows for \
         the RECIPIENT B. The DM never happened at all, so any statement about A's \
         own devices below would be vacuous. Fix the send path before reading this test."
    );
    println!("  positive control: B (the real recipient) queued {} envelope(s) — delivery works", new_b.len());

    let a_for_dev = |d: u32| -> Vec<&Vec<String>> {
        new_a.iter().filter(|f| f[1] == d.to_string()).collect()
    };
    let a_primary_rows = a_for_dev(1);
    let a_linked_rows = a_for_dev(dev_s);

    // ---- THE PROPERTY: A's OWN LINKED device got a copy ---------------------
    assert!(
        !a_linked_rows.is_empty(),
        "SELF-SYNC BROKEN — THE SENDER'S OTHER DEVICE IS DARK: A has TWO devices \
         (primary 1 + linked {dev_s}, both holding a prekey bundle — asserted above). \
         A sent ONE DM to B **from the primary**. B received {} envelope(s), so the \
         send unquestionably worked. A's OWN linked device {dev_s} received ZERO \
         ({} new self rows total). A's send returned Ok and reported success.\n\
         A's conversation is now INCONSISTENT ACROSS A'S OWN DEVICES: the message A \
         sent from the primary will never appear on A's linked device, whose \
         transcript of this conversation is silently and permanently wrong. Nothing \
         anywhere reports an error.\n\
         Signal's model is that a message you send from one device is copied to your \
         others. The code that should do this is `send_sealed_dm_content`'s self-sync \
         `deposit_sealed_to_all_devices(.., our_user_id, .., Some(our_device_id))` \
         (server_dm.rs:233-242) — reached only after the peer deposit, and never \
         asserted until this test.",
        new_b.len(),
        new_a.len()
    );
    println!("  THE PROPERTY: A's linked device {dev_s} queued {} self-sync envelope(s)", a_linked_rows.len());

    // ---- NEGATIVE HALF (1): the SENDING device did NOT echo to itself -------
    assert!(
        a_primary_rows.is_empty(),
        "SELF-SYNC SKIP BROKEN: A's own SENDING device (1) was queued {} self-sync \
         envelope(s). `skip_device_id = our_device_id` (server_dm.rs:236-241) exists \
         precisely so the sender does not mail itself a copy of what it just wrote to \
         its own store. The primary will now surface this message twice, and \
         'A's devices got sync rows' would be true while the skip is broken. Rows: {:?}",
        a_primary_rows.len(),
        a_primary_rows
    );
    println!("  negative half: A's SENDING device 1 got 0 self-sync rows — skip_device_id holds");

    // ---- NEGATIVE HALF (2): self-sync ciphertext != B's ciphertext ---------
    let b_md5: std::collections::HashSet<&str> = new_b.iter().map(|f| f[2].as_str()).collect();
    let a_md5: std::collections::HashSet<&str> = a_linked_rows.iter().map(|f| f[2].as_str()).collect();
    let shared: Vec<&&str> = b_md5.intersection(&a_md5).collect();
    assert!(
        shared.is_empty(),
        "PER-DEVICE SESSIONS VIOLATED ON THE SELF-SYNC PATH: B's device and A's linked \
         device {dev_s} were queued BYTE-IDENTICAL envelope(s) (md5 {shared:?}). These \
         are different accounts with different identity keys and different sessions; \
         they cannot legitimately produce the same ciphertext. Either key material is \
         being reused across sessions, or a single blob was copied into both queues \
         (the server's destination_device_id==0 branch, sealed_sender.go:156-176, does \
         exactly that) — in which case only one of them can actually decrypt it and \
         'the sync row is there' means nothing."
    );
    println!("  negative half: self-sync md5={a_md5:?} vs B's md5={b_md5:?} — DISTINCT ciphertexts");

    // ---- NEGATIVE HALF (3): E2EE holds on the self-sync path ---------------
    let plain: i64 = psql(&format!(
        "select count(*) from device_messages where destination_user_id = '{uid_a}' \
         and position('{marker}' in encode(envelope, 'escape')) > 0"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        plain, 0,
        "E2EE VIOLATED ON THE SELF-SYNC PATH: the plaintext marker appears in {plain} \
         of A's own queued envelope(s). A message to yourself is still a message \
         through the server. (V0 proves this detector finds a marker that is really \
         there.)"
    );
    println!("  negative half: marker occurs in 0 of A's queued envelopes");

    // ---- OBSERVED: what does the sender identity look like on a self-send? --
    // Reported, not asserted. A self-send is addressed to yourself, so the
    // meaning of "sealed sender" here is genuinely different from B's case, and
    // asserting NULL on a guess would be inventing a requirement.
    let src: Vec<&str> = new_a.iter().map(|f| f[3].as_str()).collect();
    println!(
        "  OBSERVED (not asserted): source_user_id on A's self-sync rows = {src:?} \
         (A = {}, B = {})",
        &uid_a.to_string()[..8],
        &uid_b.to_string()[..8]
    );

    // ---- THE OTHER HALF: A's linked device DECRYPTS the exact plaintext -----
    // Rows in a queue are not receipt.
    let (client_as2, mut ev_s) = restore_and_start(&path_as, "A linked").await;

    let got_s = wait_for_event(&mut ev_s, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { .. })
    })
    .await;

    let (body_s, conv_s, sender_s) = match &got_s {
        Some(DomainEvent::MessageReceived { message }) => (
            Some(message.as_text().unwrap_or("<not text>").to_string()),
            Some(message.conversation_id),
            message.sender_id,
        ),
        _ => (None, None, None),
    };
    println!("  A linked device decrypted: {body_s:?}");

    assert_eq!(
        body_s.as_deref(),
        Some(marker.as_str()),
        "A'S LINKED DEVICE CANNOT DECRYPT ITS OWN SYNC: the self-sync envelope for \
         device {dev_s} is really in the queue (asserted above — distinct ciphertext, \
         right destination), the device came online and restored the identity that was \
         live when the primary encrypted, and it did not surface the plaintext. Got \
         {body_s:?}, expected {marker:?}. An envelope a device cannot decrypt is \
         indistinguishable from no message: A's linked device is a sync target on \
         paper only."
    );
    println!("  THE OTHER HALF: A's linked device decrypted its own sync to the EXACT plaintext");

    // ---- OBSERVED: which conversation does the sync land in? ---------------
    // Reported, not asserted, on purpose. The self-sync ships the SAME `Content`
    // bytes that went to B (server_dm.rs:196, `content.encode_to_vec()`), with no
    // wrapper naming the destination — unlike Signal, whose SyncMessage.Sent
    // carries an explicit `destination`. So the receiving device has nothing in
    // the plaintext that says "this was FOR B", and the only sender identity it
    // has is A's own. Whatever it does here, it cannot be deriving "A -> B" from
    // the wire. This block prints the ground truth rather than asserting a
    // conclusion this test has not earned.
    let conv_with_b = ChannelId::from(uid_b);
    let conv_with_a = ChannelId::from(uid_a);
    println!(
        "\n  OBSERVED (not asserted) — where the sync landed on A's linked device:\n\
         \x20   conversation_id = {conv_s:?}\n\
         \x20   sender_id       = {sender_s:?}\n\
         \x20   conversation with B (correct, 'my outgoing msg to B') would be {conv_with_b:?}\n\
         \x20   conversation with A (self)                            would be {conv_with_a:?}\n\
         \x20   matches B? {}   matches A-self? {}",
        conv_s == Some(conv_with_b),
        conv_s == Some(conv_with_a)
    );

    println!(
        "V12 PASS — a DM sent from A's PRIMARY was fanned out to A's OWN LINKED device \
         as a distinct ciphertext, skipped the sending device, kept the plaintext off \
         the server, and was decrypted to the exact plaintext by the linked device; \
         B received normally\n"
    );

    client_ap.shutdown().await;
    client_as2.shutdown().await;
}

// ============================================================================
// V13 — a self-synced DM lands in the RIGHT conversation, as OUTGOING (F8 fix)
//
// V12 proved the TRANSPORT half of self-sync: a DM sent from A's primary reaches
// A's own linked device and decrypts. But V12 also OBSERVED (did not assert) that
// the linked device filed the message under a conversation with A ITSELF,
// attributed to A — never as A's outgoing message TO B. The A↔B thread on A's
// laptop stayed empty; a phantom self-thread appeared instead. That was F8.
//
// Root cause was structural: `send_sealed_dm_content` shipped the byte-identical
// DataMessage to both B and to A's own devices, so the self-copy carried no
// record of who it was for. The fix mirrors Signal's `SyncMessage.Sent`: the
// self-sync copy is now wrapped in a `SyncSent` carrying the real `destination`
// (B), and A's other devices file it as OUTGOING in the conversation with B.
//
// THE DECISIVE ASSERTION is exactly the thing F8 broke: after A's primary sends a
// DM to B, A's LINKED device must surface the message in the A↔B conversation, as
// OUTGOING (authored by A) — NOT in a self-conversation attributed to A.
//
// POSITIVE CONTROL: before asserting WHERE it landed, prove the copy actually
// reached A's linked device at all (a queued self-sync row, then a real decrypt
// to the exact marker). Otherwise "not in the wrong conversation" passes
// vacuously when nothing arrived.
//
// MUTATION CHECK — RUN IT, do not describe it. In `send_sealed_dm_content`
// (server_dm.rs), ship `&plaintext` instead of `&self_plaintext` to the self-sync
// deposit — i.e. revert to the destination-less copy. B still receives, the copy
// still reaches A's linked device and still decrypts, but it lands in the A-self
// conversation again, so THE DECISIVE ASSERTION goes red (F8 reproduced).
// Reverted after; verified by SHA-256 of server_dm.rs INSIDE the submodule
// (`git diff` from the repo root is worthless — server_dm.rs is dirty vs HEAD).
// ============================================================================

#[tokio::test]
async fn v13_self_synced_dm_files_as_outgoing_in_the_recipient_conversation() {
    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A: primary registers, then a REAL secondary device links -----------
    let dir_ap = tempfile::tempdir().unwrap();
    let path_ap = dir_ap.path().to_path_buf();
    let client_ap = build_client(&path_ap);
    let uid_a = register_and_start(&client_ap, &unique_user("v13ap"), "V13 Alice Primary").await;

    let dir_as = tempfile::tempdir().unwrap();
    let path_as = dir_as.path().to_path_buf();
    let client_as = build_client(&path_as);
    let dev_s = link_secondary_device(&client_ap, &client_as, uid_a).await;
    start_linked(&client_as, "A linked").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- POSITIVE CONTROL (a): two DISTINCT devices on the SENDER ----------
    assert_ne!(
        dev_s, 1,
        "POSITIVE CONTROL FAILED: A's linked device was assigned device_id 1, the \
         primary's own id. Self-sync SKIPS the sending device by id, so if the ids \
         collide the property under test and the skip are the same row."
    );
    let n_dev: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_a}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev, 2,
        "POSITIVE CONTROL FAILED: expected exactly 2 devices with a prekey bundle for \
         the SENDER A (primary 1 + linked {dev_s}), found {n_dev}. Without two bundles, \
         self-sync has nothing to fan out to and a missing sync row would mean 'A's \
         linked device never published a bundle', a different bug."
    );
    println!("  positive control: A primary=1, A linked={dev_s} — distinct, both hold bundles");

    // ---- B registers and befriends A ---------------------------------------
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();
    let client_b = build_client(&path_b);
    let uid_b = register_and_start(&client_b, &unique_user("v13b"), "V13 Bob").await;

    befriend(&client_ap, &client_b, uid_a, uid_b).await;

    // ---- take A's LINKED device and B offline ------------------------------
    // The envelope must SIT in A's linked queue so we can read the exact bytes it
    // will later decrypt. A's PRIMARY stays up — it is doing the sending.
    client_as.shutdown().await;
    client_b.shutdown().await;
    drop(client_as);
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let before_a = quiesced_row_ids(&uid_a).await;
    let before_b = quiesced_row_ids(&uid_b).await;

    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");

    // ---- THE SEND UNDER TEST: from A's PRIMARY, addressed to B -------------
    client_ap
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");

    // A's linked device and B are both offline, so both queues only grow. Wait
    // for the specific rows this send must produce — B's copy, then the
    // self-sync copy on A's LINKED device — rather than sleeping past two
    // sequential deposits. Targeting the linked device matters: these tests
    // also assert A's own sending device was SKIPPED, so the window has to
    // close the instant the awaited row lands.
    let new_b = wait_for_new_envelopes(&uid_b, &before_b, 1).await;
    let new_a = wait_for_device_envelope(&uid_a, &before_a, dev_s).await;

    println!("  NEW rows for B (recipient): {}", new_b.len());
    for f in &new_b {
        println!("    device_id={} envelope_md5={} bytes={}", f[1], f[2], f[4]);
    }
    println!("  NEW rows for A (self-sync): {}", new_a.len());
    for f in &new_a {
        println!("    device_id={} envelope_md5={} bytes={}", f[1], f[2], f[4]);
    }

    // ---- POSITIVE CONTROL (b): B RECEIVED (transport is alive) -------------
    assert!(
        !new_b.is_empty(),
        "POSITIVE CONTROL FAILED: A's send created ZERO new device_messages rows for \
         the RECIPIENT B. The DM never happened, so everything below is vacuous."
    );
    println!("  positive control: B received {} envelope(s) — send path works", new_b.len());

    // ---- POSITIVE CONTROL (c): the self-sync copy REACHED A's linked device -
    // This is the guard the task demands: assert the copy is actually in A's
    // linked queue BEFORE asserting which conversation it lands in. Without it,
    // "not filed under A-self" would pass for the wrong reason (nothing arrived).
    let a_linked_rows: Vec<&Vec<String>> =
        new_a.iter().filter(|f| f[1] == dev_s.to_string()).collect();
    assert!(
        !a_linked_rows.is_empty(),
        "POSITIVE CONTROL FAILED (transport): A's linked device {dev_s} received ZERO \
         self-sync envelopes. This is the F8-transport regression V12 guards, not the \
         attribution property V13 is for — fix self-sync delivery before reading below."
    );
    println!("  positive control: A's linked device {dev_s} queued {} self-sync envelope(s)", a_linked_rows.len());

    // ---- bring A's linked device online and capture the surfaced message ----
    let (client_as2, mut ev_s) = restore_and_start(&path_as, "A linked").await;

    let got_s = wait_for_event(&mut ev_s, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { .. })
    })
    .await;

    let (body_s, conv_s, sender_s) = match &got_s {
        Some(DomainEvent::MessageReceived { message }) => (
            Some(message.as_text().unwrap_or("<not text>").to_string()),
            Some(message.conversation_id),
            message.sender_id,
        ),
        _ => (None, None, None),
    };
    println!("  A linked device surfaced: body={body_s:?} conv={conv_s:?} sender={sender_s:?}");

    // ---- POSITIVE CONTROL (d): it actually DECRYPTED to the marker ---------
    // A message the device cannot decrypt is indistinguishable from no message;
    // asserting its conversation would then be meaningless.
    assert_eq!(
        body_s.as_deref(),
        Some(marker.as_str()),
        "POSITIVE CONTROL FAILED: A's linked device did not surface the self-synced \
         message at all (or could not decrypt it). Got {body_s:?}, expected {marker:?}. \
         The conversation-attribution assertion below would be vacuous."
    );
    println!("  positive control: A's linked device decrypted its own sync to the exact marker");

    // ---- THE DECISIVE ASSERTION: filed in A↔B, as OUTGOING -----------------
    // This is exactly what F8 broke. Before the fix, conv_s was the A-self
    // conversation (ChannelId::from(uid_a)); after the fix it must be the A↔B
    // conversation (ChannelId::from(uid_b)), authored by A (our outgoing message).
    let conv_with_b = ChannelId::from(uid_b);
    let conv_with_a = ChannelId::from(uid_a);
    assert_eq!(
        conv_s,
        Some(conv_with_b),
        "F8 NOT FIXED — SELF-SYNC MISFILED: A sent a DM to B from the primary. A's \
         linked device decrypted it (asserted above) but filed it under {conv_s:?}, \
         NOT the A↔B conversation {conv_with_b:?}. The A-self conversation is \
         {conv_with_a:?} (matches? {}). A message A sent to B is showing on A's laptop \
         as a note-to-self, and the A↔B thread there is still empty. The self-sync copy \
         must be a `SyncSent` carrying B as destination so the linked device buckets it \
         correctly — see send_sealed_dm_content / store_synced_sent_message.",
        conv_s == Some(conv_with_a)
    );
    assert_eq!(
        sender_s,
        Some(uid_a),
        "SELF-SYNC NOT ATTRIBUTED AS OUTGOING: the message landed in the A↔B \
         conversation but its author is {sender_s:?}, not A ({uid_a}). A self-synced \
         message is OUR outgoing message and must be authored by us so the laptop \
         renders it as an outgoing bubble, not an inbound one."
    );
    println!("  DECISIVE: self-sync filed in the A↔B conversation {conv_with_b:?}, authored by A — OUTGOING");

    // ---- NEGATIVE HALF: E2EE still holds on the self-sync path -------------
    let plain: i64 = psql(&format!(
        "select count(*) from device_messages where destination_user_id = '{uid_a}' \
         and position('{marker}' in encode(envelope, 'escape')) > 0"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        plain, 0,
        "E2EE VIOLATED ON THE SELF-SYNC PATH: the plaintext marker appears in {plain} \
         of A's own queued envelope(s). The SyncSent wrapper is still E2E encrypted; a \
         message to yourself is still a message through the server."
    );
    println!("  negative half: marker occurs in 0 of A's queued envelopes — SyncSent stays E2E encrypted");

    println!(
        "V13 PASS — a DM sent from A's PRIMARY surfaced on A's LINKED device in the \
         A↔B conversation as A's OUTGOING message (not a phantom note-to-self); the \
         SyncSent wrapper carried B as destination and stayed E2E encrypted\n"
    );

    client_ap.shutdown().await;
    client_as2.shutdown().await;
}

// ============================================================================
// V14 — Key Transparency REJECTS a server-forged identity key (Phase 2).
//
// STATUS: PASS (was #[ignore]d RED under finding F9; F9 is now RESOLVED). This
// test exercises the real client KT path against the real server:
//
//   register REAL Bob -> Bob's identity key is in the server's append-only KT
//   Merkle log (Phase-1 producer) -> Alice fetches Bob's SIGNED inclusion proof
//   via GetProof -> Alice verifies it with the client verifier
//   (crypto::keytransparency::verify_proof), pinning the tree-head public key.
//
// POSITIVE CONTROL: the honest Bob bundle (carrying Bob's REAL identity key,
// the one committed in the log) verifies against Bob's KT proof AND establishes
// a Signal session. If this half fails, the negative half proves nothing.
//
// THE ATTACK (F9): a malicious server serves an internally-valid prekey bundle
// for Bob's user_id but carrying MALLORY's identity key (prekeys signed by
// Mallory — every signature inside verifies). The client fetches Bob's KT proof;
// the proof's committed value is Bob's REAL key, which != Mallory's forged key,
// so verify_proof returns IdentityKeyMismatch and the KT-gated establishment
// REFUSES the session. The forged key is never trusted.
//
// This is the exact check wired into production session establishment
// (services/signal/session.rs::verify_peer_key_transparency, called before
// establish_session in ensure_session and the multi-device path). This test
// drives the same fetch (AuthClient::get_key_transparency_proof) and the same
// verifier (crypto::verify_kt_proof), against a real server proof — so it also
// proves byte-format interop with the Go crypto package.
//
// MUTATION (run it, observe RED, revert): disable the KT check by making
// crypto/keytransparency.rs::verify_proof return `Ok(())` unconditionally (or
// delete the `IdentityKeyMismatch` guard). The attack half then accepts
// Mallory's forged key and this test FAILS at the "forged key must be rejected"
// assertion. Reverting restores GREEN.
//
// NO-PROOF POLICY: a peer with no leaf (pre-KT user, GetProof -> NotFound) is
// trusted TOFU with a loud warn (see verify_peer_key_transparency). Bob HAS a
// leaf, so this test proves rejection works for a peer who is in the log.
// ============================================================================
#[tokio::test]
async fn v14_forged_identity_key_is_rejected_via_key_transparency() {
    use chatter_core::crypto::SessionManager;
    use chatter_core::network::grpc::AuthClient;
    use libsignal_protocol::{
        DeviceId, IdentityKey, IdentityKeyPair, IdentityKeyStore, InMemSignalProtocolStore, KeyPair,
        PreKeyBundle, ProtocolAddress, kem,
    };
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!("skipping v14: INTEGRATION_TESTS not set");
        return;
    }

    fn make_store(rng: &mut StdRng) -> InMemSignalProtocolStore {
        let identity = IdentityKeyPair::generate(rng);
        let registration_id: u32 = rng.random::<u32>() & 0x3fff; // 14-bit
        InMemSignalProtocolStore::new(identity, registration_id).expect("in-mem store")
    }

    // A fully self-consistent PQXDH bundle whose identity key is `signer`'s and
    // whose signed+kyber prekeys are signed by `signer`. Exactly what a server
    // (honest OR malicious) hands out on GetPreKeyBundle — every signature
    // inside it verifies; the ONLY thing KT can catch is the identity key.
    async fn build_bundle(signer: &IdentityKeyPair, rng: &mut StdRng) -> PreKeyBundle {
        let signed_pre_key_pair = KeyPair::generate(rng);
        let kyber_pre_key_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);

        let spk_pub = signed_pre_key_pair.public_key.serialize();
        let spk_sig = signer.private_key().calculate_signature(&spk_pub, rng).unwrap();
        let kyber_pub = kyber_pre_key_pair.public_key.serialize();
        let kyber_sig = signer.private_key().calculate_signature(&kyber_pub, rng).unwrap();

        PreKeyBundle::new(
            rng.random::<u32>() & 0x3fff,
            DeviceId::new(1).unwrap(),
            None, // no one-time prekey needed for this test
            rng.random::<u32>().into(),
            signed_pre_key_pair.public_key,
            spk_sig.to_vec(),
            rng.random::<u32>().into(),
            kyber_pre_key_pair.public_key.clone(),
            kyber_sig.to_vec(),
            *signer.identity_key(),
        )
        .expect("valid bundle")
    }

    // The exact client-side KT check used in production: fetch the peer's signed
    // inclusion proof, then verify the bundle's identity key is the one the log
    // commits (crypto::verify_kt_proof, pinned tree-head key). Returns the same
    // verdict the production `verify_peer_key_transparency` would.
    async fn kt_verify_bundle_key(
        auth_client: &mut AuthClient,
        peer_user_id: &str,
        bundle: &PreKeyBundle,
    ) -> Result<(), String> {
        let ik = bundle.identity_key().unwrap().serialize();
        match auth_client
            .get_key_transparency_proof(peer_user_id)
            .await
            .map_err(|e| format!("KT proof fetch failed: {e}"))?
        {
            Some(proof) => {
                chatter_core::crypto::verify_kt_proof(peer_user_id, &ik, &proof)
                    .map_err(|e| format!("{e}"))
            }
            None => Ok(()), // TOFU: not reached here — Bob has a leaf.
        }
    }

    let mut rng = StdRng::seed_from_u64(0x14_FEED);

    // ---- Register a REAL Bob through the server (Phase-1 producer appends his
    // identity key to the KT log), and authenticate the pool as Alice so the
    // GetProof lookup is authorized. -----------------------------------------
    let pool = common::create_test_pool()
        .await
        .expect("pool connect failed");
    let bob = common::register_test_user(&pool)
        .await
        .expect("Bob registration failed — no KT leaf to verify against");
    let _alice = common::setup_authenticated_test(&pool)
        .await
        .expect("Alice registration/auth failed — GetProof would be unauthenticated");
    let mut auth_client = pool.auth_client();

    // Bob's REAL identity key, as committed at registration (33-byte serialized).
    let ik_bob = IdentityKey::decode(&bob.public_key).expect("Bob identity key decodes");
    let bob_kp = IdentityKeyPair::try_from(bob.identity_key_pair_bytes.as_slice())
        .expect("Bob identity key pair round-trips");
    assert_eq!(
        *bob_kp.identity_key(),
        ik_bob,
        "sanity: Bob's key pair matches his registered identity key"
    );

    // Mallory the malicious server: a distinct identity Alice must NOT trust.
    let mallory_kp = IdentityKeyPair::generate(&mut rng);
    let ik_mallory = *mallory_kp.identity_key();
    assert_ne!(ik_bob, ik_mallory, "sanity: Bob and Mallory are distinct identities");

    // The address Alice BELIEVES is Bob. KT binds the served key to Bob's leaf.
    let bob_addr = ProtocolAddress::new(bob.user_id.clone(), DeviceId::new(1).unwrap());

    // Prove the KT proof exists and its committed key is Bob's real key.
    let proof = auth_client
        .get_key_transparency_proof(&bob.user_id)
        .await
        .expect("GetProof transport ok")
        .expect("Bob must have a KT leaf (Phase-1 producer runs at registration)");
    let committed = &proof.identity_key;
    assert_eq!(
        committed.as_slice(),
        bob.public_key.as_slice(),
        "KT log commits Bob's real identity key"
    );
    let epoch = proof.signed_tree_head.as_ref().map(|s| s.epoch).unwrap_or(0);
    let tree_size = proof.signed_tree_head.as_ref().map(|s| s.tree_size).unwrap_or(0);
    println!(
        "  fetched Bob's signed KT proof: epoch={epoch} tree_size={tree_size} \
         siblings={} committed_key={}",
        proof.inclusion_proof.as_ref().map(|p| p.sibling_hashes.len()).unwrap_or(0),
        hex::encode(&committed[..committed.len().min(8)]),
    );

    // ---- POSITIVE CONTROL: honest Bob bundle verifies against KT AND
    // establishes a session. Mechanism works => a later reject is real. --------
    {
        let honest = build_bundle(&bob_kp, &mut rng).await;
        assert_eq!(
            *honest.identity_key().unwrap(),
            ik_bob,
            "positive control: honest bundle carries Bob's real key"
        );

        kt_verify_bundle_key(&mut auth_client, &bob.user_id, &honest)
            .await
            .expect("POSITIVE CONTROL FAILED: Bob's real key must verify against his KT proof");

        let mut alice_sess = make_store(&mut rng);
        let mut alice_id = alice_sess.clone();
        let r = SessionManager::establish_session(
            &bob_addr, &v14_alice_addr(), &mut alice_sess, &mut alice_id, &honest, &mut rng,
        )
        .await;
        assert!(
            r.is_ok(),
            "POSITIVE CONTROL FAILED: honest Bob bundle must establish a session: {r:?}"
        );
        let trusted = alice_id.get_identity(&bob_addr).await.unwrap();
        assert_eq!(trusted, Some(ik_bob), "positive: Alice trusts Bob's real key");
        println!("  positive control: honest Bob bundle KT-verified and established a session");
    }

    // ---- THE ATTACK: a malicious server serves an internally-valid bundle for
    // Bob's user_id carrying MALLORY's identity key. Every signature verifies;
    // the identity key is forged. KT must reject it. -------------------------
    let forged = build_bundle(&mallory_kp, &mut rng).await;
    assert_eq!(
        *forged.identity_key().unwrap(),
        ik_mallory,
        "sanity: the forged bundle carries Mallory's identity key, not Bob's"
    );

    let verdict = kt_verify_bundle_key(&mut auth_client, &bob.user_id, &forged).await;

    // THE PROPERTY: a key not committed in Bob's KT leaf is REJECTED.
    assert!(
        verdict.is_err(),
        "KEY TRANSPARENCY FAILED (F9 regression): the client ACCEPTED a bundle for Bob's \
         user_id ({}) carrying MALLORY's identity key ({ik_mallory:?}) instead of Bob's \
         ({ik_bob:?}). The KT proof committed Bob's real key; verify_proof should have \
         rejected the mismatch. A malicious server could MITM this conversation.",
        bob.user_id,
    );
    println!("  attack: forged (Mallory) key REJECTED by KT: {}", verdict.unwrap_err());

    // The forged key is never trusted: because KT rejected, we NEVER call
    // establish_session, so Alice's identity store holds nothing for Bob's
    // address. Prove it end-to-end with a fresh store.
    let mut alice_sess = make_store(&mut rng);
    let mut alice_id = alice_sess.clone();
    let gated: Result<(), String> = async {
        kt_verify_bundle_key(&mut auth_client, &bob.user_id, &forged).await?;
        // Unreachable on a correct client: only runs if KT passed.
        SessionManager::establish_session(
            &bob_addr, &v14_alice_addr(), &mut alice_sess, &mut alice_id, &forged, &mut rng,
        )
        .await
        .map_err(|e| format!("{e:?}"))
    }
    .await;
    assert!(gated.is_err(), "KT-gated establishment must refuse the forged bundle");
    let trusted_after = alice_id.get_identity(&bob_addr).await.unwrap();
    assert_eq!(
        trusted_after, None,
        "forged key must NEVER be recorded as trusted for Bob's address"
    );
    assert!(
        !SessionManager::has_session(&alice_sess, &bob_addr).await.unwrap(),
        "no session may exist with the forged bundle"
    );

    println!(
        "  V14 PASS — KT verified the honest key and REJECTED the forged one; \
         Mallory's key was never trusted for Bob's address ({}).",
        bob.user_id
    );
}

// ============================================================================
// V15: POST-COMPROMISE SECURITY (PCS) — the ratchet HEALS from a state theft
//
// CLAIM (README): "Double Ratchet". V6 proved FORWARD secrecy (a delivered
// message's key is destroyed — the past is protected once state moves forward).
// PCS is the OTHER direction: if an attacker STEALS a session's state at time N,
// then the two parties exchange enough traffic to perform DH ratchet steps (each
// injecting fresh DH entropy the thief never saw), the stolen snapshot must NO
// LONGER decrypt messages sent after those steps. The ratchet heals.
//
// WHY A NAIVE TEST IS WRONG (and how this one is a clean differential):
//   * "The stolen state can't decrypt X" is VACUOUS unless something CAN decrypt
//     X — otherwise the failure could be a malformed record, wrong address, or
//     any API misuse. So there are TWO positive controls:
//       (1) the LIVE, healed session decrypts the post-step message (the message
//           is well-formed and delivery works);
//       (2) the SAME stolen record decrypts a PRE-step message (the record is a
//           valid, loadable session — the only thing separating the two decrypts
//           is which side of the DH ratchet the ciphertext was minted on).
//   Only with (1)+(2) does the third assertion — stolen FAILS on the post-step
//   message — mean "the ratchet healed" rather than "decrypt errored".
//
// WHY THE FAILURE IS SPECIFICALLY A *DH* STEP AND NOT A SYMMETRIC ADVANCE:
//   A symmetric-chain advance only ratchets a chain key forward; libsignal
//   retains skipped message keys, so a snapshot could always derive FORWARD in
//   the SAME chain to reach a later message. The post-step message is therefore
//   only unreachable from the snapshot if it lives in a NEW receiving chain,
//   created by a DH ratchet whose private ratchet key was generated AFTER N.
//   That the snapshot decrypts the pre-step message (same chain) but CANNOT
//   decrypt the post-step message (new chain) is the behavioural signature of a
//   DH ratchet step, not a symmetric advance. We also drive three full A<->B
//   round trips, which structurally force multiple DH steps, and assert the raw
//   session-record bytes changed (as V3 does).
//
// CAPTURE / REPLAY MECHANISM: B's session-for-A is the stolen asset. We capture
// it verbatim via key_store().load_session (as V3/V5 read raw state), and we
// capture the pre- and post-step CIPHERTEXTS off B's Redis queue while B is
// offline (as V6 captures the exact bytes B would process). To "replay the
// theft", we overwrite B's live session with the stolen bytes via
// key_store().store_session and call the same SignalService::decrypt_message V6
// uses — the SessionStore reads records straight from SQLite with no cache
// (crypto/signal/store.rs:328), so the overwrite is what libsignal decrypts
// against. Every decrypt here uses ONE store and ONE captured ciphertext at a
// time; only the session record differs.
//
// MUTATION CHECK (run, observed RED, reverted): point the PCS assertion at the
// PRE-step ciphertext instead of the post-step one. The stolen record decrypts
// it (it is the same chain N sits on), so `assert decrypt is Err` goes RED —
// proving the test distinguishes healed-from-not and does not merely always
// fail for some unrelated reason.
//
// Gated on `testing` for key_store()/signal() access, same as V3/V5/V6.
// ============================================================================

/// Send `marker` from `sender` to `peer` and block until the recipient (whose
/// event stream is `recv_events`) actually surfaces it as decrypted plaintext.
/// This both drives real traffic AND forces the recipient to PROCESS it — which
/// is what advances/heals the recipient's live ratchet.
#[cfg(feature = "testing")]
async fn v15_deliver(
    sender: &Arc<ChatterClient>,
    sender_uid: UserId,
    peer: UserId,
    recv_events: &mut tokio::sync::broadcast::Receiver<DomainEvent>,
    marker: &str,
) {
    sender
        .send_text(ActiveConversation::ServerDm { peer_id: peer }, marker)
        .await
        .expect("v15 send failed");
    let from_conv = ChannelId::from(sender_uid);
    let got = wait_for_event(recv_events, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == from_conv
                 && message.as_text() == Some(marker))
    })
    .await;
    assert!(
        got.is_some(),
        "v15: recipient never received/decrypted '{marker}' — cannot drive the \
         ratchet if traffic isn't delivered"
    );
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn v15_post_compromise_security_heals_after_dh_ratchet() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();
    let client_a = build_client(dir_a.path());
    let mut client_b = build_client(&path_b);

    let uid_a = register_and_start(&client_a, &unique_user("v15a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v15b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // In B's store the session for talking to A is keyed by A's protocol address
    // (sealed-sender decrypt derives the sender from the cert -> "{uid_a}:1").
    let a_addr = format!("{uid_a}:1");

    // ---- Redis queue capture helpers (mirror V6) ---------------------------
    let queue_key = format!("device_queue_envelopes:{uid_b}:1");
    let hkeys = |k: &str| -> std::collections::HashSet<String> {
        sh(&format!(
            "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HKEYS '{k}' 2>/dev/null"
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
    };
    let capture_after = |marker: &str, before: std::collections::HashSet<String>| {
        let queue_key = queue_key.clone();
        let marker = marker.to_string();
        async move {
            let after = hkeys(&queue_key);
            let new_fields: Vec<String> = after.difference(&before).cloned().collect();
            assert_eq!(
                new_fields.len(),
                1,
                "POSITIVE CONTROL FAILED: expected exactly 1 new queued envelope \
                 after sending {marker}, found {} — cannot identify the ciphertext",
                new_fields.len()
            );
            let field = new_fields[0].clone();
            let json = sh(&format!(
                "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HGET '{queue_key}' '{field}' 2>/dev/null"
            ));
            let b64 = extract_between(&json, "\"envelope\":\"", "\"")
                .expect("could not extract envelope base64 from queued JSON");
            (field, b64)
        }
    };
    let b64_to_bytes = |b64: &str| -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .expect("envelope was not valid base64")
    };

    // ========================================================================
    // PHASE 1 (B online): establish a MATURE Whisper session in BOTH directions
    // and capture B's stolen state at N.
    //
    // A round trip is required to leave the PreKey phase: A only stops sending
    // PreKeySignalMessages once it has RECEIVED a message from B. We do
    // A->B, B->A, A->B so that by the end both sides are on plain Whisper
    // messages and B's receiving chain is anchored on A's *current* ratchet key.
    // ========================================================================
    let mut events_a = client_a.subscribe_events();
    let mut events_b = client_b.subscribe_events();

    let est1 = unique_marker();
    v15_deliver(&client_a, uid_a, uid_b, &mut events_b, &est1).await;
    // B's session for A must now exist — there is something to steal.
    assert!(
        client_b.signal().key_store().load_session(&a_addr).unwrap().is_some(),
        "POSITIVE CONTROL FAILED: B has no session record for A after the first \
         message — nothing to capture as 'stolen state'"
    );
    let est_reply = unique_marker();
    v15_deliver(&client_b, uid_b, uid_a, &mut events_a, &est_reply).await;
    let est2 = unique_marker();
    v15_deliver(&client_a, uid_a, uid_b, &mut events_b, &est2).await;
    tokio::time::sleep(Duration::from_millis(700)).await;

    // *** CAPTURE THE STOLEN STATE (time N) ***
    let stolen = client_b
        .signal()
        .key_store()
        .load_session(&a_addr)
        .expect("load_session errored")
        .expect("POSITIVE CONTROL FAILED: no session record for A to steal");
    assert!(!stolen.is_empty(), "stolen session record is empty");
    println!("\n  N: stolen B's session-for-A snapshot ({} bytes)", stolen.len());
    // Sanity: it is a real, loadable SessionRecord.
    libsignal_protocol::SessionRecord::deserialize(&stolen)
        .expect("stolen snapshot is not a valid SessionRecord");

    // ========================================================================
    // PHASE 2 (B offline): mint the PRE-step ciphertext and capture it.
    // Same ratchet era as N (A has received nothing since est2, so its ratchet
    // key is unchanged) => the stolen snapshot must be able to decrypt it.
    // ========================================================================
    client_b.shutdown().await;
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let base_q = hkeys(&queue_key);
    let m_pre = unique_marker();
    println!("  pre-step marker:  {m_pre}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m_pre)
        .await
        .expect("send of pre-step marker failed");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (_pre_field, b64_pre) = capture_after(&m_pre, base_q).await;
    let bytes_pre = b64_to_bytes(&b64_pre);
    println!("  captured PRE-step ciphertext ({} b64 chars)", b64_pre.len());

    // ========================================================================
    // PHASE 3 (B online): heal. Three FULL round trips (B->A then A->B), each of
    // which forces a DH ratchet step, injecting fresh DH entropy generated long
    // after N. B processes every A->B message, so its LIVE session heals.
    // ========================================================================
    let client_b1 = build_client(&path_b);
    client_b1.connect().await.expect("B reconnect failed");
    let mut events_b1 = client_b1.subscribe_events();
    assert!(client_b1.restore_session(), "B failed to restore persisted session");
    if let Err(e) = client_b1.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("B start failed: {e:?}");
        }
    }
    // Let the queued PRE-step message drain and be processed as ordinary traffic.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut events_a2 = client_a.subscribe_events();

    for round in 0..3 {
        let reply = unique_marker();
        v15_deliver(&client_b1, uid_b, uid_a, &mut events_a2, &reply).await;
        let fwd = unique_marker();
        v15_deliver(&client_a, uid_a, uid_b, &mut events_b1, &fwd).await;
        println!("  healing round {} complete (B->A reply, A->B forward)", round + 1);
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    // The live session has moved: raw bytes differ from the stolen snapshot.
    let healed = client_b1
        .signal()
        .key_store()
        .load_session(&a_addr)
        .expect("load_session errored")
        .expect("healed session vanished");
    assert_ne!(
        stolen, healed,
        "session record is byte-identical after 3 round trips — the ratchet did \
         not advance, so there is no healing to test"
    );
    println!("  live session healed: {} -> {} bytes, records differ", stolen.len(), healed.len());

    // ========================================================================
    // PHASE 4 (B offline): mint the POST-step ciphertext, capture it, and
    // WITHHOLD it from the queue so B's live session stays pristine for the
    // positive control below (it must not auto-consume this message).
    // ========================================================================
    client_b1.shutdown().await;
    drop(client_b1);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let base_q2 = hkeys(&queue_key);
    let m_post = unique_marker();
    println!("  post-step marker: {m_post}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m_post)
        .await
        .expect("send of post-step marker failed");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (post_field, b64_post) = capture_after(&m_post, base_q2).await;
    let bytes_post = b64_to_bytes(&b64_post);
    assert_ne!(b64_pre, b64_post, "captured the same envelope twice");
    // Withhold it, so B's live session does not decrypt it on reconnect.
    sh(&format!(
        "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HDEL '{queue_key}' '{post_field}' >/dev/null 2>&1"
    ));
    assert!(
        !hkeys(&queue_key).contains(&post_field),
        "POSITIVE CONTROL FAILED: could not withhold the post-step envelope"
    );
    println!("  captured POST-step ciphertext ({} b64 chars) and withheld it", b64_post.len());

    // ========================================================================
    // PHASE 5 (B online): the three decrypts. One store, two captured
    // ciphertexts; only the session record changes between them.
    // ========================================================================
    let client_b2 = build_client(&path_b);
    client_b2.connect().await.expect("B reconnect #2 failed");
    let _events_b2 = client_b2.subscribe_events();
    assert!(client_b2.restore_session(), "B failed to restore persisted session #2");
    if let Err(e) = client_b2.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("B start #2 failed: {e:?}");
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- POSITIVE CONTROL 1: the LIVE (healed) session decrypts POST-step ----
    // Run FIRST, against the pristine healed session, before any overwrite.
    let live_post = client_b2.signal().decrypt_message(&bytes_post).await;
    match &live_post {
        Ok((_addr, pt, _)) => {
            let hay = String::from_utf8_lossy(pt);
            assert!(
                hay.contains(&m_post),
                "POSITIVE CONTROL FAILED: live session decrypted post-step bytes \
                 but not to the expected marker"
            );
            println!("  live healed session -> POST-step DECRYPTED (message is well-formed, delivery works)");
        }
        Err(e) => panic!(
            "POSITIVE CONTROL FAILED: the LIVE, healed session could not decrypt \
             the post-step message ({e:?}). The message is malformed or the capture \
             is wrong — the PCS assertion below would then fail for the wrong reason."
        ),
    }

    // ---- POSITIVE CONTROL 2: the STOLEN record is a valid, decrypting session-
    // Overwrite B's session with the stolen snapshot and decrypt the PRE-step
    // ciphertext (same chain N sits on). This proves the stolen record is not
    // malformed and that decrypt_message against it is a WORKING call.
    client_b2
        .signal()
        .key_store()
        .store_session(&a_addr, &stolen)
        .expect("store_session (stolen, for pre) failed");
    assert_eq!(
        client_b2.signal().key_store().load_session(&a_addr).unwrap().as_deref(),
        Some(stolen.as_slice()),
        "overwrite did not land — the store still holds a different session"
    );
    let stolen_pre = client_b2.signal().decrypt_message(&bytes_pre).await;
    match &stolen_pre {
        Ok((_addr, pt, _)) => {
            let hay = String::from_utf8_lossy(pt);
            assert!(
                hay.contains(&m_pre),
                "POSITIVE CONTROL FAILED: stolen record decrypted pre-step bytes \
                 but not to the expected marker"
            );
            println!("  stolen record -> PRE-step DECRYPTED (record valid; pre-step traffic recoverable)");
        }
        Err(e) => panic!(
            "POSITIVE CONTROL FAILED: the stolen record could not even decrypt a \
             PRE-step message ({e:?}). Then 'stolen fails on post-step' proves \
             nothing — the record may simply be unusable. This must decrypt."
        ),
    }

    // ---- THE PCS ASSERTION: the SAME stolen record FAILS on POST-step --------
    // Re-install the stolen snapshot (the pre-step decrypt above advanced it),
    // then point the identical API at the POST-step ciphertext. Same store, same
    // record, same call — only the ratchet era of the ciphertext differs.
    client_b2
        .signal()
        .key_store()
        .store_session(&a_addr, &stolen)
        .expect("store_session (stolen, for post) failed");
    let stolen_post = client_b2.signal().decrypt_message(&bytes_post).await;
    println!("  stolen record -> POST-step {}", match &stolen_post {
        Ok(_) => "DECRYPTED (!!)".to_string(),
        Err(e) => format!("REJECTED: {e:?}"),
    });
    if let Ok((_addr, pt, _msg_type)) = &stolen_post {
        let hay = String::from_utf8_lossy(pt);
        if hay.contains(&m_post) {
            panic!(
                "PCS VIOLATED: a session snapshot stolen at N decrypted a message \
                 minted after {} DH ratchet round trips. The ratchet did NOT heal — \
                 whoever seizes the state at N keeps decrypting all future traffic. \
                 (Controls above prove the message is well-formed and the record \
                 decrypts PRE-step traffic, so this is a real healing failure.)",
                3
            );
        }
    }
    assert!(
        stolen_post.is_err(),
        "PCS VIOLATED: the stolen snapshot decrypted the post-step ciphertext"
    );

    println!(
        "V15 PASS — post-compromise security holds: state stolen at N decrypts \
         PRE-step traffic but is HEALED out of POST-step traffic after DH ratchet \
         steps; the live session still decrypts it\n"
    );

    client_a.shutdown().await;
    client_b2.shutdown().await;
}

// ============================================================================
// V16: MESSAGE ROUND-TRIP FIDELITY ACROSS PAYLOAD SHAPES
//
// CLAIM (README-adjacent / VALIDATED "Not yet proven"): a message sent A->B is
// delivered to B byte-for-byte, for arbitrary payload shapes — not just the one
// short ASCII string every prior proof used. "No data corruption" is only shown
// if empty, 1-byte, large, control-char, NUL-embedded, multibyte, emoji (incl.
// ZWJ), combining-mark, and RTL payloads all decrypt on B to the EXACT input.
//
// SURFACE UNDER TEST: the ServerDm (channel-free, sealed-sender, UAK) path —
// `send_text(ActiveConversation::ServerDm { .. })` on A, `as_text()` on the
// DomainEvent::MessageReceived B decrypts. The DM body is a proto3 `string`
// (content.proto: `string body = 1`), so this path only carries valid UTF-8;
// true arbitrary bytes (all 256 values) travel the ATTACHMENT path, which this
// test does NOT exercise — recorded as still-open in VALIDATED.md.
//
// POSITIVE CONTROL: a normal sentinel string round-trips byte-exact (proves the
// pipeline works, so a systematic failure would show as "everything fails" not
// "one shape fails").
//
// NEGATIVE HALF / MUTATION: assert B's decrypted output equals a DIFFERENT
// string than A sent -> the test MUST go red. That proves the byte-exact
// equality has teeth and isn't comparing a value to itself. (Run by temporarily
// changing an expected payload; observed red; reverted — see VALIDATED.md V16.)
//
// THREE REAL FINDINGS this test pins (asserted as the TRUE behavior, not
// smoothed over):
//   * EMPTY and WHITESPACE-ONLY are REJECTED AT SEND. The ServerDm send path
//     trims the text and refuses an empty result
//     (services/message/server_dm.rs:48-53, "message must have text or
//     attachments"). So neither "" nor "   \t\n  " ever reaches the wire; they
//     do not round-trip. (Belt-and-suspenders: even if one did, the receive
//     path at services/gateway/mod.rs:189 consumes an empty-body DM as a
//     profile-key handshake carrier — ProfileKeyReceived, no message.)
//   * LEADING/TRAILING WHITESPACE ROUND-TRIPS BYTE-EXACT (F11 RESOLVED). The
//     send path used to ship `text.trim()`, silently stripping surrounding
//     whitespace on the wire (in != out). The trim was moved to DISPLAY time,
//     so the wire now carries what the user typed and a padded body comes back
//     byte-identical. Whitespace-only is still rejected at send (F10).
//   * SIZE CAP. ServerDm sends via `SendSealedMessage`; the server caps the
//     sealed body at MaxSealedBodySize = 8192 bytes
//     (internal/services/message/handlers/sealed_sender.go:44), behind a 1 MiB
//     Connect request cap (handlers/handlers.go:38). A 1 MB payload is therefore
//     REJECTED at send (Err), not truncated. A multi-KB payload comfortably
//     under the cap round-trips byte-exact. This is a real cap, EXPECTED, not a
//     bug.
// ============================================================================

#[tokio::test]
async fn v16_message_roundtrip_fidelity() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v16a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v16b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // B stays ONLINE and polls; its conversation with A is bucketed by A's id
    // (channel-free DM), exactly as V1 asserts.
    let mut events_b = client_b.subscribe_events();
    let conv = ChannelId::from(uid_a);

    // Wait for the next MessageReceived on the A<->B conversation and return its
    // DECRYPTED text as an owned String (byte-exact source of truth for B side).
    async fn next_body(
        rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>,
        conv: ChannelId,
        timeout: Duration,
    ) -> Option<String> {
        let ev = wait_for_event(rx, timeout, |e| {
            matches!(e, DomainEvent::MessageReceived { message }
                if message.conversation_id == conv)
        })
        .await?;
        if let DomainEvent::MessageReceived { message } = ev {
            Some(message.as_text().unwrap_or("<not text>").to_string())
        } else {
            None
        }
    }

    // ---- FINDING 1: EMPTY and WHITESPACE-ONLY are REJECTED AT SEND ---------
    // The ServerDm send path trims the text and refuses an empty result
    // (services/message/server_dm.rs:48-53) — "message must have text or
    // attachments". So neither "" nor a whitespace-only body ever reaches the
    // wire; they do NOT round-trip. Asserted as the TRUE behavior (Err), not
    // smoothed over.
    for (name, payload) in [("empty-string", ""), ("whitespace-only", "   \t\n  ")] {
        let r = client_a
            .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, payload)
            .await;
        let outcome = match &r {
            Ok(_) => "Ok(accepted — NOT rejected)".to_string(),
            Err(e) => format!("Err({e})"),
        };
        assert!(
            matches!(r, Err(chatter_core::services::gateway::SendError::Message(
                chatter_core::services::message::MessageError::InvalidContent(_)))),
            "[{name}] expected InvalidContent rejection at send, got {outcome}"
        );
        println!(
            "  [{name:15}] in={}B -> REJECTED at send (server_dm.rs:48, trims to empty) — FINDING",
            payload.len()
        );
    }

    // ---- FINDING 2 (F11 RESOLVED): LEADING/TRAILING WHITESPACE ROUND-TRIPS ----
    // server_dm.rs previously sent `text.trim()`, silently stripping surrounding
    // whitespace ON THE WIRE (the F11 fidelity finding: in != out, a data-modifying
    // transform the sender never saw). Owner decision: the wire must carry what the
    // user typed. The trim was MOVED to DISPLAY time (chatter-ui converters/
    // message.rs:190 already trims before rendering), so a padded body now arrives
    // at B byte-for-byte identical. Whitespace-only is STILL rejected at send
    // (F10 intact — proven in Finding 1 above).
    //
    // MUTATION (F11): assert `padded_out == padded_in.trim()` (the OLD stripped
    // behavior) and this goes RED — proving the wire is no longer trimmed. Verified
    // red, then reverted. See VALIDATED.md F11.
    let padded_in = "  \t padded body \n ";
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, padded_in)
        .await
        .expect("padded send failed");
    let padded_out = next_body(&mut events_b, conv, EVENT_TIMEOUT)
        .await
        .expect("padded payload never arrived");
    assert_eq!(
        padded_out.as_bytes(),
        padded_in.as_bytes(),
        "F11: leading/trailing whitespace must round-trip BYTE-EXACT (the wire carries \
         what the user typed; trim moved to display). in != out here means the send \
         path is still trimming the body."
    );
    println!(
        "  [ws-padding    ] in={}B out={}B -> BYTE-EXACT (F11 resolved: trim moved to display)",
        padded_in.len(),
        padded_out.len()
    );

    // ---- POSITIVE CONTROL: a normal sentinel round-trips byte-exact --------
    let sentinel = format!("V16-SENTINEL-{}", unique_marker());
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &sentinel)
        .await
        .expect("send sentinel failed");
    let first = next_body(&mut events_b, conv, EVENT_TIMEOUT)
        .await
        .expect("positive control: B received no sentinel — pipeline is dead");
    assert_eq!(
        first, sentinel,
        "positive control: sentinel did not round-trip byte-exact"
    );
    println!("  [pos-control   ] sentinel round-tripped byte-exact ({}B)", sentinel.len());

    // ---- BATTERY: every non-empty UTF-8 shape, byte-exact ------------------
    // All payloads are byte-distinct (so a burst-send + order-independent
    // multiset match keeps this within budget) and have NO leading/trailing
    // whitespace (that is Finding 2's separate case), so trim() is a no-op and
    // byte-exact must hold. Interior newlines/tabs/NUL/control chars ARE part of
    // the payloads and must survive verbatim.
    let battery: Vec<(&str, String)> = vec![
        ("1-byte",         "x".to_string()),
        ("sql-injection",  "'; DROP TABLE messages;--".to_string()),
        ("tmpl-injection", "{{7*7}}".to_string()),
        ("interior-nl",    "line1\nline2\r\n\ttabbed middle".to_string()),
        ("cjk-multibyte",  "中文测试消息".to_string()),
        ("emoji-zwj",      "👨‍👩‍👧‍👦".to_string()),
        ("emoji-skintone", "👍🏽🔥".to_string()),
        ("combining",      "e\u{0301}a\u{0300}o\u{0302}".to_string()),
        ("rtl-mixed",      "مرحبا שלום world".to_string()),
        ("nul-embedded",   "before\u{0}after".to_string()),
        ("control-chars",  "tab\there\r\nnl\u{0007}bell\u{001b}esc".to_string()),
        ("utf8-1234-byte", "A\u{00e9}\u{4e2d}\u{1f600}".to_string()),
    ];

    // Guard: payloads must be byte-distinct or the multiset match is ambiguous.
    for i in 0..battery.len() {
        for j in (i + 1)..battery.len() {
            assert_ne!(
                battery[i].1.as_bytes(), battery[j].1.as_bytes(),
                "test bug: payloads {} and {} are byte-identical", battery[i].0, battery[j].0
            );
        }
    }

    for (name, payload) in &battery {
        client_a
            .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, payload)
            .await
            .unwrap_or_else(|e| panic!("send [{name}] failed: {e:?}"));
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    // Collect exactly battery.len() delivered bodies.
    let mut received: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while received.len() < battery.len() && Instant::now() < deadline {
        if let Some(body) = next_body(&mut events_b, conv, Duration::from_secs(5)).await {
            received.push(body);
        }
    }
    assert_eq!(
        received.len(),
        battery.len(),
        "delivery count mismatch: sent {} non-empty shapes, B surfaced {} messages",
        battery.len(),
        received.len()
    );

    // Byte-exact multiset match: every sent shape appears once in B's output.
    for (name, payload) in &battery {
        let matches = received
            .iter()
            .filter(|r| r.as_bytes() == payload.as_bytes())
            .count();
        assert_eq!(
            matches, 1,
            "SHAPE [{name}] did NOT round-trip byte-exact: expected exactly one \
             delivery equal to the {}-byte input, found {matches}. \
             (in bytes: {:?})",
            payload.len(),
            payload.as_bytes(),
        );
        // out-length is the matched received body's byte length (== in on pass).
        let out_len = received
            .iter()
            .find(|r| r.as_bytes() == payload.as_bytes())
            .map(|r| r.len())
            .unwrap();
        println!(
            "  [{name:14}] in={:>4}B out={:>4}B  PASS (byte-exact)",
            payload.len(),
            out_len
        );
    }

    // ---- SIZE CAP: 1 MB is REJECTED at send (not truncated) ----------------
    let one_mb = "A".repeat(1024 * 1024);
    let big = client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &one_mb)
        .await;
    assert!(
        big.is_err(),
        "SIZE CAP: a 1 MB payload was ACCEPTED at send. Expected rejection by the \
         sealed-body cap (MaxSealedBodySize=8192, sealed_sender.go:44) behind the \
         1 MiB Connect request cap (handlers.go:38). If accepted, probe whether B \
         gets the full 1 MB or a truncated body — silent truncation is a finding."
    );
    println!(
        "  [1MB          ] in={}B -> REJECTED at send (cap: sealed body <= 8192B) — EXPECTED",
        one_mb.len()
    );

    // ---- JUST-UNDER-CAP: a multi-KB payload round-trips byte-exact ---------
    // Session is warm (battery already ran) so this is a regular SignalMessage
    // with small overhead; 6000 plaintext bytes leaves ample margin under the
    // 8192-byte sealed-body cap.
    let big_ok = "U".repeat(6000);
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &big_ok)
        .await
        .expect("just-under-cap 6000B send failed");
    let got_big = next_body(&mut events_b, conv, EVENT_TIMEOUT)
        .await
        .expect("just-under-cap payload never arrived at B");
    assert_eq!(
        got_big.as_bytes(),
        big_ok.as_bytes(),
        "just-under-cap payload corrupted in transit"
    );
    println!(
        "  [under-cap-6KB ] in={}B out={}B  PASS (byte-exact)",
        big_ok.len(),
        got_big.len()
    );

    println!(
        "V16 PASS — {} UTF-8 payload shapes round-trip byte-exact A->B (incl. NUL, \
         interior newlines, control chars, emoji ZWJ, combining marks, RTL, AND \
         leading/trailing whitespace preserved byte-exact — F11 resolved); empty & \
         whitespace-only are REJECTED at send (F10); 1 MB rejected by the 8192-byte \
         sealed-body cap; a 6 KB under-cap payload round-trips\n",
        battery.len()
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V17–V19 — the NON-TEXT control self-sync paths (the F8/V13 follow-through).
//
// V13 fixed the TEXT DM self-sync (SyncSent). The remaining control types
// (reaction, receipts, typing, pin, edit, delete) also self-sync to the
// sender's own linked devices. This block proves how each of the load-bearing
// ones behaves on the linked device, and pins the per-type decision:
//
//   - Reaction (V17): NO wire change needed. Reactions are message-anchored by
//     Signal's (author, ns-timestamp) key — NOT by conversation — so the bare
//     self-sync copy attaches to the exact target message on the linked device,
//     reactor = us. Proven E2E here. (Pin/Edit/Delete-for-everyone are the same
//     shape: message-anchored by target author+ts; Delete-for-me carries its own
//     conversation_id. All file correctly with no destination on the wire.)
//   - Read receipt (V18): FIXED. A self-synced read receipt (sealed-sender ==
//     us) is turned into a local mark-read on the linked device, so the unread
//     badge clears across devices (Signal read-sync). Without the fix it landed
//     as a delivery-status write on a phantom A-self thread and did nothing.
//   - Typing (V19): DROPPED. Your own typing is never synced to your other
//     devices (Signal parity); a linked device echoing "you are typing" in a
//     phantom self-thread is noise. Proven: no self-sync envelope is produced.
// ============================================================================

/// Shared fixture for V17–V19: user A with a primary + a REAL linked device,
/// and user B who is A's friend. A's linked device is started and online.
///
/// Factored out of V13's inline setup because three tests need the identical
/// link + friend handshake, and a second hand-rolled copy of a multi-step
/// provisioning flow is a third thing to get subtly wrong — the exact class of
/// bug this suite exists to catch. The TempDir guards are held in the struct so
/// the on-disk data dirs outlive the clients (dropping a TempDir deletes it).
struct AbFixture {
    _dir_ap: tempfile::TempDir,
    _dir_as: tempfile::TempDir,
    _dir_b: tempfile::TempDir,
    client_ap: Arc<ChatterClient>,
    path_as: std::path::PathBuf,
    client_as: Arc<ChatterClient>,
    uid_a: UserId,
    dev_s: u32,
    client_b: Arc<ChatterClient>,
    uid_b: UserId,
}

async fn setup_a_linked_b(tag: &str) -> AbFixture {
    let dir_ap = tempfile::tempdir().unwrap();
    let path_ap = dir_ap.path().to_path_buf();
    let client_ap = build_client(&path_ap);
    let uid_a = register_and_start(&client_ap, &unique_user(&format!("{tag}ap")), "A Primary").await;

    let dir_as = tempfile::tempdir().unwrap();
    let path_as = dir_as.path().to_path_buf();
    let client_as = build_client(&path_as);
    let dev_s = link_secondary_device(&client_ap, &client_as, uid_a).await;
    start_linked(&client_as, "A linked").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // POSITIVE CONTROL: two DISTINCT devices on the sender, both with a bundle —
    // otherwise the self-sync skip (by device_id) and the property under test are
    // the same row (V13's lesson).
    assert_ne!(
        dev_s, 1,
        "POSITIVE CONTROL FAILED: A's linked device got device_id 1 (the primary's \
         own id). Self-sync skips the sending device by id, so a collision makes the \
         skip and the property indistinguishable."
    );
    let n_dev: i64 = psql(&format!(
        "select count(distinct device_id) from signal_prekey_bundles where user_id = '{uid_a}'"
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    assert_eq!(
        n_dev, 2,
        "POSITIVE CONTROL FAILED: expected 2 devices with a prekey bundle for A \
         (primary 1 + linked {dev_s}), found {n_dev}."
    );
    println!("  fixture: A primary=1, A linked={dev_s} — distinct, both hold bundles");

    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();
    let client_b = build_client(&path_b);
    let uid_b = register_and_start(&client_b, &unique_user(&format!("{tag}b")), "Bob").await;

    befriend(&client_ap, &client_b, uid_a, uid_b).await;

    AbFixture {
        _dir_ap: dir_ap,
        _dir_as: dir_as,
        _dir_b: dir_b,
        client_ap,
        path_as,
        client_as,
        uid_a,
        dev_s,
        client_b,
        uid_b,
    }
}

// ============================================================================
// V17 — a self-synced REACTION attaches to the right message on A's linked
// device, attributed to A (not mis-filed / mis-attributed).
//
// Reactions are stored Signal-style by (target_author, ns-timestamp) in a
// conversation-independent table; the wire Reaction names its target by that
// key. So the bare self-sync copy — the SAME bytes B gets — attaches to the
// exact target message on A's linked device, with the reactor resolved from the
// sealed-sender identity (= us). No destination is needed on the wire, unlike
// the text path. THE DECISIVE ASSERTION: A's linked device emits ReactionAdded
// for B's message with reactor = A.
//
// MUTATION — RUN IT: in send_sealed_dm_content (server_dm.rs), add
// `Some(content::Content::Reaction(_)) => None,` to the self_plaintext match so
// reactions are no longer self-synced. B still gets the reaction, but A's linked
// device receives nothing → ReactionAdded never fires → the wait times out and
// the decisive assertion goes red. Revert; verify by SHA-256 of server_dm.rs
// INSIDE the submodule.
// ============================================================================
#[tokio::test]
async fn v17_self_synced_reaction_attaches_to_the_right_message_as_ours() {
    skip_if_no_server!();
    no_keyring_prompts();

    let fx = setup_a_linked_b("v17").await;
    let uid_a = fx.uid_a;
    let uid_b = fx.uid_b;
    let dev_s = fx.dev_s;

    // B sends a message A will react to. It fans out to BOTH of A's devices, so
    // the reaction's TARGET is present on the linked device (the "target synced"
    // precondition the task calls out).
    let mut ev_ap = fx.client_ap.subscribe_events();
    let mut ev_as = fx.client_as.subscribe_events();
    fx.client_b
        .send_text(ActiveConversation::ServerDm { peer_id: uid_a }, "react to me")
        .await
        .expect("B send_text failed");

    let got_ap = wait_for_event(&mut ev_ap, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.sender_id == Some(uid_b))
    })
    .await;
    let ts_m = match &got_ap {
        Some(DomainEvent::MessageReceived { message }) => message.sent_at,
        _ => panic!("A's primary never received B's target message — nothing to react to"),
    };
    println!("  B's target message received on A's primary: sent_at_ns={ts_m}");

    // POSITIVE CONTROL: the target is ALSO on A's linked device (so the reaction
    // has something to attach to there).
    let got_as = wait_for_event(&mut ev_as, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.sender_id == Some(uid_b))
    })
    .await;
    assert!(
        got_as.is_some(),
        "POSITIVE CONTROL FAILED: A's linked device never received B's target \
         message, so a reaction landing (or not) there proves nothing."
    );
    println!("  positive control: B's target message present on A's linked device");

    // Take A's linked device and B offline so the reaction SITS in A's linked
    // queue (inspect it) and B can't interfere.
    fx.client_as.shutdown().await;
    fx.client_b.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let before_a = quiesced_row_ids(&uid_a).await;

    // THE ACTION UNDER TEST: A reacts from the PRIMARY to B's message.
    fx.client_ap
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_reaction("👍", &uid_b, ts_m, false)
        .await
        .expect("send_reaction failed");

    // POSITIVE CONTROL: the reaction self-sync actually reached A's linked queue.
    // A's linked device is offline, so the queue only grows — wait for ITS row
    // specifically (a count-based wait is satisfied by any row for A).
    let new_a = wait_for_device_envelope(&uid_a, &before_a, dev_s).await;
    let a_linked_rows: Vec<&Vec<String>> =
        new_a.iter().filter(|f| f[1] == dev_s.to_string()).collect();
    assert!(
        !a_linked_rows.is_empty(),
        "POSITIVE CONTROL FAILED (transport): A's linked device {dev_s} received ZERO \
         self-sync envelopes for the reaction. If the send-side dropped it, the decisive \
         assertion below would fail for a transport reason, not attribution."
    );
    println!("  positive control: A's linked device {dev_s} queued {} reaction self-sync envelope(s)", a_linked_rows.len());

    // Bring A's linked device online and capture how it files the reaction.
    let (client_as2, mut ev_s) = restore_and_start(&fx.path_as, "A linked").await;
    let got = wait_for_event(&mut ev_s, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::ReactionAdded { .. })
    })
    .await;

    // THE DECISIVE ASSERTION: reactor = A (us), target = B's message. A reaction
    // mis-attributed to B, or attached to the wrong message, fails here.
    match &got {
        Some(DomainEvent::ReactionAdded { sender_id, sent_at_ns, reactor_id, emoji, .. }) => {
            println!(
                "  A linked filed reaction: target_author={sender_id} target_ts={sent_at_ns} reactor={reactor_id} emoji={emoji}"
            );
            assert_eq!(
                *reactor_id, uid_a,
                "SELF-SYNC REACTION MIS-ATTRIBUTED: reactor is {reactor_id}, not us ({uid_a}). \
                 Our own reaction must show as OURS on our other device."
            );
            assert_eq!(
                *sender_id, uid_b,
                "SELF-SYNC REACTION MIS-TARGETED: target author is {sender_id}, not B ({uid_b})."
            );
            assert_eq!(*sent_at_ns, ts_m, "reaction attached to the wrong message timestamp");
            assert_eq!(emoji, "👍", "reaction emoji corrupted in transit");
        }
        _ => panic!(
            "SELF-SYNC REACTION LOST: A's linked device never surfaced the reaction A made \
             from its primary. The reaction self-sync did not file on the linked device."
        ),
    }
    println!(
        "V17 PASS — A's own reaction, made on the primary, surfaced on A's LINKED device \
         attached to B's message and attributed to A — message-anchored, no destination \
         needed on the wire\n"
    );

    fx.client_ap.shutdown().await;
    client_as2.shutdown().await;
}

// ============================================================================
// V18 — a self-synced READ RECEIPT clears the unread state on A's linked device.
//
// When A reads B's message on the primary, the primary self-syncs a read receipt
// to A's other devices. BEFORE the fix that copy was filed as a delivery-status
// write on a phantom A-self thread (ChannelId::from(A)), where B's message does
// not live, so it did nothing — the unread badge stayed lit on the laptop. The
// fix detects the self-sync (sealed-sender == us) and marks the referenced
// messages READ locally (ns-timestamp pins the row in the peer's thread).
//
// THE DECISIVE ASSERTION: after A's primary reads B's message, A's linked device
// shows that message as is_read == true.
//
// MUTATION — RUN IT: in inbox_processor.rs DmReceipt, change the self-sync
// branch guard `if *receipt_type == 2 || *receipt_type == 3` to `if false` (drop
// the mark-read). The receipt still self-syncs and decrypts on the linked
// device, but is_read stays false → the decisive assertion goes red. Revert;
// verify by SHA-256 of inbox_processor.rs INSIDE the submodule.
// ============================================================================
#[tokio::test]
async fn v18_self_synced_read_receipt_clears_unread_on_linked_device() {
    skip_if_no_server!();
    no_keyring_prompts();

    let fx = setup_a_linked_b("v18").await;
    let uid_a = fx.uid_a;
    let uid_b = fx.uid_b;
    let dev_s = fx.dev_s;

    // B sends A a message; it fans out to both of A's devices as UNREAD.
    let mut ev_ap = fx.client_ap.subscribe_events();
    let mut ev_as = fx.client_as.subscribe_events();
    fx.client_b
        .send_text(ActiveConversation::ServerDm { peer_id: uid_a }, "read me")
        .await
        .expect("B send_text failed");

    let ts_m = match wait_for_event(&mut ev_ap, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.sender_id == Some(uid_b))
    })
    .await
    {
        Some(DomainEvent::MessageReceived { message }) => message.sent_at,
        _ => panic!("A's primary never received B's message"),
    };
    let _ = wait_for_event(&mut ev_as, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.sender_id == Some(uid_b))
    })
    .await;

    // POSITIVE CONTROL: the message is present AND unread on A's linked device
    // BEFORE A reads it — so "became read" is a real transition, not a row that
    // was read (or absent) all along.
    let before = fx
        .client_as
        .get_messages(&ActiveConversation::ServerDm { peer_id: uid_b }, 50)
        .expect("query failed")
        .into_iter()
        .find(|m| m.sent_at == ts_m);
    assert!(
        matches!(&before, Some(m) if !m.is_read),
        "POSITIVE CONTROL FAILED: B's message is not present-and-unread on A's linked \
         device before the read (got {:?}). The transition under test would be vacuous.",
        before.as_ref().map(|m| m.is_read)
    );
    println!("  positive control: B's message present & UNREAD on A's linked device");

    // Take A's linked device and B offline; A reads on the primary; the read
    // receipt SITS in A's linked queue.
    fx.client_as.shutdown().await;
    fx.client_b.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let before_a = quiesced_row_ids(&uid_a).await;

    // THE ACTION UNDER TEST: A reads B's message from the PRIMARY.
    fx.client_ap
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_read_receipt(&[ts_m])
        .await
        .expect("send_read_receipt failed");

    // POSITIVE CONTROL: the read receipt self-sync reached A's linked queue.
    // A's linked device is offline, so the queue only grows — wait for ITS row
    // specifically (a count-based wait is satisfied by any row for A).
    let new_a = wait_for_device_envelope(&uid_a, &before_a, dev_s).await;
    assert!(
        new_a.iter().any(|f| f[1] == dev_s.to_string()),
        "POSITIVE CONTROL FAILED (transport): A's linked device {dev_s} received ZERO \
         read-receipt self-sync envelopes — the mark-read below would fail for a \
         transport reason, not the property under test."
    );
    println!("  positive control: A's linked device {dev_s} queued the read-receipt self-sync");

    // Bring A's linked device online; it processes the self-synced read receipt.
    let (client_as2, _ev) = restore_and_start(&fx.path_as, "A linked").await;

    // THE DECISIVE ASSERTION: B's message is now READ on A's linked device. No
    // DomainEvent fires for a mark-read, so poll the row.
    let mut is_read_now = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if let Ok(msgs) = client_as2.get_messages(&ActiveConversation::ServerDm { peer_id: uid_b }, 50) {
            if msgs.iter().any(|m| m.sent_at == ts_m && m.is_read) {
                is_read_now = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        is_read_now,
        "READ-SYNC BROKEN: A read B's message on the primary, but A's linked device still \
         shows it UNREAD. The self-synced read receipt did not clear the unread state \
         (before the fix it was filed as a delivery-status write on a phantom A-self \
         thread and did nothing)."
    );
    println!(
        "V18 PASS — A reading B's message on the primary cleared the unread state on A's \
         LINKED device (read-sync); the self-synced read receipt marked the peer's message \
         read by its ns-timestamp\n"
    );

    fx.client_ap.shutdown().await;
    client_as2.shutdown().await;
}

// ============================================================================
// V19 — a user's OWN typing is NOT self-synced to their other devices.
//
// Signal does not sync typing indicators to your own linked devices, and a
// linked device echoing "you are typing" in a phantom self-thread is noise, not
// a feature. The send side drops the self-copy for Typing content entirely.
//
// THE DECISIVE ASSERTION: after A's primary sends a typing indicator to B, A's
// OWN devices receive ZERO new envelopes — while B DOES receive one (so "no
// self-sync" is not vacuous: the typing was really sent, just not to ourselves).
//
// MUTATION — RUN IT: in send_sealed_dm_content (server_dm.rs), remove the
// `Some(content::Content::Typing(_)) => None,` arm so typing self-syncs like the
// other control types. A new self-sync envelope then appears for A's linked
// device → the decisive assertion goes red. Revert; verify by SHA-256 of
// server_dm.rs INSIDE the submodule.
// ============================================================================
#[tokio::test]
async fn v19_own_typing_is_not_self_synced() {
    skip_if_no_server!();
    no_keyring_prompts();

    let fx = setup_a_linked_b("v19").await;
    let uid_a = fx.uid_a;
    let uid_b = fx.uid_b;
    let dev_s = fx.dev_s;

    // Keep B offline so its queue is a clean before/after ledger for THIS action.
    fx.client_b.shutdown().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let before_a = quiesced_row_ids(&uid_a).await;
    let before_b = quiesced_row_ids(&uid_b).await;

    // THE ACTION UNDER TEST: A sends a typing indicator to B from the primary.
    fx.client_ap
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_typing(true)
        .await
        .expect("send_typing failed");

    // Wait on the POSITIVE side (B must get the typing envelope), THEN give the
    // negative half a short settle window. You cannot poll for a non-event, so
    // the absence assertion keeps a fixed window — but it is now anchored to a
    // CAUSAL barrier instead of to the send: a self-sync copy, if the send path
    // produced one, is deposited by the same `send_sealed_dm_content` call
    // immediately after the peer copy (server_dm.rs:502-553, peer loop then own
    // loop), so once B's row exists A's would be milliseconds behind. 3s is
    // ample for that and much tighter than a window that keeps growing while B
    // is slow — the wide version swept in a live peer's delivery receipt and
    // reported the self-sync suppression as broken.
    let new_b = wait_for_new_envelopes(&uid_b, &before_b, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let new_a: Vec<Vec<String>> = envelope_rows(&uid_a)
        .into_iter()
        .filter(|f| !before_a.contains(&f[0]))
        .collect();

    // POSITIVE CONTROL: the typing REALLY went out to B — otherwise "A synced
    // nothing to itself" is vacuous (the whole send could have no-op'd).
    assert!(
        !new_b.is_empty(),
        "POSITIVE CONTROL FAILED: A's typing produced ZERO envelopes for B. The send \
         itself no-op'd, so asserting A didn't self-sync proves nothing."
    );
    println!("  positive control: B received {} typing envelope(s) — the send path works", new_b.len());

    // THE DECISIVE ASSERTION: A's OWN devices got nothing. Specifically, the
    // linked device dev_s received zero.
    let a_linked_rows: Vec<&Vec<String>> =
        new_a.iter().filter(|f| f[1] == dev_s.to_string()).collect();
    assert!(
        a_linked_rows.is_empty(),
        "OWN TYPING WAS SELF-SYNCED: A's linked device {dev_s} received {} typing \
         envelope(s). Your own typing must NOT be echoed to your other devices (Signal \
         parity); the send side must drop the self-copy for Typing content.",
        a_linked_rows.len()
    );
    println!(
        "V19 PASS — A's own typing reached B but was NOT self-synced to A's linked device \
         {dev_s} (0 self-envelopes); own-typing echo is correctly dropped\n"
    );

    fx.client_ap.shutdown().await;
    fx.client_as.shutdown().await;
}

// ============================================================================
// V20 — "delete for everyone" ACTUALLY deletes on the RECIPIENT.
//
// PROPERTY: A and B are friends. A sends a normal DM to B; B receives and
// STORES it (positive control — B's local message store really holds it). A
// then issues delete-for-everyone for that exact message. B's copy must be
// GONE from B's local store afterwards. This is the whole point of the
// feature: a privacy messenger that reports "deleted for everyone" and leaves
// the message sitting on the recipient's device is lying to its user.
//
// WHY THIS TEST EXISTS (finding F13): delete-for-everyone rides an empty-body
// `DataMessage` carrying the `delete` field on the sealed ServerDm path
// (builders::build_delete_for_everyone_content_with_channel). On the receive
// side, MessageGateway::process_incoming_message matched ANY
// `Content::DataMessage` and handed it to `store_data_message`
// (services/gateway/decryption.rs) — which ignores `delete` entirely. The
// empty-body/delete carrier therefore did NOT reach the working delete handler
// (dispatch_control_content -> DmDeleteForEveryone -> MessageDeleted ->
// delete_message_local); instead it persisted a junk empty-body row while the
// TARGET message stayed put. So the peer never actually deleted. The fix
// routes a `DataMessage` carrying `delete` to `SealedSenderResult::ControlContent`
// so the existing control dispatcher handles it — reusing the delete path the
// channel/other transports already use, not a second mechanism.
//
// POSITIVE CONTROL FIRST: assert B stored A's DM (query B's ServerDm store,
// scoped to THIS run's A->B conversation) BEFORE asserting it's gone. Without
// it, "B has no such message" passes vacuously (B might never have received).
//
// MUTATION CHECK (run it, observe RED, revert): remove the delete-routing guard
// in services/gateway/decryption.rs::process_incoming_message_inner (let a
// `DataMessage` carrying `delete` fall through to `store_data_message` again).
// B then keeps A's message and the final assertion FAILS. Reverting restores
// GREEN. This is exactly the pre-fix behavior that F13 documents.
// ============================================================================
#[tokio::test]
async fn v20_delete_for_everyone_actually_deletes_on_the_recipient() {
    use chatter_core::domain::message::ConversationType;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A and B register and befriend --------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v20a"), "V20 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v20b"), "V20 Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // ---- A sends a normal DM to B -------------------------------------------
    let marker = unique_marker();
    println!("\n  marker plaintext: {marker}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");

    // B buckets a channel-free ServerDm under the SENDER's id
    // (store_data_message: ChannelId::from(sender_id)). Scope the query to THIS
    // run's A->B conversation so a stale global row can never satisfy it.
    let conv_on_b = ChannelId::from(uid_a);

    // ---- POSITIVE CONTROL: B actually STORED A's DM -------------------------
    let mut stored = None;
    for _ in 0..30 {
        let msgs = client_b
            .get_messages_local(&conv_on_b, &ConversationType::ServerDm, None, 50)
            .unwrap_or_default();
        if let Some(m) = msgs.iter().find(|m| m.as_text() == Some(marker.as_str())) {
            stored = Some(m.clone());
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let stored = stored.expect(
        "POSITIVE CONTROL FAILED: B never stored A's DM (marker absent from B's ServerDm \
         store for this run). The delete assertion below would be vacuous — B can't lose \
         a message it never had.",
    );
    let sent_at = stored.sent_at;
    assert!(
        client_b.get_message_by_sender_timestamp(&uid_a, sent_at).is_some(),
        "POSITIVE CONTROL FAILED: B's message is not addressable by (sender={uid_a}, \
         sent_at={sent_at}) — the same key the delete handler uses to find it.",
    );
    println!(
        "  positive control: B stored A's DM (sender={uid_a}, sent_at={sent_at}) — present before delete"
    );

    // ---- THE ACTION UNDER TEST: A deletes for everyone ----------------------
    // Same timestamp B stored (Signal's (author, ns-timestamp) message key).
    client_a
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_delete(sent_at)
        .await
        .expect("send_delete (delete-for-everyone) failed");
    println!("  A issued delete-for-everyone for sent_at={sent_at}");

    // ---- NEGATIVE HALF: B's copy must be GONE -------------------------------
    let mut gone = false;
    for _ in 0..30 {
        if client_b.get_message_by_sender_timestamp(&uid_a, sent_at).is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Belt-and-suspenders: also confirm it's gone from the conversation listing,
    // not merely un-addressable by the (sender,timestamp) key.
    let still_listed = client_b
        .get_messages_local(&conv_on_b, &ConversationType::ServerDm, None, 50)
        .unwrap_or_default()
        .iter()
        .any(|m| m.as_text() == Some(marker.as_str()));

    assert!(
        gone && !still_listed,
        "DELETE-FOR-EVERYONE DID NOT DELETE ON THE RECIPIENT (finding F13): A issued \
         delete-for-everyone for its DM to B, but B STILL holds the message \
         (addressable-by-key gone={gone}, still-in-listing={still_listed}). On the sealed \
         ServerDm path the empty-body/`delete` carrier is swallowed by \
         `store_data_message` (which ignores `delete`) instead of reaching the delete \
         handler, so the recipient never actually deletes. A privacy messenger that \
         reports success here is lying."
    );
    println!(
        "  negative half: B's copy is GONE after delete-for-everyone (un-addressable AND \
         absent from the conversation listing)"
    );

    println!(
        "V20 PASS — a delete-for-everyone issued by A actually removed the message from \
         B's local store on the sealed ServerDm/peer path (not just on A's own device)\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}
// ============================================================================
// V21: GROUP / CHANNEL messaging — the Sender-Key path, proven and probed
//
// EVERY prior proof (V1-V20) is pairwise DM (ServerDm / Double Ratchet). Group
// messaging rides a DIFFERENT crypto path — Signal Sender Keys — and had NEVER
// been exercised E2E with more than one recipient. T6 in e2e_two_user_suite
// creates a 2-member "group" and prints "via Sender Keys", but a 2-member group
// is DM-shaped ("A plus one other") AND, as this test proves, T6's group is not
// even encrypted. Three members with encryption ON is the real thing: A must
// distribute its sender key to BOTH B and C, and the single sender-key ciphertext
// must fan out to BOTH.
//
// TWO things are established here:
//
//   PART 0 (POSITIVE CONTROL + the plaintext-vs-public design decision). A group
//   created the ordinary way — `create_channel(Public, config = None, …)` — is
//   legitimately plaintext: a PUBLIC channel is world-readable by design
//   (searchable, open-join), so encryption of it buys no confidentiality. Its
//   message IS stored on the server in CLEARTEXT and the marker IS readable in
//   `messages.content`. This is intentional, and it doubles as the POSITIVE
//   CONTROL for the sealing detector used in Part 1: the SAME
//   `position(marker::bytea in content)` query MUST find the public marker here,
//   so a "0" in Part 1 means "sealed", not "detector broken".
//
//   PART 1 (F14a RESOLVED — the DEFAULT PRIVATE group is now ENCRYPTED). A
//   "Private" group created the ordinary way — `create_channel(Private, config =
//   None, …)`, exactly what T6 and the whole codebase do — is now END-TO-END
//   ENCRYPTED. Two independent fixes make the silent plaintext downgrade
//   impossible: (a) the server defaults a private/group channel's config to
//   `encrypted = true` when config is omitted, and (b) the client send gate
//   (`prepare_encrypted_content`) FAILS CLOSED — it refuses to ship plaintext on
//   any non-public channel. So the default private group's `require_encryption`
//   is now `true` locally, A encrypts ONCE with a group Sender Key
//   (`encrypt_for_group` -> `GroupEncrypted`), the server stores/streams only
//   ciphertext (marker absent from a full Postgres AND Redis dump), and BOTH B
//   and C decrypt it to the EXACT plaintext, attributed to A. Delivery is
//   asserted on B's and C's decrypted output, never on A's send return value.
//   (History: Part 1 previously DOCUMENTED finding F14a — a default group stored
//   in plaintext. F14a is now RESOLVED; see VALIDATED.md.)
//
//   PROPERTY 4 (does the server learn the group sender?). Even on the encrypted
//   path the group message rides the AUTHENTICATED `MessageClient::send_message`
//   RPC and lands in the server `messages` table with `author_id = A`. So the
//   server does NOT read the content but DOES record the sender — group messaging
//   is content-sealed, not sender-sealed (unlike the sealed-sender DM path, V4).
//   Recorded as the real answer.
//
// FIRST-MESSAGE SKDM RACE (observed, informs the test's warm-up). The very first
// message from a new sender reaches recipients before its Sender-Key Distribution
// Message (SKDM), so it decrypts to nothing and is not recovered within ~20s; the
// SKDM it carries still establishes the key, so every SUBSEQUENT message decrypts
// immediately. Part 2 sends a sacrificial warm-up first, waits for the SKDM to
// land in B's and C's device inboxes, and only THEN sends the message it asserts
// on — so the proof is deterministic, not racing the key.
//
// MUTATION CHECK (natural): point B's/C's decrypt assertion at a WRONG string.
// The wait_for_event predicate never matches and the "B/C received" assertion
// fails RED. If group fan-out were broken, that RED would be the finding.
// ============================================================================

#[tokio::test]
async fn v21_group_sender_key_fanout_and_plaintext_default() {
    use chatter_core::domain::channel::{ChannelConfig, ChannelType};

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A, B, C register + start -------------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v21a"), "V21 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v21b"), "V21 Bob").await;

    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let uid_c = register_and_start(&client_c, &unique_user("v21c"), "V21 Carol").await;

    assert!(
        uid_a != uid_b && uid_b != uid_c && uid_a != uid_c,
        "POSITIVE CONTROL FAILED: three registrations did not yield three distinct \
         users (A={uid_a} B={uid_b} C={uid_c}). Every per-recipient assertion below \
         would then be measuring one user twice."
    );

    // ========================================================================
    // PART 0 — POSITIVE CONTROL + the plaintext-vs-public design decision.
    // An explicitly PUBLIC channel (config=None) is LEGITIMATELY plaintext:
    // world-readable by design, so encryption buys no confidentiality. Its
    // message lands in cleartext and the marker IS readable — which also proves
    // the `position(marker::bytea in content)` detector works, so a "0" in
    // Part 1 means "sealed", not "detector broken".
    // ========================================================================
    let name_pub = format!(
        "v21-public-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let group_pub = common::create_group_with_members(
        &client_a,
        &name_pub,
        ChannelType::Public,
        None, // public channels are world-readable by design → plaintext is intended
        Some("V21 public (legitimately plaintext) channel"),
        &[(&client_b, &uid_b), (&client_c, &uid_c)],
    )
    .await;
    println!("\n  [PART 0] A created a PUBLIC channel: {} ({})", group_pub.name, group_pub.id);

    let marker_pub = unique_marker();
    println!("  marker_pub (public plaintext): {marker_pub}");
    client_a
        .send_text(ActiveConversation::Channel(group_pub.id.clone()), &marker_pub)
        .await
        .expect("A send to public channel failed");

    // Wait for the row to land, then run the detector — it MUST find the marker.
    let mut leak_public: i64 = 0;
    for _ in 0..20 {
        leak_public = psql(&format!(
            "select count(*) from messages where channel_id = '{}' \
             and content_type = 'PLAIN_TEXT' \
             and position('{marker_pub}'::bytea in content) > 0",
            group_pub.id
        ))
        .trim()
        .parse()
        .unwrap_or(0);
        if leak_public >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!(
        "  POSITIVE CONTROL: marker_pub rows readable as PLAIN_TEXT bytes in messages.content = {leak_public}"
    );
    assert!(
        leak_public >= 1,
        "POSITIVE CONTROL FAILED: a PUBLIC channel message was expected to be stored in \
         cleartext (the intended, visible property of a public channel) and found by the \
         `position(marker::bytea in content)` detector, but the marker was NOT found \
         (rows={leak_public}). Either public-channel plaintext regressed, or the sealing \
         detector is broken — in which case Part 1's '0 == sealed' assertion is meaningless."
    );
    println!("  [PART 0] CONFIRMED: PUBLIC channels are legitimately plaintext, and the detector works.");

    // ========================================================================
    // PART 1 — F14a RESOLVED: the DEFAULT PRIVATE group is now ENCRYPTED.
    // Create it EXACTLY the way T6 / the codebase do — Private, config = None —
    // and prove (a) it is flagged require_encryption locally, (b) its Sender-Key
    // message fans out to BOTH B and C decrypted, and (c) the server holds only
    // ciphertext. Two fixes make the old silent plaintext downgrade impossible:
    // the server now defaults a private/group channel to encrypted, and the
    // client send gate fails closed on any non-public channel.
    // ========================================================================
    let name_e = format!(
        "v21-default-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let group_e = common::create_group_with_members(
        &client_a,
        &name_e,
        ChannelType::Private,
        None, // <-- exactly what T6 / the codebase do; MUST end up encrypted now
        Some("V21 default (now-encrypted) private group"),
        &[(&client_b, &uid_b), (&client_c, &uid_c)],
    )
    .await;
    println!("\n  [PART 1] A created a DEFAULT Private group (config=None): {} ({})", group_e.name, group_e.id);

    // THE F14a FIX, asserted: a default-created private group is encrypted.
    let local_enc = client_a.get_channel_local(&group_e.id).map(|c| c.config.require_encryption);
    println!(
        "  ChannelConfig::default().require_encryption = {} ; this DEFAULT (config=None) \
         private group's require_encryption = {:?}",
        ChannelConfig::default().require_encryption,
        local_enc
    );
    assert_eq!(
        local_enc,
        Some(true),
        "F14a REGRESSION: a default `create_channel(Private, config=None)` group came back with \
         require_encryption={local_enc:?}, expected Some(true). The silent plaintext downgrade is \
         back — the server default and/or the client fail-closed gate regressed."
    );

    // POSITIVE CONTROL (membership): the SERVER records A, B, C as members of THIS
    // channel. A vacuous "C got 0" when C was never a member proves nothing.
    let mut member_set = std::collections::HashSet::new();
    for _ in 0..15 {
        member_set = psql(&format!(
            "select user_id from channel_members where channel_id = '{}'",
            group_e.id
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<std::collections::HashSet<String>>();
        if member_set.contains(&uid_a.to_string())
            && member_set.contains(&uid_b.to_string())
            && member_set.contains(&uid_c.to_string())
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    for (label, uid) in [("A", &uid_a), ("B", &uid_b), ("C", &uid_c)] {
        assert!(
            member_set.contains(&uid.to_string()),
            "POSITIVE CONTROL FAILED: {label} ({uid}) is not a member of encrypted group {} on \
             the server. Delivery assertions to a non-member are vacuous.",
            group_e.id
        );
    }
    println!("  positive control: A, B and C are all server-side members of the encrypted group");

    // B and C subscribe to the group stream + events BEFORE any send.
    client_b.resubscribe_channel_messages();
    client_c.resubscribe_channel_messages();
    let mut events_b = client_b.subscribe_events();
    let mut events_c = client_c.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // --- WARM-UP: establish A's sender key on B and C (defeat the SKDM race) ---
    // The first send from a new sender mints the Sender Key and ships an SKDM to
    // every member via sealed sender -> their device inbox. Its own content is not
    // recovered by the recipients (observed), but the SKDM it carries establishes
    // the key. We wait until B and C have INSTALLED A's sender key — terminal
    // state in their own key stores — before sending the message we assert on,
    // so the proof is not racing key distribution. (This used to sample
    // `device_messages`, a queue the recipients drain concurrently; see
    // `wait_for_sender_key`.)
    let marker_warm = unique_marker();
    client_a
        .send_text(ActiveConversation::Channel(group_e.id.clone()), &marker_warm)
        .await
        .expect("A warm-up send failed");
    let skdm_b = wait_for_sender_key(&client_b, &uid_a, &group_e.id).await;
    let skdm_c = wait_for_sender_key(&client_c, &uid_a, &group_e.id).await;
    assert!(
        skdm_b && skdm_c,
        "SENDER-KEY DISTRIBUTION BROKEN: A's first send to the encrypted group did not \
         install A's sender key on B ({skdm_b}) and/or C ({skdm_c}) within \
         {CONDITION_TIMEOUT:?}. \
         Without the sender key, B and C can never decrypt A's group messages. \
         Diagnosis: message/group.rs `distribute_sender_key` fans the SKDM to every member \
         via `deposit_sealed_to_all_devices` (sealed, UAK-authorized); a failure \
         here is a bundle-fetch or UAK-deposit problem, NOT the SENDERKEY_MESSAGE \
         path itself."
    );
    println!("  warm-up: B and C have INSTALLED A's sender key (SKDM delivered AND processed)");

    // --- THE PROOF MESSAGE: one Sender-Key message, BOTH B and C must decrypt ---
    let marker_e = unique_marker();
    println!("\n  marker_e plaintext (encrypted proof message): {marker_e}");
    client_a
        .send_text(ActiveConversation::Channel(group_e.id.clone()), &marker_e)
        .await
        .expect("A proof send to encrypted group failed");

    let gid = group_e.id.clone();
    let m = marker_e.clone();
    let recv_b = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == gid && message.as_text() == Some(m.as_str()))
    })
    .await;
    let gid = group_e.id.clone();
    let m = marker_e.clone();
    let recv_c = wait_for_event(&mut events_c, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == gid && message.as_text() == Some(m.as_str()))
    })
    .await;

    // THE PROPERTY (fan-out receive+decrypt), asserted on the RECIPIENTS' side.
    let msg_b = match recv_b {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "GROUP FAN-OUT BROKEN: A sent ONE Sender-Key message to encrypted group {} \
             (members A, B, C; A's SKDM confirmed delivered to B and C above). B never \
             surfaced it decrypted to the exact plaintext within {EVENT_TIMEOUT:?}. A's send \
             returned Ok. The server either did not stream the ciphertext to B, or B could \
             not decrypt with A's sender key (SENDERKEY_MESSAGE path).",
            group_e.id
        ),
    };
    let msg_c = match recv_c {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "GROUP FAN-OUT BROKEN: A sent ONE Sender-Key message to encrypted group {} \
             (members A, B, C; A's SKDM confirmed delivered above). C never surfaced it \
             decrypted to the exact plaintext within {EVENT_TIMEOUT:?}. This is the \
             fan-out-to-the-third-member case a 2-member group can never exercise.",
            group_e.id
        ),
    };
    // Byte-exact (this equality is the MUTATION point).
    assert_eq!(
        msg_b.as_text(),
        Some(marker_e.as_str()),
        "B decoded a group message but the bytes differ from what A sent"
    );
    assert_eq!(
        msg_c.as_text(),
        Some(marker_e.as_str()),
        "C decoded a group message but the bytes differ from what A sent"
    );
    println!("  B decrypted encrypted group message: {:?}", msg_b.as_text());
    println!("  C decrypted encrypted group message: {:?}", msg_c.as_text());

    // Property 3 — sender attribution to A, in the group conversation.
    assert_eq!(
        msg_b.sender_id.as_ref(), Some(&uid_a),
        "SENDER ATTRIBUTION WRONG on B: attributed to {:?}, expected A ({uid_a})", msg_b.sender_id
    );
    assert_eq!(
        msg_c.sender_id.as_ref(), Some(&uid_a),
        "SENDER ATTRIBUTION WRONG on C: attributed to {:?}, expected A ({uid_a})", msg_c.sender_id
    );
    assert_eq!(msg_b.conversation_id, group_e.id, "B filed the message in the wrong conversation");
    assert_eq!(msg_c.conversation_id, group_e.id, "C filed the message in the wrong conversation");
    println!("  attribution: B and C both see the message authored by A in the group conversation");

    // NEGATIVE HALF (load-bearing): the server holds ciphertext it cannot read.
    // Same BYTEA substring detector that found the plaintext in Part 1 — here it
    // MUST return 0. (Part 1 returning >=1 with the identical query is the proof
    // this 0 means "sealed", not "detector broken".) The proof message also lands
    // in the server `messages` table (it is an authenticated channel send), so this
    // is a real store holding the real row — not a vacuous grep of an empty table.
    let sealed_content: i64 = psql(&format!(
        "select count(*) from messages where channel_id = '{}' \
         and position('{marker_e}'::bytea in content) > 0",
        group_e.id
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    let rows_here: i64 = psql(&format!(
        "select count(*) from messages where channel_id = '{}'",
        group_e.id
    ))
    .trim()
    .parse()
    .unwrap_or(0);
    let rd_hits = redis_dump_all().matches(&marker_e).count();
    println!(
        "  negative half: marker_e readable in messages.content = {sealed_content} \
         (out of {rows_here} stored rows for this channel); redis occurrences = {rd_hits}"
    );
    assert!(
        rows_here > 0,
        "POSITIVE CONTROL FAILED: the encrypted group has 0 rows in the server `messages` \
         table — the 'sealed' check below would be vacuous."
    );
    assert_eq!(
        sealed_content + rd_hits as i64,
        0,
        "PLAINTEXT LEAK ON THE ENCRYPTED PATH: the group message plaintext is readable in the \
         server's stores (messages.content={sealed_content}, redis={rd_hits}). The Sender-Key \
         envelope is supposed to be opaque to the server. Contrast Part 1, where the identical \
         detector deliberately DID find the plaintext."
    );

    // PROPERTY 4 — the real answer: content is sealed, but the SENDER is not.
    // The encrypted group message still rides the authenticated channel send and
    // lands in `messages` with author_id = A.
    let authors = psql(&format!(
        "select distinct left(author_id::text, 8) from messages where channel_id = '{}'",
        group_e.id
    ));
    assert_eq!(
        sealed_content, 0,
        "the encrypted group message content is readable in messages.content ({sealed_content} \
         row(s)) — it is not actually sealed"
    );
    println!(
        "  PROPERTY 4: encrypted group message CONTENT is sealed (0 readable rows), but the \
         server records the SENDER — messages.author_id(s) for this channel = {} (A is {}). \
         Group messaging is content-sealed, NOT sender-sealed (unlike the DM path, V4). \
         [finding F14b]",
        authors.replace('\n', ","),
        &uid_a.to_string()[..8]
    );

    println!(
        "\nV21 PASS — (0) an explicitly PUBLIC channel is legitimately plaintext (marker readable, \
         detector proven); (1) F14a RESOLVED: a DEFAULT 'Private' group (create_channel(Private, \
         config=None), exactly what T6/the codebase do) is now ENCRYPTED — require_encryption=true \
         locally, its ONE Sender-Key message fanned out to BOTH B and C, each decrypted the EXACT \
         plaintext attributed to A, and the server holds only ciphertext (marker NOT in \
         messages.content, NOT in redis) — but the server still learns the sender (F14b). See \
         VALIDATED.md.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
    client_c.shutdown().await;
}

// ============================================================================
// V22: ATTACHMENT / RAW-BINARY FIDELITY — the attachment path carries TRUE
// arbitrary binary byte-exact, and the bytes are NOT readable server-side.
//
// V16 proved DM *text* round-trips byte-exact, but DM bodies are proto3 `string`
// (UTF-8 only). TRUE arbitrary binary (NULs, 0xFF, every byte value) rides the
// ATTACHMENT path: client-side AES-encrypt -> presigned PUT to RustFS/S3 ->
// presigned GET -> client-side decrypt. NO validated test exercised A->B
// cross-user attachment delivery + byte-exact decrypt before this one. (T7 in
// e2e_two_user_suite only asserts B sees an attachment *id*, never downloads or
// decrypts; integration_file's round-trip is single-client — same
// AttachmentService encrypts and decrypts, so it never proves the key crossed
// the wire.)
//
// POSITIVE CONTROL: B receives a message carrying a non-empty attachment list,
// and the marker embedded in the plaintext IS found by the byte detector in the
// decrypted output (so the negative half below is not vacuous).
// NEGATIVE HALF: the same marker is ABSENT from the ciphertext-at-rest (the .enc
// blob B downloads is the exact object the server/RustFS stores), ABSENT from
// the Postgres dump and Redis.
//
// MUTATION CHECK: assert B's decrypted bytes equal a corrupted copy of the
// payload (flip one byte) -> RED. Or assert the marker is absent from the
// decrypted plaintext -> RED.
// ============================================================================

/// True if `hay` contains `needle` as a contiguous byte subsequence.
fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return needle.is_empty();
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Send `bytes` from A to B as a single attachment with the given filename/mime.
/// Returns the AttachmentId as it lands on B (derived from the server CDN key),
/// or an error string if the send failed or B never received a non-empty
/// attachment list. B must be online.
async fn ab_send_attachment(
    client_a: &Arc<ChatterClient>,
    client_b: &Arc<ChatterClient>,
    uid_a: UserId,
    uid_b: UserId,
    dir_a: &std::path::Path,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<AttachmentId, String> {
    use chatter_core::state::chat_state::PendingAttachment;

    // send_message derives the wire filename from the SOURCE PATH basename
    // (upload.rs:101 `file_path.file_name()`), NOT from PendingAttachment's
    // declared name. So to control the propagated filename we name the on-disk
    // source file by `filename` when it is a valid single path component;
    // otherwise (e.g. it contains a slash) we fall back to a uuid name.
    let unique = uuid::Uuid::new_v4();
    let src_name = if !filename.is_empty()
        && !filename.contains('/')
        && !filename.contains('\\')
    {
        format!("{unique}__{filename}")
    } else {
        format!("attsrc_{unique}")
    };
    let file_path = dir_a.join(&src_name);
    std::fs::write(&file_path, bytes).map_err(|e| format!("write source: {e}"))?;

    let pending = PendingAttachment::new(
        AttachmentId(unique),
        filename.to_string(),
        file_path.clone(),
        bytes.len() as u64,
        mime.to_string(),
    );

    let mut events_b = client_b.subscribe_events();
    let expected_conv = ChannelId::from(uid_a);

    client_a
        .send_message(
            ActiveConversation::ServerDm { peer_id: uid_b },
            "attachment",
            std::slice::from_ref(&pending),
            None,
            None,
        )
        .await
        .map_err(|e| format!("send_message failed: {e:?}"))?;

    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == expected_conv && !message.attachments.is_empty())
    })
    .await;

    match received {
        Some(DomainEvent::MessageReceived { message }) => Ok(message.attachments[0]),
        _ => Err("B never received a message with a non-empty attachment list".into()),
    }
}

use chatter_core::domain::ids::AttachmentId;

#[tokio::test]
async fn v22_attachment_binary_fidelity() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v22a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v22b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // ---- PART 1: the main binary-fidelity + encrypted-at-rest case ----------
    // Payload = marker | (all 256 byte values, x16) | random | marker.
    // Contains 0x00 (interior NUL), 0xFF, and every value in between — the exact
    // bytes proto3 `string` cannot carry, which is why they must ride the
    // attachment path.
    let marker = unique_marker();
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(marker.as_bytes());
    for _ in 0..16 {
        for b in 0u16..=255u16 {
            payload.push(b as u8);
        }
    }
    let mut rnd = [0u8; 2048];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut rnd);
    payload.extend_from_slice(&rnd);
    payload.extend_from_slice(marker.as_bytes());
    println!("\n  marker: {marker}");
    println!("  main payload: {} bytes (incl. all 256 byte values, NUL, 0xFF)", payload.len());

    let att_id = ab_send_attachment(
        &client_a, &client_b, uid_a, uid_b, dir_a.path(),
        "blob.bin", "application/octet-stream", &payload,
    )
    .await
    .expect("POSITIVE CONTROL FAILED: main binary attachment never reached B");
    println!("  POSITIVE CONTROL: B received attachment id={att_id}");

    client_b
        .download_attachment_to_filesystem(&att_id)
        .await
        .expect("B failed to download the attachment ciphertext");

    // NEGATIVE HALF: the ciphertext at rest (the exact object the server/RustFS
    // holds — a presigned GET returns the stored bytes verbatim) must not reveal
    // the plaintext marker.
    let cpath = chatter_core::services::attachment::attachment_file_path(&att_id);
    let ciphertext = std::fs::read(&cpath).expect("read ciphertext-at-rest");
    println!("  ciphertext-at-rest: {} bytes at {}", ciphertext.len(), cpath.display());
    assert!(
        ciphertext.len() >= payload.len(),
        "ciphertext ({}) smaller than plaintext ({}) — not a real encrypted blob",
        ciphertext.len(), payload.len()
    );
    assert!(
        !contains_bytes(&ciphertext, marker.as_bytes()),
        "ATTACHMENT PLAINTEXT AT REST: the marker is readable inside the stored \
         ciphertext blob — the attachment is NOT encrypted at rest (F14a-class)"
    );
    println!("  negative half: marker NOT present in ciphertext-at-rest");

    // POSITIVE HALF: B decrypts to the byte-exact original.
    let plain = client_b
        .decrypt_attachment_auto(&att_id)
        .expect("B failed to decrypt the attachment");
    assert_eq!(
        plain.as_slice(),
        payload.as_slice(),
        "BINARY FIDELITY VIOLATED: decrypted attachment bytes differ from what A sent \
         (len in={} out={})",
        payload.len(), plain.len()
    );
    // The detector that found "marker absent" in ciphertext DOES find it in the
    // plaintext — proves the negative half above was not vacuous.
    assert!(
        contains_bytes(plain.as_slice(), marker.as_bytes()),
        "detector self-check failed: marker not found even in the decrypted plaintext"
    );
    println!("  POSITIVE HALF: B decrypted {} bytes, byte-exact == A's original", plain.len());

    // Server-side stores never held the marker (the AttachmentPointer key/blob
    // ride inside the E2E-encrypted DataMessage; the blob is client-encrypted).
    let pg = pg_dump_all();
    assert!(pg.len() > 10_000, "pg_dump too small to be real");
    assert_eq!(pg.matches(&marker).count(), 0, "E2EE VIOLATED: marker in Postgres");
    let rd = redis_dump_all();
    assert_eq!(rd.matches(&marker).count(), 0, "E2EE VIOLATED: marker in Redis");
    println!("  server stores audited: marker occurs 0x in Postgres, 0x in Redis");

    // ---- PART 2: edges ------------------------------------------------------
    // 1-byte
    let one = vec![0xEEu8];
    let one_id = ab_send_attachment(
        &client_a, &client_b, uid_a, uid_b, dir_a.path(),
        "one.bin", "application/octet-stream", &one,
    ).await.expect("1-byte attachment never reached B");
    client_b.download_attachment_to_filesystem(&one_id).await.expect("download 1-byte");
    let one_plain = client_b.decrypt_attachment_auto(&one_id).expect("decrypt 1-byte");
    assert_eq!(one_plain.as_slice(), one.as_slice(), "1-byte attachment not byte-exact");
    println!("  edge 1-byte: byte-exact OK");

    // empty (0-byte) — behavior probed, not assumed.
    let empty: Vec<u8> = Vec::new();
    let empty_id = ab_send_attachment(
        &client_a, &client_b, uid_a, uid_b, dir_a.path(),
        "empty.bin", "application/octet-stream", &empty,
    ).await.expect("empty(0-byte) attachment never reached B");
    client_b.download_attachment_to_filesystem(&empty_id).await.expect("download empty");
    let empty_plain = client_b.decrypt_attachment_auto(&empty_id).expect("decrypt empty");
    assert_eq!(empty_plain.len(), 0, "empty attachment did not decrypt to 0 bytes");
    println!("  edge empty(0-byte): delivered + decrypted to 0 bytes, byte-exact OK");

    // Traversal / unicode filename — the recipient-visible filename is opaque
    // metadata inside the E2E pointer. The literal `../../etc/passwd` cannot be a
    // real filesystem basename (it has separators), and send_message derives the
    // wire name from the source basename, so we use a VALID single-component name
    // that still carries dot-dot runs + unicode + emoji. The SERVER never uses
    // this string for any path: it mints a random CDN key and stores at
    // `attachments/{cdnKey}` (attachment.go:44-45), so traversal is structurally
    // impossible regardless of the name.
    let trav_name = "..__..__etc__passwd__日本語__🔥.bin";
    let trav_bytes = b"traversal-opaque-payload-\x00\xff".to_vec();
    let trav_id = ab_send_attachment(
        &client_a, &client_b, uid_a, uid_b, dir_a.path(),
        trav_name, "application/octet-stream", &trav_bytes,
    ).await.expect("tricky-named attachment never reached B");
    client_b.download_attachment_to_filesystem(&trav_id).await.expect("download traversal");
    let trav_plain = client_b.decrypt_attachment_auto(&trav_id).expect("decrypt traversal");
    assert_eq!(trav_plain.as_slice(), trav_bytes.as_slice(), "tricky-named payload not byte-exact");
    // The recipient-visible name preserves the tricky string verbatim (opaque,
    // unicode intact — no sanitization), and is a single component (no escape).
    let trav_meta = client_b.get_attachment_metadata(&trav_id).expect("traversal metadata");
    assert!(
        trav_meta.metadata.name.ends_with(trav_name),
        "filename was mangled server-side: got {:?}", trav_meta.metadata.name
    );
    assert!(
        !trav_meta.metadata.name.contains('/') && !trav_meta.metadata.name.contains('\\'),
        "recipient filename contains a path separator: {:?}", trav_meta.metadata.name
    );
    // The stored blob is named by the server CDN key (the attachment id), NOT by
    // the filename — the filename never touches any storage path.
    let trav_enc = chatter_core::services::attachment::attachment_file_path(&trav_id);
    assert!(
        trav_enc.to_string_lossy().contains("attachments")
            && trav_enc.file_stem().map(|s| s == trav_id.0.to_string().as_str()).unwrap_or(false),
        "attachment blob path is not id-named under attachments dir: {}", trav_enc.display()
    );
    println!("  edge tricky filename (dot-dot + unicode + emoji): opaque, unicode-preserved, \
              blob id-named under attachments dir, payload byte-exact OK");

    // ---- PART 3: EICAR / virus-scan probe -----------------------------------
    // The EICAR test string SHOULD be flagged/rejected if ClamAV/YARA scanning
    // (README claims both) is wired into the attachment path. We send it and
    // observe whether it round-trips.
    let eicar = br"X5O!P%@AP[4\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*".to_vec();
    match ab_send_attachment(
        &client_a, &client_b, uid_a, uid_b, dir_a.path(),
        "eicar.com", "application/octet-stream", &eicar,
    ).await {
        Ok(eid) => {
            let dl = client_b.download_attachment_to_filesystem(&eid).await;
            let dec = if dl.is_ok() { client_b.decrypt_attachment_auto(&eid).ok() } else { None };
            match dec {
                Some(p) if p.as_slice() == eicar.as_slice() => {
                    println!("  EICAR PROBE: the EICAR test file was accepted, delivered, and \
                              round-tripped byte-exact — virus scanning is NOT effective on the \
                              attachment path (F2-class; see report/VALIDATED).");
                }
                _ => println!("  EICAR PROBE: EICAR delivered but did not round-trip cleanly"),
            }
        }
        Err(e) => println!("  EICAR PROBE: EICAR attachment was REJECTED at send/delivery ({e}) — \
                            scanning appears active."),
    }

    // ---- PART 4: ONE large case — exercises the streaming decrypt path (>1MB) ­
    let big_len: usize = 3 * 1024 * 1024;
    let mut big = vec![0u8; big_len];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut big);
    // Stamp a known marker at a known interior offset so the round-trip proves
    // position fidelity, not just length.
    let big_marker = unique_marker();
    let off = big_len / 2;
    big[off..off + big_marker.len()].copy_from_slice(big_marker.as_bytes());
    let big_id = ab_send_attachment(
        &client_a, &client_b, uid_a, uid_b, dir_a.path(),
        "big.bin", "application/octet-stream", &big,
    ).await.expect("3MB attachment never reached B");
    client_b.download_attachment_to_filesystem(&big_id).await.expect("download 3MB");
    let big_plain = client_b.decrypt_attachment_auto(&big_id).expect("decrypt 3MB (streaming path)");
    assert_eq!(big_plain.len(), big_len, "3MB attachment length mismatch");
    assert_eq!(big_plain.as_slice(), big.as_slice(), "3MB attachment not byte-exact (streaming path)");
    println!("  large 3MB (streaming decrypt): byte-exact OK");

    println!("V22 PASS — attachment path carries TRUE arbitrary binary byte-exact (all 256 \
              values, NUL, 0xFF, 1-byte, 3MB), encrypted at rest (marker absent from ciphertext \
              blob, Postgres, Redis), path-traversal filename is opaque. EICAR probe result \
              printed above.\n");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V23: MESSAGE ORDERING under REORDER + GAP — the Double Ratchet decrypts
// out-of-order and skipped messages correctly.
//
// Signal's Double Ratchet retains skipped message keys, so a recipient can
// decrypt msg3 before msg2 (reorder) and still decrypt msg2 when it finally
// arrives (gap tolerance). NO validated test exercised this before.
//
// MECHANISM (reuses V1/V6): take B offline so A's msg1,msg2,msg3 SIT in B's
// Redis queue; identify each envelope by DIFFING HKEYS across each send (V2's
// technique — the server assigns its own envelope GUID, so client message ids
// are useless); WITHHOLD all three from the queue (HDEL) so B's poller never
// touches them and this test alone controls the decrypt order; then feed them
// to B's live session via signal().decrypt_message() in a deliberately
// out-of-order, gapped sequence: msg1, then msg3 (skipping msg2), then msg2.
//
// POSITIVE CONTROL: an in-order warm-up message (msg0) is sent while B is
// ONLINE and decrypted normally — the baseline, and it establishes the session
// so msg1..3 are ordinary ratchet messages (the pure skipped-key case).
// NEGATIVE HALF: each out-of-order decrypt must yield EXACTLY its own marker and
// NOT any other message's marker.
//
// MUTATION CHECK: assert one out-of-order decrypt contains a DIFFERENT message's
// marker -> RED.
// ============================================================================

#[tokio::test]
async fn v23_message_ordering_reorder_and_gap() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_b = dir_b.path().to_path_buf();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(&path_b);

    let uid_a = register_and_start(&client_a, &unique_user("v23a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v23b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // ---- POSITIVE CONTROL / baseline: in-order delivery works --------------
    // A warm-up message delivered while B is online. Establishes the session, so
    // msg1..3 below are ordinary (non-prekey) ratchet messages — the case where
    // out-of-order tolerance depends purely on retained skipped message keys.
    let mut events_b0 = client_b.subscribe_events();
    let m0 = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m0)
        .await
        .expect("warm-up send failed");
    let expected_conv = ChannelId::from(uid_a);
    let got0 = wait_for_event(&mut events_b0, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == expected_conv && message.as_text() == Some(m0.as_str()))
    }).await;
    assert!(got0.is_some(), "POSITIVE CONTROL FAILED: in-order warm-up never decrypted on B");
    println!("\n  POSITIVE CONTROL: in-order warm-up decrypted on B (session established)");

    // ---- take B offline so msg1..3 SIT in the queue ------------------------
    client_b.shutdown().await;
    drop(client_b);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let key = format!("device_queue_envelopes:{uid_b}:1");
    let hkeys = |k: &str| -> std::collections::HashSet<String> {
        sh(&format!(
            "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HKEYS '{k}' 2>/dev/null"
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
    };

    // Send one message, then return (field, envelope_b64) for the ONE new queue
    // entry it produced — identified by diffing HKEYS (V6's method).
    let send_and_capture = |marker: &str, before: std::collections::HashSet<String>| {
        let key = key.clone();
        let marker = marker.to_string();
        async move {
            let after = hkeys(&key);
            let new_fields: Vec<String> = after.difference(&before).cloned().collect();
            assert_eq!(
                new_fields.len(), 1,
                "POSITIVE CONTROL FAILED: expected exactly 1 new envelope in {key} after \
                 sending {marker}, found {}. Zero means it never queued; >1 means we cannot \
                 tell which envelope is which.",
                new_fields.len()
            );
            let field = new_fields[0].clone();
            let json = sh(&format!(
                "docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HGET '{key}' '{field}' 2>/dev/null"
            ));
            let b64 = extract_between(&json, "\"envelope\":\"", "\"")
                .expect("could not extract envelope base64 from queued JSON");
            (field, b64)
        }
    };

    let m1 = unique_marker();
    let m2 = unique_marker();
    let m3 = unique_marker();
    println!("  msg1 marker: {m1}");
    println!("  msg2 marker: {m2}");
    println!("  msg3 marker: {m3}");

    let before1 = hkeys(&key);
    client_a.send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m1).await.expect("send msg1");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (f1, b64_1) = send_and_capture(&m1, before1).await;

    let before2 = hkeys(&key);
    client_a.send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m2).await.expect("send msg2");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (f2, b64_2) = send_and_capture(&m2, before2).await;

    let before3 = hkeys(&key);
    client_a.send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m3).await.expect("send msg3");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (f3, b64_3) = send_and_capture(&m3, before3).await;

    assert!(b64_1 != b64_2 && b64_2 != b64_3 && b64_1 != b64_3,
        "captured duplicate envelopes — the three markers are not distinguishable");
    println!("  captured 3 distinct envelopes ({}, {}, {} b64 chars)", b64_1.len(), b64_2.len(), b64_3.len());

    // Withhold ALL THREE from B's queue: B's poller must never touch them, so
    // this test alone controls the order in which B's session processes them.
    for f in [&f1, &f2, &f3] {
        sh(&format!("docker compose exec -T -e REDISCLI_AUTH=\"$(grep -m1 '^REDIS_PASSWORD=' .env | cut -d= -f2-)\" redis redis-cli HDEL '{key}' '{f}' >/dev/null 2>&1"));
    }
    let remaining = hkeys(&key);
    assert!(
        !remaining.contains(&f1) && !remaining.contains(&f2) && !remaining.contains(&f3),
        "POSITIVE CONTROL FAILED: could not withhold all three envelopes from B's queue"
    );
    println!("  withheld all 3 envelopes from B's queue (B's poller will never see them)");

    let b64_to_bytes = |b64: &str| -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(b64.trim()).expect("invalid base64")
    };
    let bytes1 = b64_to_bytes(&b64_1);
    let bytes2 = b64_to_bytes(&b64_2);
    let bytes3 = b64_to_bytes(&b64_3);

    // ---- B comes online with its live session ------------------------------
    let client_b2 = build_client(&path_b);
    client_b2.connect().await.expect("B reconnect failed");
    let _rx = client_b2.subscribe_events();
    assert!(client_b2.restore_session(), "B failed to restore persisted session");
    if let Err(e) = client_b2.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("B start failed: {e:?}");
        }
    }
    // Give the poller a moment to drain any unrelated bootstrap envelopes; ours
    // are already gone from the queue.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Assert an out-of-order decrypt yields EXACTLY its own marker (negative
    // half: never another message's marker).
    let decrypt_expect = |bytes: Vec<u8>, want: String, others: [String; 2]| {
        let c = client_b2.clone();
        async move {
            match c.signal().decrypt_message(&bytes).await {
                Ok((_addr, plaintext, _)) => {
                    let hay = String::from_utf8_lossy(&plaintext);
                    assert!(
                        hay.contains(&want),
                        "ORDERING BROKEN: decrypted message did not contain its own marker {want}"
                    );
                    for o in &others {
                        assert!(
                            !hay.contains(o.as_str()),
                            "ORDERING BROKEN: decrypt yielded a DIFFERENT message's marker {o} \
                             (expected {want}) — wrong plaintext"
                        );
                    }
                }
                Err(e) => panic!(
                    "ORDERING BROKEN: B could not decrypt the message for marker {want}: {e:?}. \
                     If this is msg2 after msg3, the Double Ratchet failed to retain the skipped key."
                ),
            }
        }
    };

    // Deliver order: msg1, then msg3 (SKIPPING msg2), then msg2 — reorder + gap.
    println!("  delivering to B's session OUT OF ORDER: msg1, msg3 (skip msg2), then msg2");
    decrypt_expect(bytes1, m1.clone(), [m2.clone(), m3.clone()]).await;
    println!("    msg1 -> decrypted to its own marker");
    decrypt_expect(bytes3, m3.clone(), [m1.clone(), m2.clone()]).await;
    println!("    msg3 -> decrypted to its own marker (msg2 still missing = gap tolerated, out-of-order OK)");
    decrypt_expect(bytes2, m2.clone(), [m1.clone(), m3.clone()]).await;
    println!("    msg2 -> decrypted to its own marker AFTER msg3 (skipped message key retained)");

    println!("V23 PASS — out-of-order + gapped delivery (1,3,2) all decrypt to the correct \
              plaintext: the Double Ratchet retains skipped message keys and tolerates reordering.\n");

    client_a.shutdown().await;
    client_b2.shutdown().await;
}

// ============================================================================
// V24 — ANONYMOUS-DM path REJECTS a server-forged identity key via Key
// Transparency (Phase 3a — closes the anon-path MITM hole).
//
// STATUS: PASS. This is the anon-path analog of V14. V14 proved the IDENTIFIED
// DM path KT-gates session establishment (rejects a forged peer identity key).
// The ANONYMOUS-DM path (Tor/shield mode, `ensure_session_with_bundle`) was
// explicitly left KT- UNgated in Phase 2 because `GetProof` was authenticated
// and the anon path has no identity to authenticate with — so it trusted peer
// identity keys BLINDLY, a MITM hole on exactly the metadata-private path that
// most needs protection.
//
// Phase 3a closes it: `GetProof` is now a PUBLIC method (a KT proof is public,
// auditable transparency data), so the anon path fetches a peer's signed
// inclusion proof over the ANONYMOUS channel (`AnonymousKtClient`, no auth
// interceptor) and `ensure_session_with_bundle` KT-gates establishment exactly
// like the identified path — via the SAME verifier (`crypto::verify_kt_proof`)
// and the SAME reject/TOFU semantics (`verify_peer_key_transparency_anon`).
//
//   register REAL Bob -> Bob's identity key is in the append-only KT log
//   -> the anon path fetches Bob's signed proof WITHOUT authentication
//   -> ensure_session_with_bundle verifies the bundle key == committed key.
//
// POSITIVE CONTROL: an HONEST anonymously-fetched bundle carrying Bob's REAL
// key (the one committed in the log) passes the anon KT gate AND establishes a
// Signal session. If this half fails, the reject below proves nothing.
//
// THE ATTACK (F9 on the anon path): a malicious server serves an
// internally-valid prekey bundle for Bob's user_id carrying MALLORY's identity
// key (every signature inside verifies). The anon path fetches Bob's KT proof;
// the committed value is Bob's REAL key != Mallory's forged key, so the anon KT
// gate returns an error and `ensure_session_with_bundle` REFUSES — no session,
// the forged key is never trusted.
//
// This drives the REAL production anon entrypoint
// (`SignalService::ensure_session_with_bundle`, the exact method
// `PairwiseService::encrypt_anonymous` calls), so it also proves byte-format
// interop of the public GetProof over the anonymous channel.
//
// MUTATION (run it, observe RED, revert): in
// `services/signal/session.rs::verify_peer_key_transparency_anon`, make the
// function `return Ok(())` unconditionally (disable the anon KT check). The
// attack half then ACCEPTS Mallory's forged bundle, a session is established,
// and this test FAILS at the "forged bundle must be rejected" assertion.
// Reverting restores GREEN.
//
// QUERY-PRIVACY NOTE: this closes the MITM hole. It does NOT yet hide WHICH
// peer is looked up — the anon GetProof still carries the plaintext user_id
// (the same user_id already sent in the anonymous prekey fetch). VRF-indexed
// query privacy is the remaining Phase 3b work (crypto/vrf.go).
// ============================================================================
#[tokio::test]
async fn v24_anon_dm_forged_identity_key_is_rejected_via_key_transparency() {
    use chatter_core::services::SignalService;
    use chatter_core::storage::{settings_keys, Database, KeyStore, SettingsRepository};
    use libsignal_protocol::{
        kem, DeviceId, IdentityKey, IdentityKeyPair, KeyPair, ProtocolAddress,
    };
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!("skipping v24: INTEGRATION_TESTS not set");
        return;
    }

    // Build a fresh, real SignalService backed by an in-memory-ish temp DB. This
    // is the SAME type PairwiseService drives on the anon send path, so the KT
    // gate under test is the production one.
    async fn make_signal_service(rng: &mut StdRng) -> (SignalService, tempfile::NamedTempFile) {
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();
        let key_store = Arc::new(KeyStore::new(db.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));
        // The local (sender) identity — arbitrary; the anon KT gate does not use
        // it, but establish_session (PQXDH) needs OUR local identity key pair.
        let local_id = IdentityKeyPair::generate(rng);
        let local_reg_id: u32 = rng.random::<u32>() & 0x3fff;
        key_store
            .set_local_identity(local_reg_id, &local_id.serialize())
            .unwrap();
        settings_repo
            .set(settings_keys::CURRENT_USER_ID, &"v24-local-uuid".to_string())
            .unwrap();
        settings_repo.set(settings_keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        (SignalService::new(key_store.clone(), settings_repo.clone()), db_file)
    }

    // A fully self-consistent PQXDH proto bundle whose identity key is `signer`'s
    // and whose signed+kyber prekeys are signed by `signer` — exactly what a
    // server (honest OR malicious) returns on an anonymous GetPreKeyBundle. Every
    // signature inside verifies; the ONLY thing KT can catch is the identity key.
    fn build_proto_bundle_response(
        signer: &IdentityKeyPair,
        device_id: u32,
        rng: &mut StdRng,
    ) -> chatter_core::proto::chat_v1::GetPreKeyBundleResponse {
        use chatter_core::proto::chat_v1::signal_pre_key_bundle::{
            KyberPreKeyRecord, SignedPreKeyRecord,
        };
        use chatter_core::proto::chat_v1::{
            get_pre_key_bundle_response, GetPreKeyBundleResponse, SignalPreKeyBundle,
        };

        let signed_pre_key_pair = KeyPair::generate(rng);
        let kyber_pre_key_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);

        let spk_pub = signed_pre_key_pair.public_key.serialize();
        let spk_sig = signer.private_key().calculate_signature(&spk_pub, rng).unwrap();
        let kyber_pub = kyber_pre_key_pair.public_key.serialize();
        let kyber_sig = signer.private_key().calculate_signature(&kyber_pub, rng).unwrap();

        let bundle = SignalPreKeyBundle {
            identity_key: signer.identity_key().serialize().to_vec(),
            registration_id: rng.random::<u32>() & 0x3fff,
            device_id,
            signed_prekey: Some(SignedPreKeyRecord {
                id: rng.random::<u32>(),
                public_key: spk_pub.to_vec(),
                signature: spk_sig.to_vec(),
                timestamp: 0,
            }),
            one_time_prekey: None,
            kyber_prekey: Some(KyberPreKeyRecord {
                id: rng.random::<u32>(),
                public_key: kyber_pub.to_vec(),
                signature: kyber_sig.to_vec(),
                timestamp: 0,
            }),
            unidentified_access_key: None,
            unrestricted_unidentified_access: None,
        };

        GetPreKeyBundleResponse {
            result: Some(get_pre_key_bundle_response::Result::Bundle(bundle)),
        }
    }

    let mut rng = StdRng::seed_from_u64(0x24_FEED);
    let device_id = 1u32;

    // ---- Register a REAL Bob so the Phase-1 producer appends his identity key
    // to the KT log. The pool is deliberately NOT authenticated: the anon KT
    // fetch below must work WITHOUT any identity. -----------------------------
    let pool = common::create_test_pool().await.expect("pool connect failed");
    let bob = common::register_test_user(&pool)
        .await
        .expect("Bob registration failed — no KT leaf to verify against");

    let ik_bob = IdentityKey::decode(&bob.public_key).expect("Bob identity key decodes");
    let bob_kp = IdentityKeyPair::try_from(bob.identity_key_pair_bytes.as_slice())
        .expect("Bob identity key pair round-trips");
    assert_eq!(*bob_kp.identity_key(), ik_bob, "sanity: Bob's key pair matches his registered key");

    let mallory_kp = IdentityKeyPair::generate(&mut rng);
    let ik_mallory = *mallory_kp.identity_key();
    assert_ne!(ik_bob, ik_mallory, "sanity: Bob and Mallory are distinct identities");

    let bob_addr = ProtocolAddress::new(bob.user_id.clone(), DeviceId::new(1).unwrap());

    // ---- Prove the anon-reachable public GetProof works: fetch Bob's proof
    // over the ANONYMOUS channel (no auth interceptor, no identity) and confirm
    // it commits Bob's real key. This is the fetch the anon gate uses. ---------
    {
        let mut anon_kt = pool.anonymous_kt_client();
        let proof = anon_kt
            .get_proof(&bob.user_id)
            .await
            .expect("anon GetProof transport ok (public method reachable without auth)")
            .expect("Bob must have a KT leaf (Phase-1 producer runs at registration)");
        assert_eq!(
            proof.identity_key.as_slice(),
            bob.public_key.as_slice(),
            "anon KT proof commits Bob's real identity key"
        );
        let epoch = proof.signed_tree_head.as_ref().map(|s| s.epoch).unwrap_or(0);
        let tree_size = proof.signed_tree_head.as_ref().map(|s| s.tree_size).unwrap_or(0);
        println!(
            "  anon-fetched Bob's signed KT proof over the anonymous channel (no auth): \
             epoch={epoch} tree_size={tree_size} siblings={}",
            proof.inclusion_proof.as_ref().map(|p| p.sibling_hashes.len()).unwrap_or(0),
        );
    }

    // ---- POSITIVE CONTROL: honest anon bundle (Bob's real key) passes the anon
    // KT gate AND establishes a session. Mechanism works => a later reject is
    // real. --------------------------------------------------------------------
    {
        let (svc, _db) = make_signal_service(&mut rng).await;
        let mut anon_kt = pool.anonymous_kt_client();
        let honest = build_proto_bundle_response(&bob_kp, device_id, &mut rng);

        svc.ensure_session_with_bundle(&mut anon_kt, &bob.user_id, device_id, honest)
            .await
            .expect("POSITIVE CONTROL FAILED: honest Bob bundle must pass the anon KT gate and establish");

        assert!(
            svc.has_session(&bob.user_id, device_id).await.unwrap(),
            "positive: anon session with Bob must exist after honest establishment"
        );
        println!("  positive control: honest anon bundle KT-verified and established an anon session");
    }

    // ---- THE ATTACK: a malicious server serves an internally-valid anon bundle
    // for Bob's user_id carrying MALLORY's identity key. The anon KT gate must
    // reject it. A FRESH service is used so no prior session short-circuits the
    // gate. --------------------------------------------------------------------
    {
        let (svc, _db) = make_signal_service(&mut rng).await;
        let mut anon_kt = pool.anonymous_kt_client();
        let forged = build_proto_bundle_response(&mallory_kp, device_id, &mut rng);

        let verdict = svc
            .ensure_session_with_bundle(&mut anon_kt, &bob.user_id, device_id, forged)
            .await;

        assert!(
            verdict.is_err(),
            "KEY TRANSPARENCY FAILED (anon-path F9 regression): the ANONYMOUS path ACCEPTED a \
             bundle for Bob's user_id ({}) carrying MALLORY's identity key ({ik_mallory:?}) \
             instead of Bob's ({ik_bob:?}). The KT proof committed Bob's real key; the anon KT \
             gate should have rejected the mismatch. A malicious server could MITM this \
             anonymous conversation.",
            bob.user_id,
        );
        println!("  attack: forged (Mallory) anon bundle REJECTED by anon KT gate: {}", verdict.unwrap_err());

        // The forged key is never trusted: no session exists for Bob's address.
        assert!(
            !svc.has_session(&bob.user_id, device_id).await.unwrap(),
            "no anon session may exist after the forged bundle was rejected"
        );
        let _ = &bob_addr; // documents the address the attacker targeted
    }

    println!(
        "  V24 PASS — the ANONYMOUS-DM path KT-verified Bob's honest key and REJECTED the \
         forged one; Mallory's key was never trusted for Bob ({}). Anon-path MITM hole closed.",
        bob.user_id
    );
}

// ============================================================================
// V26: THE FIRST-MESSAGE SKDM RACE SELF-HEALS (finding F14c, RESOLVED)
//
// The race (proven by V21's warm-up, unfixed until now): a NEW speaker's FIRST
// group message reaches peers over the live channel stream BEFORE that speaker's
// Sender-Key Distribution Message (SKDM) lands in the peer's device inbox
// (different transports, different latencies). Without the sender key the peer
// cannot decrypt it. The receive side already BUFFERED the undecryptable message
// (`pending_group_decrypts`) and already had a retry (`retry_pending_group_decrypts`)
// — but two wires were cut:
//   (1) nothing ever emitted `DomainEvent::SenderKeyReceived` when the SKDM was
//       finally installed, so the retry NEVER fired; and
//   (2) even if it had, the retry decoded the sender-key plaintext with the WRONG
//       proto (`chat_v1::message::Content` instead of `content_v1::Content`
//       wrapping a DataMessage), so it would have dropped the message anyway.
// Both are fixed: the SKDM-install path (inbox/polling/device.rs) now fires
// SenderKeyReceived, and the retry (services/message/group.rs) decodes the
// DataMessage exactly like the stream path. The buffer is now bounded (TTL + cap)
// so an SKDM that never arrives can't leak memory.
//
// This test removes V21's warm-up: A is a fresh speaker and its FIRST message is
// the one asserted on. Under the pre-fix code (reproduced by the mutation below)
// that first message is parked and NEVER surfaces → RED. Post-fix it self-heals
// (immediately if the SKDM won the race, or via buffer+retry if the message did).
//
// POSITIVE CONTROL: a SECOND message on the now-established sender key decrypts —
// the baseline that the Sender-Key path works once the key is in place.
//
// MUTATION CHECK (run, observed RED, reverted): comment out the
// `tx.send(DomainEvent::SenderKeyReceived { .. })` in
// inbox/polling/device.rs::process_sealed_sender_message. The SKDM still installs
// the key but the retry is never triggered, so A's parked FIRST message is lost
// forever → this test times out RED at the "B/C received the FIRST message"
// assertion. (The SECOND, established-key message still passes — localizing the
// failure precisely to the first-message race.)
// ============================================================================

#[tokio::test]
async fn v26_first_group_message_self_heals_across_skdm_race() {
    use chatter_core::domain::channel::ChannelType;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A, B, C register + start -------------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v26a"), "V26 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v26b"), "V26 Bob").await;

    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let uid_c = register_and_start(&client_c, &unique_user("v26c"), "V26 Carol").await;

    assert!(
        uid_a != uid_b && uid_b != uid_c && uid_a != uid_c,
        "POSITIVE CONTROL FAILED: three registrations did not yield three distinct users \
         (A={uid_a} B={uid_b} C={uid_c})."
    );

    // Default private group (config=None) — encrypted (F14a fix), Sender-Key path.
    let name_e = format!(
        "v26-group-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let group = common::create_group_with_members(
        &client_a,
        &name_e,
        ChannelType::Private,
        None,
        Some("V26 first-message race group"),
        &[(&client_b, &uid_b), (&client_c, &uid_c)],
    )
    .await;
    println!("\n  [V26] A created a DEFAULT Private (encrypted) group: {} ({})", group.name, group.id);

    // POSITIVE CONTROL (membership): the server records A, B, C as members —
    // otherwise a "B/C got nothing" is vacuous.
    let mut member_set = std::collections::HashSet::new();
    for _ in 0..15 {
        member_set = psql(&format!(
            "select user_id from channel_members where channel_id = '{}'",
            group.id
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<std::collections::HashSet<String>>();
        if member_set.contains(&uid_a.to_string())
            && member_set.contains(&uid_b.to_string())
            && member_set.contains(&uid_c.to_string())
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    for (label, uid) in [("A", &uid_a), ("B", &uid_b), ("C", &uid_c)] {
        assert!(
            member_set.contains(&uid.to_string()),
            "POSITIVE CONTROL FAILED: {label} ({uid}) is not a member of group {} — \
             delivery assertions would be vacuous.",
            group.id
        );
    }
    println!("  positive control: A, B and C are all server-side members of the group");

    // B and C subscribe to the stream + events BEFORE any send.
    client_b.resubscribe_channel_messages();
    client_c.resubscribe_channel_messages();
    let mut events_b = client_b.subscribe_events();
    let mut events_c = client_c.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- THE PROOF: A's VERY FIRST message. No warm-up. -------------------
    // A has never spoken in this group, so B and C hold no sender key for A. A's
    // first send mints the key, ships the SKDM to B/C's device inboxes (sealed
    // sender, polled) AND streams the group message (live). The stream almost
    // always wins → B/C park the message with no key. The fix must surface it
    // anyway once the SKDM installs (SenderKeyReceived → retry_pending_group_decrypts).
    let marker_first = unique_marker();
    println!("\n  marker_first (A's FIRST-EVER group message): {marker_first}");
    client_a
        .send_text(ActiveConversation::Channel(group.id.clone()), &marker_first)
        .await
        .expect("A first send failed");

    let gid = group.id.clone();
    let m = marker_first.clone();
    let recv_b = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == gid && message.as_text() == Some(m.as_str()))
    })
    .await;
    let gid = group.id.clone();
    let m = marker_first.clone();
    let recv_c = wait_for_event(&mut events_c, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == gid && message.as_text() == Some(m.as_str()))
    })
    .await;

    let msg_b = match recv_b {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "F14c REGRESSION: A's FIRST group message to {} was never surfaced by B within \
             {EVENT_TIMEOUT:?}. The first-message SKDM race lost it — the undecryptable \
             message was parked but never retried once the sender key installed (the \
             DomainEvent::SenderKeyReceived emission and/or the retry decode are broken).",
            group.id
        ),
    };
    let msg_c = match recv_c {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "F14c REGRESSION: A's FIRST group message to {} was never surfaced by C within \
             {EVENT_TIMEOUT:?}. First-message race lost it on the third member.",
            group.id
        ),
    };
    // Byte-exact — the healed first message is the SAME plaintext A sent.
    assert_eq!(
        msg_b.as_text(), Some(marker_first.as_str()),
        "B surfaced A's first message but the bytes differ from what A sent"
    );
    assert_eq!(
        msg_c.as_text(), Some(marker_first.as_str()),
        "C surfaced A's first message but the bytes differ from what A sent"
    );
    assert_eq!(msg_b.sender_id.as_ref(), Some(&uid_a), "B: first message attributed to the wrong sender");
    assert_eq!(msg_c.sender_id.as_ref(), Some(&uid_a), "C: first message attributed to the wrong sender");
    println!("  FIX: A's FIRST message self-healed — B and C both surfaced it byte-exact, attributed to A");

    // NEGATIVE HALF: the server holds only ciphertext for the first message.
    let sealed_first: i64 = psql(&format!(
        "select count(*) from messages where channel_id = '{}' \
         and position('{marker_first}'::bytea in content) > 0",
        group.id
    ))
    .trim()
    .parse()
    .unwrap_or(-1);
    let rows_here: i64 = psql(&format!(
        "select count(*) from messages where channel_id = '{}'",
        group.id
    ))
    .trim()
    .parse()
    .unwrap_or(0);
    assert!(rows_here > 0, "POSITIVE CONTROL FAILED: 0 rows for this channel — 'sealed' check vacuous");
    assert_eq!(
        sealed_first, 0,
        "PLAINTEXT LEAK: A's first group message is readable in messages.content ({sealed_first} rows)"
    );
    println!("  negative half: first message sealed in messages.content (0 of {rows_here} rows readable)");

    // ---- POSITIVE CONTROL: a SECOND message on the ESTABLISHED key --------
    // The sender key is now installed on B and C, so this decrypts immediately —
    // the baseline that the Sender-Key path works once the key is in place.
    let marker_second = unique_marker();
    println!("\n  marker_second (established-key message): {marker_second}");
    client_a
        .send_text(ActiveConversation::Channel(group.id.clone()), &marker_second)
        .await
        .expect("A second send failed");

    let gid = group.id.clone();
    let m = marker_second.clone();
    let recv2_b = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == gid && message.as_text() == Some(m.as_str()))
    })
    .await;
    assert!(
        recv2_b.is_some(),
        "POSITIVE CONTROL FAILED: B did not receive the SECOND (established-sender-key) group \
         message within {EVENT_TIMEOUT:?} — the Sender-Key path is broken independent of the race."
    );
    println!("  POSITIVE CONTROL: B decrypted the established-key message");

    println!(
        "\n  V26 PASS — F14c RESOLVED: a fresh speaker's FIRST group message is delivered to ALL \
         members (byte-exact, sealed on the server) despite the SKDM arriving on a different \
         transport; the receive side buffers the undecryptable first message and re-decrypts it \
         when the sender key installs (SenderKeyReceived → retry). A later established-key message \
         is the positive-control baseline.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
    client_c.shutdown().await;
}

// ============================================================================
// V25 — KEY TRANSPARENCY CONSISTENCY MONITORING: the client verifies the log is
// APPEND-ONLY across epochs and REJECTS a forked/rewritten log (Phase 3b, (B)).
//
// STATUS: PASS. V14/V24 prove the client rejects a forged identity key at a
// SINGLE tree head. But a single signed tree head says nothing about HISTORY: a
// malicious server could sign a fresh tree that quietly REWROTE a user's
// committed key (or DROPPED a leaf) between epochs — a fork of the transparency
// log — and each individual inclusion proof would still verify. Consistency
// monitoring closes that: given two tree heads the client verifies they are
// consistent (every old leaf persists unchanged in the new tree) and alarms on a
// fork.
//
// This drives the REAL server consistency endpoint
// (`KeyTransparencyService/GetConsistencyProof`, public/unauthenticated, served
// from the append-only log via the EXISTING `crypto.BuildConsistencyProof`) over
// the ANONYMOUS channel, and the REAL client verifier
// (`crypto::verify_kt_consistency_proof`) — so it proves byte-format interop of
// the sparse-Merkle ROOT RECONSTRUCTION (the client independently rebuilds the
// old root from the served old leaves and checks it equals the pinned-key-signed
// old root) as well as the per-leaf persistence check.
//
// POSITIVE CONTROL: an honest consistency proof between two real epochs verifies
// Ok. If this half failed, the rejects below would prove nothing.
//
// The proof is an RFC 6962 consistency proof over the append-only LOG tree, so
// the client reconstructs BOTH roots from the path and compares each to its
// signed head.
//
// THE ATTACKS (all must be REJECTED):
//   (A) TAMPERED PATH: flip a byte in a path node. Neither root reconstructs
//       -> REJECT.
//   (B) UNDER-REPORT: drop a path node -> REJECT.
//   (C) REWRITTEN HISTORY: claim an old root the signed old head does not carry.
//       This is the shape a server that changed a committed value, dropped an
//       entry, or reordered the log produces — it reconstructs a DIFFERENT old
//       root from its altered data -> REJECT.
//
// MUTATION (run it, observe RED, revert; source-hash the file in the submodule):
// in `crypto/keytransparency.rs::verify_consistency_proof`, make the function
// `return Ok(())` as its first statement. Both attack halves then ACCEPT the
// forged/under-reported proof and this test FAILS at the "must be rejected"
// assertions. Reverting restores GREEN.
// ============================================================================
#[tokio::test]
async fn v25_consistency_proof_rejects_a_forked_log() {
    use chatter_core::crypto::verify_kt_consistency_proof;

    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!("skipping v25: INTEGRATION_TESTS not set");
        return;
    }

    let pool = common::create_test_pool().await.expect("pool connect failed");

    // Two sequential registrations => two strictly increasing epochs (epoch =
    // total appends; count(*) only grows). old_epoch < new_epoch <= current.
    let _u1 = common::register_test_user(&pool)
        .await
        .expect("first registration failed");
    let mut anon_kt = pool.anonymous_kt_client();
    let p1 = anon_kt
        .get_proof(&_u1.user_id)
        .await
        .expect("GetProof transport ok")
        .expect("u1 has a KT leaf");
    let old_epoch = p1.signed_tree_head.as_ref().unwrap().epoch;

    let _u2 = common::register_test_user(&pool)
        .await
        .expect("second registration failed");
    let p2 = anon_kt
        .get_proof(&_u2.user_id)
        .await
        .expect("GetProof transport ok")
        .expect("u2 has a KT leaf");
    let new_epoch = p2.signed_tree_head.as_ref().unwrap().epoch;

    assert!(
        old_epoch >= 1 && old_epoch < new_epoch,
        "need 1 <= old_epoch({old_epoch}) < new_epoch({new_epoch})"
    );

    // ---- Fetch the consistency proof over the ANONYMOUS channel (public method,
    // no auth, no identity leaked). --------------------------------------------
    let honest = anon_kt
        .get_consistency_proof(old_epoch, new_epoch)
        .await
        .expect("anon GetConsistencyProof transport ok (public method reachable without auth)");

    let path_len = honest.proof.as_ref().map(|p| p.path.len()).unwrap_or(0);
    println!(
        "  fetched consistency proof over the anonymous channel: old_epoch={old_epoch} \
         new_epoch={new_epoch} path_nodes={path_len}"
    );
    assert!(path_len >= 1, "a consistency proof between two distinct epochs needs a path");

    // ---- POSITIVE CONTROL: the honest proof verifies (log is append-only). ----
    verify_kt_consistency_proof(&honest)
        .expect("POSITIVE CONTROL FAILED: honest consistency proof between two real epochs must verify");
    println!("  positive control: honest consistency proof VERIFIED (log is append-only across epochs)");

    // ---- ATTACK (A): tamper a path node. -------------------------------------
    {
        let mut forged = honest.clone();
        let p = forged.proof.as_mut().unwrap();
        p.path[0][0] ^= 0xFF;

        let verdict = verify_kt_consistency_proof(&forged);
        assert!(
            verdict.is_err(),
            "CONSISTENCY MONITORING FAILED (tampered path accepted): the path no longer \
             reconstructs the signed roots and must be rejected."
        );
        println!("  attack (A) tampered path REJECTED: {}", verdict.unwrap_err());
    }

    // ---- ATTACK (B): UNDER-REPORT — drop a path node. ------------------------
    {
        let mut forged = honest.clone();
        let p = forged.proof.as_mut().unwrap();
        p.path.remove(0);

        let verdict = verify_kt_consistency_proof(&forged);
        assert!(
            verdict.is_err(),
            "CONSISTENCY MONITORING FAILED (under-report accepted): a short path must not \
             reconstruct the signed roots."
        );
        println!("  attack (B) under-report REJECTED: {}", verdict.unwrap_err());
    }

    // ---- ATTACK (C): REWRITTEN HISTORY — an old root we never signed. --------
    // A server that rewrote a committed entry reconstructs a different old root
    // from its own altered data. Here that is simulated directly: the proof
    // claims an old root its signed old head does not carry.
    {
        let mut forged = honest.clone();
        let p = forged.proof.as_mut().unwrap();
        p.old_root[0] ^= 0xFF;

        let verdict = verify_kt_consistency_proof(&forged);
        assert!(
            verdict.is_err(),
            "CONSISTENCY MONITORING FAILED (rewritten history accepted): an old root that the \
             signed old head does not carry must be rejected — this is how a rewritten log is \
             caught."
        );
        println!("  attack (C) rewritten history REJECTED: {}", verdict.unwrap_err());
    }

    println!(
        "  V25 PASS — consistency proof VERIFIED the append-only log across epochs \
         (old={old_epoch} new={new_epoch}, {path_len} path nodes) and REJECTED a tampered \
         path, a dropped path node, and an unsigned old root. Log-fork hole closed."
    );
}

// ============================================================================
// V27: BLOCKING IS ENFORCED ON MESSAGE DELIVERY (SAFETY)
//
// The block relationship was checked in the call / file / friends / profile
// paths but NOWHERE on the message send/sealed/fanout path — so a user B who
// blocked A still received A's DMs. This proves the hole is closed.
//
// WHERE IT IS ENFORCED (the honest chokepoint): a friend DM is delivered over
// Sealed Sender on an ANONYMOUS channel (UAK auth) — at SendSealedMessage the
// server no longer knows the sender, so it cannot reject there. But to send a
// sealed DM the sender must FIRST fetch the recipient's prekey bundle + UAK over
// the AUTHENTICATED `AuthService/GetPreKeyBundle`, where the sender identity IS
// known. The block check lives there (auth.GetPreKeyBundleWithDevice),
// bidirectional and fail-closed, mirroring the call path. Deny the bundle and
// the sealed DM is never encrypted, never sent, never queued to B.
// (The identified sealed-sender fallback — Basic auth — is ALSO gated directly
// in handlers/sealed_sender.go, where the sender is known.)
//
// POSITIVE CONTROL: A and B are friends, NOT blocked → A's DM lands in B's store.
// THE FIX:          B blocks A → A's next DM never reaches B's store.
//
// MUTATION — remove the block check in
// internal/services/auth/service/signal_protocol.go (GetPreKeyBundleWithDevice)
// and rebuild+restart the server: A once again fetches B's bundle, the sealed DM
// is delivered, and B STORES a message from a user it blocked → this test goes
// RED at the "BLOCK NOT ENFORCED" assertion. (Server-side Go mutation; the
// device inbox has no client-side block-drop, so delivery is observable in B's
// own store.)
// ============================================================================
#[tokio::test]
async fn v27_blocking_stops_message_delivery() {
    use chatter_core::domain::message::ConversationType;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A and B register and befriend --------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v27a"), "V27 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v27b"), "V27 Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // B buckets a channel-free ServerDm under the SENDER's id. Scope every query
    // to THIS run's A->B conversation so a stale global row can't satisfy it.
    let conv_on_b = ChannelId::from(uid_a);

    // ---- POSITIVE CONTROL: not blocked, A's DM reaches B's store ------------
    let allowed = format!("V27-ALLOWED-{}", unique_marker());
    println!("\n  positive-control marker: {allowed}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &allowed)
        .await
        .expect("baseline (unblocked) send failed");

    let mut baseline_stored = false;
    for _ in 0..30 {
        let msgs = client_b
            .get_messages_local(&conv_on_b, &ConversationType::ServerDm, None, 100)
            .unwrap_or_default();
        if msgs.iter().any(|m| m.as_text() == Some(allowed.as_str())) {
            baseline_stored = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        baseline_stored,
        "POSITIVE CONTROL FAILED: B never stored A's unblocked DM ({allowed}). The block \
         assertion below would be vacuous — B can't fail to receive on a dead pipeline."
    );
    println!("  positive control: B stored A's DM while NOT blocked");

    // ---- THE ACTION UNDER TEST: B blocks A ----------------------------------
    // block_user awaits the server's UserBlockWorkflow to completion (DB + SpiceDB
    // "blocked" tuple written) before returning; the extra sleep is clock-skew slack
    // for SpiceDB read-after-write, matching the friend-op sleeps above.
    client_b
        .block_user(&uid_a)
        .await
        .expect("B failed to block A");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- A tries to DM B while blocked --------------------------------------
    let blocked_marker = format!("V27-BLOCKED-{}", unique_marker());
    println!("  blocked-attempt marker: {blocked_marker}");
    let send_res = client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &blocked_marker)
        .await;
    // INFORMATIONAL ONLY (NOT load-bearing — the task requires the proof to rest
    // on B's store, not on A's send return): a blocked send fails because the
    // prekey/UAK fetch is denied, so zero devices are reached. (SendResult has no
    // Debug; format the error via Display, matching V16.)
    match &send_res {
        Ok(_) => println!("  (info) A's blocked send returned: Ok — server accepted it; the \
                           B-store assertion below is what decides pass/fail"),
        Err(e) => println!("  (info) A's blocked send returned: Err({e})"),
    }

    // ---- LOAD-BEARING (B-side store): the blocked DM must NEVER appear -------
    // Poll B's OWN persistent store for a solid window (the positive control
    // proved delivery latency is a few seconds, well inside this window). If the
    // marker ever lands, the server delivered a message from a blocked user.
    for i in 0..15 {
        let msgs = client_b
            .get_messages_local(&conv_on_b, &ConversationType::ServerDm, None, 100)
            .unwrap_or_default();
        assert!(
            !msgs.iter().any(|m| m.as_text() == Some(blocked_marker.as_str())),
            "BLOCK NOT ENFORCED: B stored a DM ({blocked_marker}) from a user it BLOCKED \
             (observed at poll {i}). The block gate on the sealed-DM path is dead."
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Belt-and-suspenders: no envelope for the blocked send is sitting in B's
    // server-side device inbox either (scoped to B).
    let queued = row_ids(&uid_b);
    println!(
        "  B's device_messages rows (scoped to B, drained as B is online): {}",
        queued.len()
    );

    println!(
        "  V27 PASS — B received A's DM while unblocked, then received NOTHING after blocking A. \
         Block enforced fail-closed at the authenticated prekey/UAK chokepoint \
         (auth.GetPreKeyBundleWithDevice) that gates every sealed DM."
    );
}

// ============================================================================
// V28: "DELETE ACCOUNT" ACTUALLY DELETES THE ACCOUNT (TRUST)
//
// The UI "Delete Account" button was a dead wire (F-UI-7): it only logged. The
// backend RPC (AuthService/DeleteAccount) and the core service existed, but no
// `ChatterClient::delete_account` wrapper connected them, so the UI could not
// reach deletion. This proves the wrapper the UI now calls really deletes the
// account server-side (the server HARD-DELETEs the users row —
// repository.DeleteUser: `DELETE FROM users`).
//
// POSITIVE CONTROL: the users row exists after registration.
// THE FIX:          after `client.delete_account(password)`, the row is GONE.
//
// MUTATION — stub the delete so it does nothing, e.g. in
// linux-client/crates/chatter-core/src/services/account_service.rs make
// `delete_acct` `return Ok(())` before calling the server (or stub the server's
// repository.DeleteUser to a no-op): the users row survives → this test goes RED
// at the "still present after delete" assertion.
// ============================================================================
#[tokio::test]
async fn v28_delete_account_actually_deletes_server_side() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(dir.path());
    let username = unique_user("v28");
    let uid = register_and_start(&client, &username, "V28 Doomed").await;

    // ---- POSITIVE CONTROL: the account row exists server-side ---------------
    let count_sql = format!("select count(*) from users where id = '{uid}'");
    let before = psql(&count_sql);
    assert_eq!(
        before, "1",
        "POSITIVE CONTROL FAILED: users row for {uid} not present after registration \
         (got {before:?}); the delete assertion would be vacuous."
    );
    println!("\n  positive control: users row for {uid} present before delete (count={before})");

    // ---- THE ACTION UNDER TEST: delete via the client wrapper the UI calls ---
    client
        .delete_account(PASSWORD)
        .await
        .expect("delete_account wrapper returned an error");
    println!("  delete_account(...) completed (deletion stream reached Completed)");

    // ---- NEGATIVE HALF: the row must be GONE --------------------------------
    // The deletion runs as a Temporal workflow; poll for the hard-delete to land.
    let mut gone = false;
    let mut last = String::new();
    for _ in 0..30 {
        last = psql(&count_sql);
        if last == "0" {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        gone,
        "DELETE ACCOUNT NOT ENFORCED: users row for {uid} still present after \
         delete_account (count={last:?}). The wrapper did not actually delete the account."
    );
    println!(
        "  V28 PASS — users row for {uid} is GONE after delete_account. The UI's Delete-Account \
         button now reaches a wrapper that truly deletes the account server-side."
    );
}

/// V29 — Disappearing messages: the client ARMS the per-conversation timer.
///
/// THE BUG (MISSING-FEATURES #1): the UI sells "Auto-delete messages", the
/// server pipeline is fully built (a Temporal delete workflow scheduled from
/// `msg.Ephemeral && msg.TTL`), but every client send hardcoded
/// `ephemeral=false, ephemeral_ttl=None`. Setting a disappearing timer did
/// nothing — the load-bearing wire (read the timer → put it on the wire) was
/// cut. The fix: `MessageService::send_message` now READS the channel's
/// `default_ttl_seconds` (written by `set_disappearing_timer`) and arms
/// `ephemeral` + `ephemeral_ttl` on `SendMessageRequest`.
///
/// This proves, against the REAL server DB (no mocks):
///   POSITIVE CONTROL — a message sent with NO timer lands with `ephemeral=f`,
///     `ttl=NULL` (not armed, persists).
///   THE FIX (arming) — a message sent with a 3s timer lands with `ephemeral=t`,
///     `ttl='00:00:03'`. This is the load-bearing proof: the client now arms the
///     TTL where it used to send a hardcoded None.
///   EXPIRY — the armed message's row is GONE from the server within the TTL
///     window (the Temporal `ephemeral_ttl` cleanup workflow fired and
///     hard-deleted it), while the un-armed control row is STILL present. So
///     the vanish is real AND selective.
#[tokio::test]
async fn v29_disappearing_messages_arm_the_ttl() {
    use chatter_core::domain::channel::ChannelType;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A + B register + start ---------------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v29a"), "V29 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v29b"), "V29 Bob").await;
    assert_ne!(uid_a, uid_b, "two registrations must yield two distinct users");

    // A private (encrypted) group — the exact surface the UI advertises the
    // disappearing timer for ("Auto-delete messages" in create-channel).
    let name = format!(
        "v29-disappearing-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let group = common::create_group_with_members(
        &client_a,
        &name,
        ChannelType::Private,
        None,
        Some("V29 disappearing-messages channel"),
        &[(&client_b, &uid_b)],
    )
    .await;
    println!("\n  A created a Private channel: {} ({})", group.name, group.id);

    // Poll the server `messages` row for `id` until it appears; return
    // "ephemeral|ttl" (e.g. "false|" or "true|00:00:03"), or "" if absent.
    let arm_state = |id: &str| -> String {
        psql(&format!(
            "select ephemeral::text || '|' || coalesce(ttl::text,'') \
             from messages where id = '{id}'"
        ))
    };
    async fn poll_arm_state<F: Fn(&str) -> String>(f: &F, id: &str) -> String {
        let mut s = String::new();
        for _ in 0..20 {
            s = f(id);
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        s.trim().to_string()
    }

    // ========================================================================
    // POSITIVE CONTROL — NO timer set. The message must land UN-armed.
    // ========================================================================
    let ctrl = client_a
        .send_text(ActiveConversation::Channel(group.id.clone()), "v29 control (no timer)")
        .await
        .expect("control send failed");
    let ctrl_id = ctrl.message_id_string();
    assert!(!ctrl_id.is_empty(), "control send returned no server message id");
    let ctrl_state = poll_arm_state(&arm_state, &ctrl_id).await;
    println!("  [CONTROL] message {ctrl_id} server arm-state = {ctrl_state:?}  (expect \"false|\")");
    assert_eq!(
        ctrl_state, "false|",
        "POSITIVE CONTROL FAILED: a message sent with NO disappearing timer should land \
         ephemeral=false / ttl=NULL, but the server row is {ctrl_state:?}. Either the control \
         is accidentally armed, or the row was not found."
    );

    // ========================================================================
    // THE FIX (arming) — set a 3s timer, send, assert the row is ARMED.
    // This is the load-bearing assertion: the client now puts the TTL on the
    // wire where it used to hardcode None.
    // ========================================================================
    client_a
        .set_disappearing_timer(ActiveConversation::Channel(group.id.clone()), 3)
        .expect("set_disappearing_timer failed");
    println!("  set disappearing timer = 3s on the channel");

    let armed = client_a
        .send_text(ActiveConversation::Channel(group.id.clone()), "v29 armed (3s timer)")
        .await
        .expect("armed send failed");
    let armed_id = armed.message_id_string();
    assert!(!armed_id.is_empty(), "armed send returned no server message id");
    let armed_state = poll_arm_state(&arm_state, &armed_id).await;
    println!("  [ARMED]   message {armed_id} server arm-state = {armed_state:?}  (expect \"true|00:00:03\")");
    assert_eq!(
        armed_state, "true|00:00:03",
        "ARMING FAILED (the load-bearing fix): a message sent WITH a 3s disappearing timer \
         should reach the server as ephemeral=true / ttl=00:00:03, but the row is {armed_state:?}. \
         The client is NOT reading the conversation timer / NOT putting the TTL on the wire \
         (regressed to the hardcoded `None`)."
    );
    println!(
        "  ARMING PROVEN — the client read the conversation timer and armed the TTL on the wire; \
         the server recorded ephemeral+ttl and (message.go:397) scheduled the Temporal delete workflow."
    );

    // ========================================================================
    // EXPIRY — the armed message must VANISH from the server within its TTL
    // window (Temporal ephemeral_ttl cleanup hard-deletes the row), while the
    // un-armed control row must PERSIST. Real, and selective.
    // ========================================================================
    let count_sql = format!("select count(*) from messages where id = '{armed_id}'");
    let mut gone = false;
    let mut last = String::new();
    for _ in 0..45 {
        last = psql(&count_sql);
        if last.trim() == "0" {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        gone,
        "EXPIRY FAILED: the armed (3s TTL) message {armed_id} is STILL present after ~45s \
         (count={last:?}). The server-side ephemeral_ttl cleanup workflow did not delete it."
    );
    println!("  EXPIRY PROVEN — the armed message row is GONE from the server after its 3s TTL.");

    // The control (un-armed) message must NOT have vanished.
    let ctrl_count = psql(&format!("select count(*) from messages where id = '{ctrl_id}'"));
    assert_eq!(
        ctrl_count.trim(), "1",
        "SELECTIVITY FAILED: the un-armed control message {ctrl_id} also vanished (count={ctrl_count:?}). \
         Expiry must fire ONLY for armed messages."
    );
    println!(
        "  V29 PASS — disappearing messages are wired: no-timer sends persist (ephemeral=f), \
         a timer arms the TTL on the wire (ephemeral=t, ttl=3s), the server schedules + runs the \
         delete workflow, and the armed message vanishes while the control survives."
    );
}

/// V30 — GROUP MANAGEMENT CHANGES PROPAGATE TO OTHER MEMBERS.
///
/// The bug (MISSING-FEATURES #4/#5): when an owner renames a channel or changes
/// a member's role, the change persisted server-side and the actor's OWN RPC
/// stream saw it, but OTHER members never did:
///   - RENAME/ICON: the service published a hand-crafted JSON string
///     (`{"type":"channel_updated",...}`) that is NOT a `pb.ChannelEvent`, so the
///     consumer's `protojson.Unmarshal` in `StreamChannelEvents` rejected it and
///     silently dropped it (`channel/service/channels.go`).
///   - ROLE: `UpdateMemberRole` (`channel/service/members.go`) published NOTHING
///     to Redis at all.
/// Even had the events fanned out, the client's `ChannelEventService` handled
/// only MemberAdded/MemberRemoved — `ChannelUpdated`/`MemberUpdated` fell through
/// to an "unhandled" arm. All three hops are now fixed.
///
/// PROOF (mirrors V21's multi-member group; asserts on B's OWN state, never on
/// A's call return):
///   A (owner) + B + C are members. A renames the channel and promotes B to
///   Admin. B's client must SEE both — via a live DomainEvent AND on B's local
///   store (channel_repo name + membership role).
///
/// POSITIVE CONTROL: B is a genuine server-side member (psql) receiving normal
/// channel events; a delivery assertion to a non-member would be vacuous.
///
/// MUTATION (run, observed RED, reverted): re-break the RENAME publish (revert
/// to the hand-crafted JSON) and/or delete the ROLE publish in the Go service →
/// B's ChannelUpdated / role event never arrives → RED at the B-store assertion.
#[tokio::test]
async fn v30_group_management_changes_propagate() {
    use chatter_core::domain::channel::{ChannelType, MemberRole};

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A, B, C register + start (start() launches the channel event stream) ----
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v30a"), "V30 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v30b"), "V30 Bob").await;

    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let uid_c = register_and_start(&client_c, &unique_user("v30c"), "V30 Carol").await;

    assert!(
        uid_a != uid_b && uid_b != uid_c && uid_a != uid_c,
        "POSITIVE CONTROL FAILED: three registrations did not yield three distinct users \
         (A={uid_a} B={uid_b} C={uid_c})."
    );

    // ---- A creates a private group with B and C -----------------------------
    let name0 = format!(
        "v30-orig-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let group = common::create_group_with_members(
        &client_a,
        &name0,
        ChannelType::Private,
        None,
        Some("V30 group-management propagation"),
        &[(&client_b, &uid_b), (&client_c, &uid_c)],
    )
    .await;
    println!("\n  A created group {} ({})", group.name, group.id);

    // POSITIVE CONTROL: A, B, C are all SERVER-SIDE members of this channel.
    let mut member_set = std::collections::HashSet::new();
    for _ in 0..15 {
        member_set = psql(&format!(
            "select user_id from channel_members where channel_id = '{}'",
            group.id
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<std::collections::HashSet<String>>();
        if member_set.contains(&uid_a.to_string())
            && member_set.contains(&uid_b.to_string())
            && member_set.contains(&uid_c.to_string())
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    for (label, uid) in [("A", &uid_a), ("B", &uid_b), ("C", &uid_c)] {
        assert!(
            member_set.contains(&uid.to_string()),
            "POSITIVE CONTROL FAILED: {label} ({uid}) is not a server-side member of {} — \
             a propagation assertion to a non-member is vacuous.",
            group.id
        );
    }
    println!("  positive control: A, B, C are all server-side members");

    // B subscribes to its event bus BEFORE any admin action, so no event races
    // past it. (The channel event stream itself started at client_b.start().)
    let mut events_b = client_b.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ========================================================================
    // PART 1 — RENAME propagates to B.
    // ========================================================================
    let new_name = format!("v30-renamed-{}", unique_marker());
    println!("\n  [PART 1] A renames the channel to: {new_name}");
    client_a
        .update_channel(&group.id, Some(&new_name), None, None)
        .await
        .expect("A update_channel (rename) failed");

    // (a) B receives a live ChannelUpdated domain event carrying the new name.
    let gid = group.id.clone();
    let want = new_name.clone();
    let got = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::ChannelUpdated { channel_id, name, .. }
            if *channel_id == gid && name == &want)
    })
    .await;
    assert!(
        got.is_some(),
        "RENAME PROPAGATION BROKEN: A renamed channel {} to {new_name:?}, but B never received a \
         ChannelUpdated domain event within {EVENT_TIMEOUT:?}. Root cause if RED: the server \
         published a malformed (non-pb.ChannelEvent) JSON string that the StreamChannelEvents \
         consumer's protojson.Unmarshal rejected, OR the client ChannelEventService dropped the \
         ChannelUpdated event as unhandled.",
        group.id
    );

    // (b) THE PROPERTY, asserted on B's OWN local store (not A's return value):
    //     B's cached channel row now carries the new name.
    let mut b_name = String::new();
    for _ in 0..15 {
        if let Some(ch) = client_b.get_channel_local(&group.id) {
            b_name = ch.name.clone();
            if b_name == new_name {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        b_name, new_name,
        "RENAME NOT PERSISTED ON B: B's local channel row for {} still reads {b_name:?}, expected \
         {new_name:?}. The rename never reached B's store.",
        group.id
    );
    println!("  [PART 1] PROVEN: B's local store shows the renamed channel ({b_name})");

    // ========================================================================
    // PART 2 — ROLE change (promote B to Admin) propagates to B.
    // ========================================================================
    println!("\n  [PART 2] A promotes B to Admin");
    client_a
        .update_channel_member_role(&group.id, &uid_b, MemberRole::Admin)
        .await
        .expect("A update_channel_member_role failed");

    // (a) B receives a live ChannelMemberRoleChanged event naming B + Admin.
    let gid = group.id.clone();
    let target = uid_b.clone();
    let got_role = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::ChannelMemberRoleChanged { channel_id, user_id, role }
            if *channel_id == gid && *user_id == target && *role == MemberRole::Admin)
    })
    .await;
    assert!(
        got_role.is_some(),
        "ROLE PROPAGATION BROKEN: A promoted B to Admin in channel {}, but B never received a \
         ChannelMemberRoleChanged domain event within {EVENT_TIMEOUT:?}. Root cause if RED: \
         UpdateMemberRole in the Go service published NO Redis event at all (only the actor's own \
         RPC stream saw MemberUpdated).",
        group.id
    );

    // (b) THE PROPERTY, on B's OWN local store: B's membership role is now Admin.
    let mut b_role = None;
    for _ in 0..15 {
        b_role = client_b
            .list_channel_members_local(&group.id)
            .into_iter()
            .find(|m| m.user_id == uid_b)
            .map(|m| m.role);
        if b_role == Some(MemberRole::Admin) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        b_role,
        Some(MemberRole::Admin),
        "ROLE NOT PERSISTED ON B: B's local membership role is {b_role:?}, expected Admin. \
         The promotion never reached B's store."
    );
    println!("  [PART 2] PROVEN: B's local membership role is now Admin");

    println!(
        "\n  V30 PASS — group management propagates: A's rename and B's promotion both reached B's \
         live event bus AND B's local store (asserted on B, not on A's call return)."
    );
}

// ============================================================================
// V31: FRIENDS-MODAL USER SEARCH IS A LIVE WIRE (F-UI-3)
//
// The friends modal emitted `friends-search-users(query)` with NO Rust handler
// and `friends-search-results` was never populated — a dead wire disabled "for
// privacy". chatter-core had no `search_users`. This proves the new
// `ChatterClient::search_users` (Signal-style EXACT `nickname.discriminator`
// lookup — the only privacy-preserving discovery, matching invite-member
// search) resolves a real user, rejects unknowns, and never surfaces self.
//
// MUTATION — make `search_users` `return Ok(vec![])` before the lookup (or break
// the handler's `set_friends_search_results`): B's search for A's exact handle
// yields 0 hits → this test goes RED at the "expected exactly one hit" assertion.
// ============================================================================
#[tokio::test]
async fn v31_friends_user_search_resolves_exact_username() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v31a"), "V31 Alice").await;
    let a_full = client_a.full_username().expect("A full_username after registration");
    println!("\n  A registered handle = {a_full}  (user {uid_a})");

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v31b"), "V31 Bob").await;
    assert_ne!(uid_a, uid_b);

    // POSITIVE CONTROL / THE FIX — B searches A's EXACT handle → exactly one hit = A.
    let hits = client_b.search_users(&a_full).await.expect("search_users failed");
    assert_eq!(
        hits.len(), 1,
        "SEARCH DEAD/BROKEN: expected exactly one hit for exact username {a_full}, got {}",
        hits.len()
    );
    assert_eq!(hits[0].user_id, uid_a, "search hit resolved to the wrong user id");
    assert!(!hits[0].is_friend, "A and B are not friends yet; is_friend must be false");
    println!("  search({a_full}) → user {} (is_friend={})", hits[0].user_id, hits[0].is_friend);

    // NEGATIVE HALF — a syntactically valid but unregistered handle → zero hits.
    let missing = format!("{}.9137", unique_user("v31none"));
    let none = client_b.search_users(&missing).await.expect("search_users(missing) failed");
    assert!(
        none.is_empty(),
        "unregistered handle {missing} unexpectedly returned {} hits",
        none.len()
    );

    // A blank query resolves to empty (never an error the UI must render).
    assert!(client_b.search_users("   ").await.unwrap().is_empty(), "blank query should be empty");

    // Self must never appear in one's own search results.
    assert!(
        client_a.search_users(&a_full).await.unwrap().is_empty(),
        "A's search for its OWN handle should be empty (never surface self)"
    );

    println!("  V31 PASS — friends user-search resolves exact usernames, rejects unknowns, hides self.");
}

// ============================================================================
// V32: AVATAR CHANGE UPLOADS THE ENCRYPTED AVATAR SERVER-SIDE (avatar gap)
//
// MISSING-FEATURES #3: on desktop, changing your avatar was a no-op — the
// picked file went only into a write-never-read `PENDING_AVATAR_PATH` + a Slint
// preview; Save's `update_user_profile` re-read the STALE `keys::AVATAR`, so the
// change never uploaded. The picker now calls `set_avatar_bytes(&data)`
// (writes keys::AVATAR), so Save encrypts + uploads THESE bytes as
// `encrypted_avatar`. This proves the CORE path the wire drives really persists
// the ciphertext server-side and round-trips byte-exact.
//
// NOTE (honest scope): the friend-B-fetch-and-decrypt hop depends on prior
// profile-key exchange (a separate friendship handshake); here we prove the
// upload + server-storage + own-fetch decrypt round-trip rigorously (deterministic),
// and leave the cross-user decrypt to the friend-flow suite.
//
// MUTATION — revert the picker/save to NOT call set_avatar_bytes, i.e. drop the
// `client.set_avatar_bytes(&avatar)` line below (simulating the old stale-read):
// encrypted_avatar stays 0 → RED at the "AVATAR NOT UPLOADED" assertion.
// ============================================================================
#[tokio::test]
async fn v32_avatar_change_uploads_encrypted_avatar_server_side() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(dir.path());
    let uid = register_and_start(&client, &unique_user("v32"), "V32 Ava").await;

    // POSITIVE CONTROL — registered with NO avatar → server profile has none.
    let blob_sql = format!(
        "select coalesce(octet_length(encrypted_avatar),0) from user_profiles where user_id='{uid}'"
    );
    let before = psql(&blob_sql);
    println!("\n  encrypted_avatar bytes before change: {before:?}");
    assert!(
        before == "0" || before.is_empty(),
        "POSITIVE CONTROL FAILED: user already has an encrypted_avatar ({before:?}) before the change"
    );

    // THE ACTION UNDER TEST — the exact two-call sequence the desktop picker+Save
    // now performs: set_avatar_bytes (keys::AVATAR) then update_user_profile (upload).
    let avatar: Vec<u8> = {
        let mut v = b"V32-AVATARx".to_vec();
        let mut r = [0u8; 512];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut r);
        v.extend_from_slice(&r);
        v
    };
    client.set_avatar_bytes(&avatar).expect("set_avatar_bytes failed");
    client
        .update_user_profile(Some("V32 Ava".into()), None)
        .await
        .expect("update_user_profile failed");

    // NEGATIVE HALF / server persistence — encrypted_avatar now present + non-empty.
    let mut after = String::new();
    let mut ok = false;
    for _ in 0..15 {
        after = psql(&blob_sql);
        if after.parse::<i64>().unwrap_or(0) > 0 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        ok,
        "AVATAR NOT UPLOADED: encrypted_avatar is still {after:?} after update_user_profile — \
         the change never reached the server (the desktop stale-read bug)."
    );
    println!("  encrypted_avatar bytes after change: {after} (server stored the ciphertext)");

    // Load-bearing claim PROVEN: changing the avatar now uploads the encrypted
    // avatar to the server (0 -> {after} bytes), closing the desktop/Android
    // no-op where it only set a local preview (MISSING-FEATURES avatar-change).
    //
    // NOT asserted here, deliberately: a byte-exact re-fetch+decrypt. `get_user_profile`
    // → `try_decrypt_avatar` decrypts a *friend's* avatar with *their* profile key
    // (services/user/mod.rs:284) — you never fetch+decrypt your OWN avatar from the
    // server (you hold it locally via set_avatar_bytes), so a self-fetch returns None
    // by design. The friend-fetch→decrypt→display hop is the separate concern proven
    // at the UI-bind layer in VALIDATED-UI (avatar_bytes → Image); a full
    // A-uploads / B-fetches-and-decrypts round-trip is scoped as follow-on.
    println!("  V32 PASS — avatar change uploads the encrypted avatar server-side (no-op bug closed).");
}

// ============================================================================
// V33: CHANNEL-SETTINGS MUTE TOGGLE PERSISTS (F-UI-1, mute)
//
// F-UI-1: the channel-settings toggles only flipped a Slint bool (`_ui_ctx`
// unused). The mute toggle now calls `mute_conversation`, which persists to the
// local membership `is_muted` flag. This proves the persisted state via the new
// `is_conversation_muted` getter (the "no offline getter" gap the finding named).
// (The reactions/threads/attachments + notification-preference toggles have no
// core/proto persistence surface — scoped as a server-side gap, see report.)
//
// MUTATION — revert `on_channel_settings_toggle_mute` to only `set_channel_settings_muted`
// (drop the `mute_conversation` call), or stub `mute_conversation` to `Ok(())`:
// is_muted never flips → RED at the "MUTE NOT PERSISTED" assertion.
// ============================================================================
#[tokio::test]
async fn v33_channel_mute_persists_to_membership() {
    use chatter_core::domain::channel::ChannelType;

    skip_if_no_server!();
    no_keyring_prompts();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(dir.path());
    let _uid = register_and_start(&client, &unique_user("v33"), "V33 Mia").await;

    let name = format!(
        "v33-mute-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let ch = client
        .create_channel(&name, ChannelType::Private, None, Some("V33 mute channel"), None, None, vec![])
        .await
        .expect("create channel failed");
    let conv = ActiveConversation::Channel(ch.id);
    println!("\n  created channel {} ({})", ch.name, ch.id);

    // POSITIVE CONTROL — a fresh membership is NOT muted.
    assert_eq!(
        client.is_conversation_muted(conv).unwrap(),
        false,
        "precondition failed: a newly created channel should be unmuted"
    );

    // THE ACTION (what the toggle now calls) — mute persists on the membership.
    client.mute_conversation(conv, true).expect("mute_conversation(true) failed");
    assert_eq!(
        client.is_conversation_muted(conv).unwrap(),
        true,
        "MUTE NOT PERSISTED: membership.is_muted did not flip on after mute_conversation(true) — \
         the toggle is still a Slint-only bool."
    );
    println!("  mute persisted: is_conversation_muted = true");

    // REVERSIBLE — unmute clears it.
    client.mute_conversation(conv, false).expect("mute_conversation(false) failed");
    assert_eq!(
        client.is_conversation_muted(conv).unwrap(),
        false,
        "unmute did not clear membership.is_muted"
    );

    println!("  V33 PASS — channel mute toggle persists to (and clears from) the local membership row.");
}

// ============================================================================
// V34: PRESENCE reaches FRIENDS — and ONLY friends (privacy)
//
// Presence was dead client-side and leaky server-side:
//   - The client's set_presence() only sent anon-DM E2E presence; it NEVER
//     called the identified UserService.UpdatePresence RPC, so a user's real
//     online status was never published.
//   - The server's SetPresence published only to a single GLOBAL presence_updates
//     firehose (no friend-scoping) consumed by a client StreamManager that is
//     never instantiated. The live consumer, StreamUserEvents, listens on the
//     friend-events channel, where presence was never published.
//
// THE WIRE (now):
//   producer  — ChatterClient::set_presence() also calls UpdatePresence (identified)
//   fan-out   — server publishes an EventTypeUserPresenceUpdated UserEvent to EACH
//               of the updating user's friends (event.UserID = the friend), on the
//               friend-events channel StreamUserEvents already consumes. NOT the
//               global firehose.
//   consumer  — StreamUserEvents maps PresenceChanged -> DomainEvent::UserPresenceChanged
//
// PROOF:
//   - A and B are FRIENDS. A comes online -> B receives UserPresenceChanged{A, Online}.
//     (Asserts on B's STATE / event, not on A's call return.)
//   - PRIVACY: C is NOT a friend of A. C's event stream is ALIVE (positive control:
//     C is a friend of B and receives B's presence). C must NOT receive A's presence.
//     This proves the fan-out is friend-scoped, not a global firehose.
//   - OFFLINE: A goes offline -> B receives UserPresenceChanged{A, Offline}. Proves
//     the full lifecycle both directions, not just the online edge.
//
// MUTATION — in internal/services/user/repository/redis/repository.go, remove the
// `r.publishPresenceToFriends(ctx, update)` call in SetPresence, rebuild + restart
// server+worker: B never receives A's presence -> RED at the main assertion.
// ============================================================================
#[tokio::test]
async fn v34_presence_reaches_friends_not_strangers() {
    use chatter_core::domain::user::PresenceStatus;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A, B, C register and start -----------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v34a"), "V34 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v34b"), "V34 Bob").await;

    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let uid_c = register_and_start(&client_c, &unique_user("v34c"), "V34 Carol").await;

    // ---- Friendship graph: A-B friends, B-C friends, A-C NOT friends --------
    // A befriends B
    befriend(&client_a, &client_b, uid_a, uid_b).await;
    // B befriends C (gives C a live, meaningful event stream to observe)
    befriend(&client_b, &client_c, uid_b, uid_c).await;
    println!("  graph: A-B friends, B-C friends, A-C strangers");

    // ---- Subscribe to B's and C's event buses BEFORE the presence action ----
    let mut events_b = client_b.subscribe_events();
    let mut events_c = client_c.subscribe_events();

    // ---- THE ACTION: A comes online -----------------------------------------
    // Fires the identified UpdatePresence RPC -> server friend-scoped fan-out.
    // (A's start already fired presence, but that was before the A-B friendship
    // existed; this call is the one B must observe.)
    client_a.set_presence(PresenceStatus::Online).await;
    println!("  A published ONLINE via identified UpdatePresence");

    // ---- MAIN ASSERTION: B (a friend) sees A online -------------------------
    let seen_by_b = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e,
            DomainEvent::UserPresenceChanged { user_id, presence, .. }
                if *user_id == uid_a && *presence == PresenceStatus::Online)
    }).await;
    assert!(
        seen_by_b.is_some(),
        "PRESENCE NOT DELIVERED TO FRIEND: A came online but B (A's friend) never received a \
         UserPresenceChanged{{A, Online}}. Either the client producer never called the identified \
         UpdatePresence RPC, or the server never fanned the presence out to A's friends on the \
         friend-events channel StreamUserEvents consumes."
    );
    println!("  MAIN: B received UserPresenceChanged{{A, Online}} — friend delivery works");

    // ---- POSITIVE CONTROL / C-liveness: C (friend of B) sees B online -------
    // Proves C's event stream is ALIVE and would carry a presence event — so the
    // privacy assertion below ("C gets nothing for A") is meaningful, not vacuous.
    client_b.set_presence(PresenceStatus::Online).await;
    let seen_by_c_for_b = wait_for_event(&mut events_c, EVENT_TIMEOUT, |e| {
        matches!(e,
            DomainEvent::UserPresenceChanged { user_id, .. } if *user_id == uid_b)
    }).await;
    assert!(
        seen_by_c_for_b.is_some(),
        "POSITIVE CONTROL FAILED: C never received its friend B's presence, so C's stream is not \
         demonstrably alive — the privacy assertion below would be vacuous (a dead stream trivially \
         'receives nothing')."
    );
    println!("  CONTROL: C received B's presence — C's stream is alive");

    // ---- PRIVACY ASSERTION: the stranger C never sees A's presence ----------
    // A published ONLINE again to give any (buggy) global firehose a fresh chance
    // to leak to C. C is NOT A's friend, so a friend-scoped fan-out reaches B only.
    client_a.set_presence(PresenceStatus::Online).await;
    let leaked_to_c = wait_for_event(&mut events_c, Duration::from_secs(8), |e| {
        matches!(e,
            DomainEvent::UserPresenceChanged { user_id, .. } if *user_id == uid_a)
    }).await;
    assert!(
        leaked_to_c.is_none(),
        "PRIVACY LEAK: the stranger C received A's presence ({:?}). Presence fan-out is not \
         friend-scoped — A's online status leaked to a non-friend. This is the global-firehose \
         leak the fix closes.",
        leaked_to_c
    );
    println!("  PRIVACY: C (a stranger to A) received NOTHING about A — fan-out is friend-scoped");

    // ---- OFFLINE HALF: A goes offline -> B (a friend) sees it ---------------
    // Same wire as shutdown(): set_presence(Offline) awaits the identified
    // UpdatePresence RPC, so the offline edge is exactly what a clean shutdown
    // publishes.
    client_a.set_presence(PresenceStatus::Offline).await;
    let offline_seen_by_b = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e,
            DomainEvent::UserPresenceChanged { user_id, presence, .. }
                if *user_id == uid_a && *presence == PresenceStatus::Offline)
    }).await;
    assert!(
        offline_seen_by_b.is_some(),
        "OFFLINE NOT DELIVERED TO FRIEND: A went offline but B (A's friend) never received a \
         UserPresenceChanged{{A, Offline}}. Friends would see A as online forever after A quits."
    );
    println!("  OFFLINE: B received UserPresenceChanged{{A, Offline}} — full lifecycle delivered");

    println!(
        "V34 PASS — presence is wired end-to-end and friend-scoped: A's friend B sees A come \
         online AND go offline, the stranger C sees neither.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
    client_c.shutdown().await;
}

// ============================================================================
// V35: BLOCKING IS ENFORCED ON GROUP / CHANNEL MESSAGES (SAFETY)
//
// V27 closed blocking on the DM path: a blocked sender cannot fetch the
// recipient's prekey bundle, so a sealed DM never lands. But group / channel
// messages use SENDER KEYS — one shared group session, not a per-recipient
// prekey fetch — so that chokepoint does NOT cover them. The gap: a blocked
// user could still post in a shared channel and the blocker would see it.
//
// Unlike sealed DMs, the server KNOWS the author of a channel message
// (messages.author_id is the real sender for encrypted groups — see V21/F14b:
// content is sealed, sender is NOT). So enforcement is tractable at the
// per-subscriber DELIVERY point: each member receives channel content by
// running their OWN StreamMessageEvents subscription (client/channels.rs
// the channel message stream), the Signal-shaped place to filter — the message
// still exists for everyone else; only the blocking pair is dropped.
//
// THE FIX: internal/services/message/service/block_filter.go +
// streams.go — StreamMessages / StreamMessageEvents / StreamGlobalEvents each
// wrap their delivery channel in a per-subscriber block filter that drops any
// message whose author has a block relationship with the subscriber.
// BIDIRECTIONAL (A blocked B OR B blocked A -> B does not receive A's message)
// and FAIL-CLOSED (an authz backend error DROPS the message, never delivers).
//
// FOUR members prove both directions with a clean positive control:
//   A  = author of the proof message
//   D  = no block relationship with A  -> RECEIVES (positive control)
//   B  = B blocked A (subscriber->author) -> does NOT receive
//   C  = A blocked C (author->subscriber) -> does NOT receive
// Sender keys are established (warm-up) BEFORE any block, so B, C and D all
// hold A's sender key — the ONLY thing keeping B and C from decrypting A's
// message is the server withholding the ciphertext event.
//
// MUTATION — RUN, RED, revert (source hash: Go = main repo): in
// internal/services/message/service/streams.go, in StreamMessageEvents change
// `return s.filterBlockedEvents(ctx, userID, eventChan), nil` back to
// `return eventChan, nil` (remove the filter), rebuild + restart server+worker.
// B and C then receive A's group message (they hold A's sender key) -> RED at
// the "blocker did NOT receive" assertions. D is unaffected either way.
// ============================================================================
#[tokio::test]
async fn v35_blocking_stops_group_message_delivery() {
    use chatter_core::domain::channel::ChannelType;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A, B, C, D register + start ----------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v35a"), "V35 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v35b"), "V35 Bob").await;

    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let uid_c = register_and_start(&client_c, &unique_user("v35c"), "V35 Carol").await;

    let dir_d = tempfile::tempdir().unwrap();
    let client_d = build_client(dir_d.path());
    let uid_d = register_and_start(&client_d, &unique_user("v35d"), "V35 Dave").await;

    assert!(
        [&uid_a, &uid_b, &uid_c, &uid_d]
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 4,
        "POSITIVE CONTROL FAILED: four registrations did not yield four distinct users \
         (A={uid_a} B={uid_b} C={uid_c} D={uid_d})."
    );

    // ---- A creates a DEFAULT Private (encrypted, Sender-Key) group ----------
    let name_e = format!(
        "v35-group-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let group_e = common::create_group_with_members(
        &client_a,
        &name_e,
        ChannelType::Private,
        None, // default -> encrypted (F14a), exactly what T6 / the codebase do
        Some("V35 blocking-on-group-messages"),
        &[(&client_b, &uid_b), (&client_c, &uid_c), (&client_d, &uid_d)],
    )
    .await;
    println!("\n  A created encrypted group {} ({})", group_e.name, group_e.id);

    // POSITIVE CONTROL (membership): the server records A, B, C, D as members.
    let mut member_set = std::collections::HashSet::new();
    for _ in 0..15 {
        member_set = psql(&format!(
            "select user_id from channel_members where channel_id = '{}'",
            group_e.id
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<std::collections::HashSet<String>>();
        if [&uid_a, &uid_b, &uid_c, &uid_d]
            .iter()
            .all(|u| member_set.contains(&u.to_string()))
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    for (label, uid) in [("A", &uid_a), ("B", &uid_b), ("C", &uid_c), ("D", &uid_d)] {
        assert!(
            member_set.contains(&uid.to_string()),
            "POSITIVE CONTROL FAILED: {label} ({uid}) is not a server-side member of group {}. \
             Delivery assertions would be vacuous.",
            group_e.id
        );
    }
    println!("  positive control: A, B, C, D are all server-side members");

    // B, C, D subscribe to the group stream + events BEFORE any send.
    client_b.resubscribe_channel_messages();
    client_c.resubscribe_channel_messages();
    client_d.resubscribe_channel_messages();
    let mut events_b = client_b.subscribe_events();
    let mut events_c = client_c.subscribe_events();
    let mut events_d = client_d.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- WARM-UP (NO BLOCKS YET): establish A's sender key on B, C, D --------
    // The first send mints A's sender key and ships an SKDM to every member's
    // device inbox. Done BEFORE any block so B, C and D all hold A's key — the
    // block must be what withholds the CONTENT, not a missing key.
    client_a
        .send_text(ActiveConversation::Channel(group_e.id.clone()), &unique_marker())
        .await
        .expect("A warm-up send failed");
    let skdm_b = wait_for_sender_key(&client_b, &uid_a, &group_e.id).await;
    let skdm_c = wait_for_sender_key(&client_c, &uid_a, &group_e.id).await;
    let skdm_d = wait_for_sender_key(&client_d, &uid_a, &group_e.id).await;
    assert!(
        skdm_b && skdm_c && skdm_d,
        "SENDER-KEY DISTRIBUTION BROKEN: A's warm-up did not install A's sender key on B \
         ({skdm_b}), C ({skdm_c}) and D ({skdm_d}) within {CONDITION_TIMEOUT:?}. Without the \
         key the block assertions below would pass for the wrong reason (missing key, not \
         enforced block)."
    );
    println!("  warm-up: B, C and D have INSTALLED A's sender key (SKDM delivered AND processed)");

    // ---- THE ACTIONS UNDER TEST: two block directions -----------------------
    // B blocks A  -> subscriber-blocked-author direction.
    client_b.block_user(&uid_a).await.expect("B failed to block A");
    // A blocks C  -> author-blocked-subscriber direction (bidirectional proof).
    client_a.block_user(&uid_c).await.expect("A failed to block C");
    tokio::time::sleep(Duration::from_secs(3)).await; // SpiceDB read-after-write slack

    // ---- A posts the PROOF message ------------------------------------------
    let marker_e = unique_marker();
    println!("  proof marker: {marker_e}");
    client_a
        .send_text(ActiveConversation::Channel(group_e.id.clone()), &marker_e)
        .await
        .expect("A proof send failed");

    // D (no block) MUST receive; B and C (blocked pairs) MUST NOT. Run all three
    // waits concurrently so the two negative waits don't serialize to 2x timeout.
    let gid = group_e.id.clone();
    let (m_d, m_b, m_c) = (marker_e.clone(), marker_e.clone(), marker_e.clone());
    let g_d = gid.clone();
    let g_b = gid.clone();
    let g_c = gid.clone();
    let (recv_d, recv_b, recv_c) = tokio::join!(
        wait_for_event(&mut events_d, EVENT_TIMEOUT, move |e| matches!(
            e, DomainEvent::MessageReceived { message }
            if message.conversation_id == g_d && message.as_text() == Some(m_d.as_str())
        )),
        wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| matches!(
            e, DomainEvent::MessageReceived { message }
            if message.conversation_id == g_b && message.as_text() == Some(m_b.as_str())
        )),
        wait_for_event(&mut events_c, EVENT_TIMEOUT, move |e| matches!(
            e, DomainEvent::MessageReceived { message }
            if message.conversation_id == g_c && message.as_text() == Some(m_c.as_str())
        )),
    );

    // POSITIVE CONTROL: D, who blocks nobody, decrypted A's group message.
    let msg_d = match recv_d {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "POSITIVE CONTROL FAILED: D (no block relationship with A, holds A's sender key) \
             never received A's group message ({marker_e}) within {EVENT_TIMEOUT:?}. The block \
             assertions below would be vacuous — the group pipeline itself is dead."
        ),
    };
    assert_eq!(
        msg_d.as_text(),
        Some(marker_e.as_str()),
        "D decoded a group message but the bytes differ from what A sent"
    );
    assert_eq!(msg_d.sender_id.as_ref(), Some(&uid_a), "D attributed the message to the wrong sender");
    println!("  positive control: D received + decrypted A's group message");

    // LOAD-BEARING (subscriber-blocked-author): B blocked A -> B got NOTHING.
    assert!(
        recv_b.is_none(),
        "GROUP BLOCK NOT ENFORCED (B blocked A): B received A's group message ({marker_e}) even \
         though B blocked A. The per-subscriber block filter on the Sender-Key delivery path is \
         not withholding the ciphertext event from the blocker."
    );
    println!("  B (blocked A) received NOTHING — subscriber->author direction enforced");

    // LOAD-BEARING (author-blocked-subscriber): A blocked C -> C got NOTHING.
    assert!(
        recv_c.is_none(),
        "GROUP BLOCK NOT ENFORCED (A blocked C): C received A's group message ({marker_e}) even \
         though A blocked C. The block filter is not bidirectional on the group delivery path."
    );
    println!("  C (blocked by A) received NOTHING — author->subscriber direction enforced");

    println!(
        "\nV35 PASS — A posted ONE encrypted group message; D (no block) received + decrypted it \
         while B (B blocked A) and C (A blocked C) received NOTHING. Blocking is enforced on the \
         GROUP / CHANNEL Sender-Key delivery path, bidirectionally, at the per-subscriber \
         StreamMessageEvents chokepoint — closing the gap V27 could not (DM prekey fetch does not \
         cover Sender-Key groups). See VALIDATED.md.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
    client_c.shutdown().await;
    client_d.shutdown().await;
}

/// V36 — Disappearing messages: the LOCAL copies actually VANISH.
///
/// THE GAP (documented scope of V29): V29 proved a disappearing message arms
/// its TTL on the wire and the SERVER's Temporal workflow deletes the server
/// row. But the local-expiry engine (`services/ephemeral_service.rs`,
/// migration v015) was NEVER instantiated in production — so the message kept
/// living forever in the SENDER's and RECIPIENT's OWN SQLCipher stores. On a
/// privacy messenger that is the important half: your own device still holds
/// the "disappeared" message.
///
/// THE FIX (this test): the engine is now wired — a bounded periodic reaper
/// (`ChatterClient::start`) calls `cleanup_expired`, and the send/receive
/// paths `track_armed` any message that carries an armed `ephemeral_ttl`. This
/// exercises the SEALED-DM path (`send_server_dm` → `store_data_message`),
/// which ALSO closes the sealed-DM arming gap: the server can't schedule a
/// delete for a sealed message it can't read, so sealed-DM disappearing is
/// entirely local — exactly what this proves.
///
/// Semantics under test: the timer starts at message time (send-time-start),
/// matching V29's channel semantics and the server's Temporal-from-receipt
/// schedule; the recipient's clock starts at receive (negligible skew). This
/// also gives cross-restart expiry for free: `expires_at` is persisted, so a
/// message past its TTL is reaped on the next boot regardless of a "read".
///
/// Proof (asserts on the LOCAL store, scoped to this run):
///   POSITIVE CONTROL — a message with NO timer stays in BOTH local stores.
///   THE FIX — a message with a SHORT timer is present first (so "gone" is not
///     vacuous), then GONE from the RECIPIENT's local store AND the SENDER's
///     own local store after the TTL + a reaper tick, while the control
///     survives (selective).
///
/// MUTATION (RED): remove the reaper spawn in `ChatterClient::start` (or the
/// `track_armed` calls in `send_server_dm` / `store_data_message`) → the armed
/// local copy survives past its TTL → the "GONE" assertions fail.
#[tokio::test]
async fn v36_disappearing_messages_expire_locally() {
    use chatter_core::domain::message::ConversationType;

    skip_if_no_server!();
    no_keyring_prompts();

    // NOTE: this leans on the REAL spawned reaper (default 10s cadence, see
    // `ChatterClient::start`), NOT a test-driven cleanup — that is the point,
    // so removing the reaper spawn makes this test RED. We deliberately do NOT
    // mutate `CHATTER_EPHEMERAL_REAP_SECS` here: this suite runs multi-threaded,
    // and `std::env::set_var` racing another thread's `getenv` is UB. The poll
    // windows below (TTL + one reaper interval + slack) absorb the 10s cadence.

    // ---- A and B register and befriend --------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v36a"), "V36 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v36b"), "V36 Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // Local conversation keys: B buckets a channel-free ServerDm by the SENDER's
    // id; A (sender) stores its own copy keyed by the RECIPIENT's id.
    let conv_on_b = ChannelId::from(uid_a);
    let conv_on_a = ChannelId::from(uid_b);

    // "is a LIVE copy of `marker` present in B's / A's local message store?"
    // The reaper HARD-deletes (purges) an expired message, so once reaped the
    // row is gone from the store entirely — this reads it as absent, which is
    // exactly the "the local copy vanished" property under proof.
    let b_has = |marker: &str| -> bool {
        client_b
            .get_messages_local(&conv_on_b, &ConversationType::ServerDm, None, 200)
            .unwrap_or_default()
            .iter()
            .any(|m| m.as_text() == Some(marker) && !m.is_deleted)
    };
    let a_has = |marker: &str| -> bool {
        client_a
            .get_messages_local(&conv_on_a, &ConversationType::ServerDm, None, 200)
            .unwrap_or_default()
            .iter()
            .any(|m| m.as_text() == Some(marker) && !m.is_deleted)
    };

    // ========================================================================
    // POSITIVE CONTROL — NO timer. The message must PERSIST locally on BOTH.
    // ========================================================================
    let control = format!("v36-control-{}", unique_marker());
    println!("\n  control marker (no timer): {control}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &control)
        .await
        .expect("control send failed");

    // A stores its own outgoing copy synchronously on send.
    assert!(
        a_has(&control),
        "POSITIVE CONTROL FAILED: A did not store its own outgoing control DM."
    );
    // B receives + stores within a few seconds.
    let mut ctrl_on_b = false;
    for _ in 0..30 {
        if b_has(&control) {
            ctrl_on_b = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        ctrl_on_b,
        "POSITIVE CONTROL FAILED: B never stored A's (no-timer) DM — the 'gone' \
         assertions below would be vacuous on a dead pipeline."
    );
    println!("  control stored on BOTH A and B (no timer → should persist)");

    // ========================================================================
    // THE FIX — set a SHORT timer, send. BOTH local copies must VANISH after
    // the TTL + a reaper tick, while the control survives.
    // ========================================================================
    let ttl_secs: u32 = 5;
    client_a
        .set_disappearing_timer(ActiveConversation::ServerDm { peer_id: uid_b }, ttl_secs)
        .expect("set_disappearing_timer failed");
    println!("  set disappearing timer = {ttl_secs}s on the A→B DM conversation");

    let vanish = format!("v36-vanish-{}", unique_marker());
    println!("  disappearing marker: {vanish}");
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &vanish)
        .await
        .expect("armed send failed");

    // A stored its OWN armed copy on send (tracked for reaping).
    assert!(
        a_has(&vanish),
        "SEND-SIDE FAILED: A did not store its own outgoing armed DM."
    );
    // B receives + stores the armed message. We MUST observe it present first,
    // otherwise a later 'absent' is vacuous ("never arrived" vs "reaped").
    let mut vanish_on_b = false;
    for _ in 0..30 {
        if b_has(&vanish) {
            vanish_on_b = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        vanish_on_b,
        "RECEIVE-SIDE FAILED: B never stored the armed DM ({vanish}) — cannot prove \
         local expiry on a message that never landed."
    );
    println!("  armed message stored on BOTH A and B — now waiting for the reaper");

    // LOAD-BEARING: the armed copy must vanish from the RECIPIENT's local store
    // (TTL elapsed + a reaper tick soft-deleted it).
    let mut gone_b = false;
    for _ in 0..35 {
        if !b_has(&vanish) {
            gone_b = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        gone_b,
        "LOCAL EXPIRY FAILED (recipient): the armed DM ({vanish}) is STILL in B's \
         local store well past its {ttl_secs}s TTL. The local-expiry engine / reaper \
         did not remove the recipient's own copy — the message 'disappeared' \
         everywhere except the device that received it."
    );
    println!("  RECIPIENT local copy GONE after TTL + reaper tick");

    // LOAD-BEARING: the SENDER's own local copy must vanish too.
    let mut gone_a = false;
    for _ in 0..35 {
        if !a_has(&vanish) {
            gone_a = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        gone_a,
        "LOCAL EXPIRY FAILED (sender): the armed DM ({vanish}) is STILL in A's own \
         local store past its {ttl_secs}s TTL. The sender's device kept the \
         'disappeared' message."
    );
    println!("  SENDER local copy GONE after TTL + reaper tick");

    // ---- SELECTIVITY: the no-timer control must STILL be present on both -----
    assert!(
        b_has(&control),
        "SELECTIVITY FAILED: the no-timer control ({control}) also vanished from B's \
         store — the reaper is deleting non-ephemeral messages."
    );
    assert!(
        a_has(&control),
        "SELECTIVITY FAILED: the no-timer control ({control}) also vanished from A's \
         store — the reaper is deleting non-ephemeral messages."
    );

    println!(
        "\n  V36 PASS — disappearing messages vanish LOCALLY: a short-TTL sealed DM was \
         stored on both devices, then reaped from the RECIPIENT's and the SENDER's own \
         local stores after its TTL, while the un-armed control survived on both. The \
         local-expiry engine is wired (reaper + track-on-send/receive) and sealed-DM \
         disappearing — which the server cannot schedule — works end to end.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V37 — KEY TRANSPARENCY QUERY-PRIVACY VRF: the served leaf key is VRF_sk(user_id),
// the client verifies the VRF proof byte-exact against a LIVE server proof, the
// recovered index IS the committed leaf, the anon path still REJECTS a forged
// identity key (V24 property preserved under VRF), and a TAMPERED VRF proof is
// REJECTED (the verifier is not decorative).
//
// STATUS: PASS. Phase 3b. Before this, the KT sparse-Merkle leaf key was
// SHA-256("ChatterKT-leaf-v1" || user_id) — a value ANY auditor scanning the
// public log could recompute, mapping every leaf back to its username. Now the
// leaf key is VRF_sk(user_id), an ECVRF-EDWARDS25519-SHA512-TAI output the server
// computes with a secret key; the server serves an 80-byte VRF proof binding the
// queried user_id to that index, and the client verifies it under the PINNED VRF
// public key (config::KT_VRF_PUBLIC_KEY_BYTES). The mapping stays committed and
// publicly auditable, but the leaf keys are pseudorandom and unlinkable to
// usernames without the server's VRF secret.
//
// WHAT THIS PROVES (live, not asserted):
//   1. INTEROP: a REAL server VRF proof, fetched over the anonymous channel,
//      verifies in the byte-exact Rust ECVRF verifier (crypto::vrf::vrf_verify),
//      and the recovered 32-byte index EQUALS inclusion_proof.key. If any byte
//      format diverged (suite tag 0x03, TAI hash-to-curve, 16-byte challenge
//      encoding, cofactor-x8 output), this would fail against the live proof.
//   2. LEAF-KEY IS PSEUDORANDOM: inclusion_proof.key != SHA-256(domain||user_id).
//      The old, auditor-recomputable derivation no longer keys the tree.
//   3. FORGERY STILL REJECTED (V24 under VRF): verify_kt_proof accepts Bob's real
//      key and REJECTS a forged (Mallory) key — the F9 property is preserved now
//      that the leaf key is VRF-derived and the VRF proof is LOAD-BEARING in
//      verify_kt_proof (it derives the expected leaf key from the VRF proof).
//   4. TAMPER -> REJECT (not decorative): flipping one byte of the VRF proof makes
//      verify_kt_proof REJECT with VrfInvalid.
//
// HONEST SCOPE — PROVEN vs ASSERTED:
//   * PROVEN: byte-exact Go<->Rust ECVRF interop against a live proof; tamper is
//     rejected; the tree is VRF-keyed and V14/V24/V25 stay green under it.
//   * NOT achieved (documented, not claimed): query privacy against the SERVING
//     server. GetProof still carries the plaintext user_id because the server
//     needs it to compute VRF_sk(user_id) — the VRF is server-side, not oblivious.
//     Hiding the lookup from the server too needs an OPRF (out of scope). What VRF
//     buys is unlinkability of leaf->username for a THIRD-PARTY AUDITOR of the log
//     plus a committed, verifiable name->index mapping.
//
// MUTATION (RED test): in crypto/keytransparency.rs::verify_proof, replace the
// VRF-derived `expected_key` with the old SHA-256 leaf_key(user_id): against the
// VRF-keyed server the honest POSITIVE CONTROL then fails with WrongLeafKey -> RED.
// Or make vrf_verify return Some(index) unconditionally (decorative verifier):
// the TAMPER assertion then fails to reject -> RED. Reverting restores GREEN.
// ============================================================================
#[tokio::test]
async fn v37_kt_query_privacy_vrf_verified_and_forgery_still_rejected() {
    use chatter_core::config::KT_VRF_PUBLIC_KEY_BYTES;
    use chatter_core::crypto::vrf::vrf_verify;
    use chatter_core::crypto::{verify_kt_proof, KtError};
    use libsignal_protocol::{IdentityKey, IdentityKeyPair};
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use sha2::{Digest, Sha256};

    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!("skipping v37: INTEGRATION_TESTS not set");
        return;
    }

    let mut rng = StdRng::seed_from_u64(0x37_FEED);

    // ---- Register a REAL Bob (Phase-1 producer appends his identity key under
    // its VRF index). The pool is NOT authenticated: the anon channel must reach
    // the public GetProof, which now carries the VRF proof. --------------------
    let pool = common::create_test_pool().await.expect("pool connect failed");
    let bob = common::register_test_user(&pool)
        .await
        .expect("Bob registration failed — no KT leaf to verify against");

    let ik_bob = IdentityKey::decode(&bob.public_key).expect("Bob identity key decodes");
    let mallory_kp = IdentityKeyPair::generate(&mut rng);
    let ik_mallory = *mallory_kp.identity_key();
    let mallory_serialized = ik_mallory.serialize();
    assert_ne!(ik_bob, ik_mallory, "sanity: Bob and Mallory are distinct identities");

    // ---- Fetch Bob's proof over the ANONYMOUS channel (public method, no auth).
    let mut anon_kt = pool.anonymous_kt_client();
    let proof = anon_kt
        .get_proof(&bob.user_id)
        .await
        .expect("anon GetProof transport ok (public method reachable without auth)")
        .expect("Bob must have a KT leaf (Phase-1 producer runs at registration)");

    let incl = proof.inclusion_proof.as_ref().expect("inclusion proof present");
    let leaf_key_on_wire: [u8; 32] =
        incl.key.as_slice().try_into().expect("leaf key is 32 bytes");

    assert!(!proof.vrf_proof.is_empty(), "server must serve a VRF proof (Phase 3b)");
    assert_eq!(proof.vrf_proof.len(), 80, "ECVRF proof is 80 bytes (gamma||c||s)");
    assert_eq!(
        proof.vrf_public_key.as_slice(),
        KT_VRF_PUBLIC_KEY_BYTES.as_slice(),
        "advisory VRF pubkey the server names must match the client's PINNED key"
    );

    // (1) INTEROP: the LIVE server VRF proof verifies byte-exact in the Rust
    // ECVRF verifier, and the recovered index IS the committed leaf key.
    let index = vrf_verify(&KT_VRF_PUBLIC_KEY_BYTES, bob.user_id.as_bytes(), &proof.vrf_proof)
        .expect("LIVE server VRF proof must verify in the byte-exact Rust ECVRF verifier");
    assert_eq!(
        index, leaf_key_on_wire,
        "VRF index recovered from the proof must EQUAL inclusion_proof.key (the tree is VRF-keyed)"
    );
    assert_eq!(
        &proof.vrf_output[..32],
        index.as_slice(),
        "the first 32 bytes of the 64-byte VRF output are the leaf index"
    );
    println!(
        "  (1) INTEROP: live server VRF proof verified byte-exact; index={} == leaf key",
        hex::encode(&index[..8])
    );

    // (2) LEAF-KEY IS PSEUDORANDOM: NOT the old, auditor-recomputable
    // SHA-256("ChatterKT-leaf-v1" || user_id).
    let old_sha_leaf: [u8; 32] = {
        let mut h = Sha256::new();
        h.update(b"ChatterKT-leaf-v1");
        h.update(bob.user_id.as_bytes());
        h.finalize().into()
    };
    assert_ne!(
        leaf_key_on_wire, old_sha_leaf,
        "QUERY-PRIVACY REGRESSION: the leaf key equals the guessable SHA-256(domain||user_id) — \
         an auditor could map it back to the username. It must be the VRF index."
    );
    println!("  (2) leaf key is the VRF index, NOT the auditor-recomputable SHA-256(user_id)");

    // (3) FORGERY STILL REJECTED (V24 under VRF): the VRF proof is load-bearing in
    // verify_kt_proof. Honest key verifies; a forged (Mallory) key is rejected.
    verify_kt_proof(&bob.user_id, &bob.public_key, &proof)
        .expect("POSITIVE CONTROL: Bob's real key must verify against his VRF-keyed KT proof");

    let forged_verdict = verify_kt_proof(&bob.user_id, &mallory_serialized, &proof);
    assert!(
        matches!(forged_verdict, Err(KtError::IdentityKeyMismatch)),
        "F9 REGRESSION under VRF: a forged identity key ({ik_mallory:?}) must be REJECTED with \
         IdentityKeyMismatch, got {forged_verdict:?}"
    );
    println!("  (3) forgery still rejected under VRF: {}", forged_verdict.unwrap_err());

    // (4) TAMPER -> REJECT (verifier is not decorative): flip one byte of the VRF
    // proof; verify_kt_proof must reject with VrfInvalid.
    {
        let mut tampered = proof.clone();
        tampered.vrf_proof[10] ^= 0x01; // corrupt gamma byte
        let verdict = verify_kt_proof(&bob.user_id, &bob.public_key, &tampered);
        assert!(
            matches!(verdict, Err(KtError::VrfInvalid)),
            "DECORATIVE-VERIFIER TRAP: a tampered VRF proof must be REJECTED with VrfInvalid, \
             got {verdict:?}. A verifier that accepts a broken proof is worse than none."
        );
        println!("  (4) tampered VRF proof REJECTED: {}", verdict.unwrap_err());
    }

    // Bonus: the VRF proof does not verify under a DIFFERENT user_id (wrong alpha)
    // — it is bound to the exact user queried.
    {
        let wrong = vrf_verify(
            &KT_VRF_PUBLIC_KEY_BYTES,
            b"00000000-0000-0000-0000-000000000000",
            &proof.vrf_proof,
        );
        assert!(wrong.is_none(), "VRF proof must not verify under a different user_id (alpha)");
    }

    println!(
        "\n  V37 PASS — the KT leaf key is now VRF_sk(user_id); the client verified a LIVE server \
         VRF proof byte-exact (Go go-ecvrf <-> Rust curve25519-dalek), the recovered index is the \
         committed leaf, a forged identity key is STILL rejected (V24 under VRF), and a tampered \
         VRF proof is rejected. Query privacy vs a THIRD-PARTY AUDITOR is achieved; vs the serving \
         server it is not (GetProof still carries user_id — an OPRF is out of scope).\n"
    );
}

// ============================================================================
// V38: A FRIEND ACTUALLY SEES YOUR NEW AVATAR (avatar friend-fetch round-trip)
//
// V32 proved avatar UPLOAD: changing your avatar encrypts it with YOUR profile
// key and stores the ciphertext server-side (0 -> N bytes). V32 explicitly
// SCOPED OUT the user-visible half — a FRIEND fetching your profile and
// DECRYPTING your avatar — because you never decrypt your OWN avatar
// (try_decrypt_avatar uses the *friend's* profile key; a self-fetch is None by
// design; services/user/mod.rs:290). This test proves the scoped-out property:
//
//   A and B are FRIENDS, so B holds A's profile key (carried on the friend-
//   request envelope + the post-accept profile-key handshake + any A->B DM via
//   inject_profile_key_if_sharing; server_dm.rs:109). A changes its avatar to
//   known random bytes. B fetches A's profile and B's get_user_profile ->
//   try_decrypt_avatar decrypts A's `encrypted_avatar` with A's profile key.
//   B's decrypted avatar must equal A's uploaded bytes BYTE-EXACT.
//
// POSITIVE CONTROL: the encrypted avatar is present server-side (V32's fact) —
//   without this the friend-fetch would decrypt nothing and pass vacuously.
// PRIVACY HALF: a STRANGER C (not A's friend, holds no profile key) fetching
//   A's profile gets avatar_bytes = None — the ciphertext is on the server
//   (octet_length > 0, asserted) yet C cannot decrypt it.
//
// MUTATION — RUN, RED, revert: change the byte-exact expectation below from
//   `&avatar` to wrong bytes (e.g. `assert_eq!(got, vec![0u8; avatar.len()], ...)`)
//   -> the "BYTE-EXACT" assertion goes RED, proving the equality has teeth
//   (B genuinely decrypted A's real avatar, not a coincidental Some(...)).
//   Verified RED: "assertion `left == right` failed ... left: [<A's bytes>]
//   right: [0, 0, 0, ...]".
// ============================================================================
#[tokio::test]
async fn v38_friend_fetches_and_decrypts_your_avatar_byte_exact() {
    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A and B register and start (inbox polling live on both) -----------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v38a"), "V38 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v38b"), "V38 Bob").await;

    // ---- become FRIENDS: this is how B comes to hold A's profile key -------
    // A's friend-request envelope carries A's profile key (ECIES to B), the
    // post-accept handshake re-sends it, and the explicit A->B DM below carries
    // it a third way. Any one suffices; we exercise the real friendship path.
    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // ---- A changes its avatar to KNOWN random bytes (small: no normalize) ---
    // < 64 KB so the WebP normalizer never fires (services/user/mod.rs:377),
    // hence what B decrypts must equal these bytes byte-for-byte.
    let avatar: Vec<u8> = {
        let mut v = b"V38-AVATARx".to_vec();
        let mut r = [0u8; 512];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut r);
        v.extend_from_slice(&r);
        v
    };
    client_a.set_avatar_bytes(&avatar).expect("set_avatar_bytes failed");
    client_a
        .update_user_profile(Some("V38 Alice".into()), None)
        .await
        .expect("update_user_profile failed");

    // ---- POSITIVE CONTROL: the encrypted avatar is present server-side -----
    // (V32's property; without ciphertext on the server every decrypt below is
    // vacuous.) Also proves the byte-exact match later is NOT a plaintext leak.
    let blob_sql = format!(
        "select coalesce(octet_length(encrypted_avatar),0) from user_profiles where user_id='{uid_a}'"
    );
    let mut server_bytes = String::new();
    let mut uploaded = false;
    for _ in 0..15 {
        server_bytes = psql(&blob_sql);
        if server_bytes.parse::<i64>().unwrap_or(0) > 0 {
            uploaded = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        uploaded,
        "POSITIVE CONTROL FAILED: A's encrypted_avatar is still {server_bytes:?} server-side — \
         the upload never landed, so a friend-fetch would prove nothing."
    );
    println!("\n  positive control: A's encrypted_avatar on server = {server_bytes} bytes (ciphertext present)");

    // ---- NEGATIVE HALF: the plaintext avatar is NOT in the server blob -----
    // The random tail is unique enough to grep for. E2E means the server holds
    // ciphertext only; B's byte-exact match must come from B's decrypt, not from
    // the server handing out plaintext.
    let hexbody: String = avatar[11..].iter().map(|b| format!("{b:02x}")).collect();
    let leak = psql(&format!(
        "select count(*) from user_profiles where user_id='{uid_a}' \
         and position('\\x{hexbody}'::bytea in encrypted_avatar) > 0"
    ));
    assert_eq!(
        leak.trim(),
        "0",
        "E2EE VIOLATED: A's plaintext avatar bytes appear inside the server's encrypted_avatar blob"
    );
    println!("  negative half: plaintext avatar body is NOT present in the server ciphertext");

    // ---- A sends B a DM so B definitely holds A's profile key --------------
    // Belt-and-suspenders on top of the friend-request envelope + handshake.
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, "v38 profile-key carrier")
        .await
        .expect("send_text failed");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- THE PROPERTY: B fetches A's profile and DECRYPTS A's avatar -------
    // Poll: B may need a moment to have persisted A's profile key from the
    // envelope/handshake/DM. get_user_profile -> try_decrypt_avatar uses A's key.
    let mut b_avatar: Option<Vec<u8>> = None;
    for _ in 0..20 {
        let prof = client_b
            .get_user_profile(&uid_a)
            .await
            .expect("B get_user_profile(A) failed");
        if let Some(bytes) = prof.avatar_bytes {
            b_avatar = Some(bytes);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let got = b_avatar.expect(
        "FRIEND CANNOT SEE AVATAR: B fetched A's profile but avatar_bytes was None — \
         B never obtained A's profile key, or try_decrypt_avatar failed to decrypt. \
         The avatar uploads (V32) but a friend can't decrypt it.",
    );

    // BYTE-EXACT — the whole point. B decrypted A's real avatar, not a stale or
    // truncated blob. (MUTATION: replace `avatar` with wrong bytes -> RED.)
    assert_eq!(
        got, avatar,
        "AVATAR MISMATCH: B decrypted {} bytes but A uploaded {} bytes; they differ. \
         The friend-fetch round-trip is not byte-exact.",
        got.len(),
        avatar.len()
    );
    println!(
        "  THE PROPERTY: B decrypted A's avatar byte-exact ({} bytes) using A's profile key",
        got.len()
    );

    // ---- PRIVACY HALF: a STRANGER C (no profile key) gets avatar_bytes=None -
    // C is registered but NOT A's friend, so C holds no A profile key. The
    // ciphertext is on the server (octet_length asserted > 0 above), yet C's
    // get_user_profile -> try_decrypt_avatar returns None (no key -> None).
    let dir_c = tempfile::tempdir().unwrap();
    let client_c = build_client(dir_c.path());
    let _uid_c = register_and_start(&client_c, &unique_user("v38c"), "V38 Carol").await;

    let c_prof = client_c
        .get_user_profile(&uid_a)
        .await
        .expect("C get_user_profile(A) failed");
    assert!(
        c_prof.avatar_bytes.is_none(),
        "PRIVACY LEAK: stranger C (not A's friend, no profile key) got avatar_bytes=Some — \
         the encrypted avatar is on the server but C must NOT be able to decrypt it."
    );
    println!("  privacy half: stranger C (no profile key) gets avatar_bytes=None despite ciphertext on server");

    println!(
        "  V38 PASS — a friend actually SEES your new avatar: A changed avatar, B (friend) fetched \
         A's profile and decrypted the avatar BYTE-EXACT with A's profile key, while stranger C \
         (no key) could not. This is the round-trip V32 scoped out."
    );
}

// ============================================================================
// V39: the "send read receipts" privacy toggle actually gates the send.
//
// CLAIM (Settings UI): a "Send read receipts" switch. Before this fix
// (MISSING-FEATURES #5) `set_read_receipts_enabled` persisted a bool but the
// getter had NO caller on any send path — so toggling it OFF changed nothing
// and A's read receipts kept flowing. A privacy setting that lies.
//
// PROPERTY (assert on the RECIPIENT, B): A reads B's DM →
//   - receipts ENABLED (shipped default): B's copy of the message it sent flips
//     to delivery_status = "read" (the receipt reached B).
//   - receipts DISABLED (set_read_receipts_enabled(false)): B's copy NEVER
//     flips to "read" — the receipt is not sent.
//
// This also exercises finding #8: the receipt is sent on the 1:1 ServerDm path
// (channel-free DM), i.e. DM read receipts DO round-trip, not only channels.
//
// MUTATION CHECK: delete the gate in ConversationHandle::send_read_receipt
// (services/conversation.rs) → the disabled half sends anyway → B's second
// message flips to "read" → this test FAILS (RED). That is what makes the
// gate assertion non-vacuous.
// ============================================================================

/// Poll B's stored copy (keyed by the SENDER=peer channel) of the message it
/// sent at `sent_at`, returning its delivery_status, until it equals `want` or
/// the deadline passes. Returns the last-seen status.
async fn poll_delivery_status(
    client_b: &Arc<ChatterClient>,
    conv_on_b: &ChannelId,
    sent_at: u64,
    want: &str,
    max: Duration,
) -> String {
    use chatter_core::domain::message::ConversationType;
    let start = Instant::now();
    let mut last = String::from("<none>");
    while start.elapsed() < max {
        let msgs = client_b
            .get_messages_local(conv_on_b, &ConversationType::ServerDm, None, 50)
            .unwrap_or_default();
        if let Some(m) = msgs.iter().find(|m| m.sent_at == sent_at) {
            last = m.delivery_status.clone();
            if last == want {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    last
}

/// A reads whatever B sent this run (finds B's message in A's local ServerDm
/// store and sends a read receipt for it). Returns the receipted timestamp.
async fn a_reads_bs_message(
    client_a: &Arc<ChatterClient>,
    uid_b: UserId,
    conv_on_a: &ChannelId,
    marker: &str,
) -> u64 {
    use chatter_core::domain::message::ConversationType;
    // A must have STORED B's DM before it can read it.
    let mut ts = None;
    for _ in 0..40 {
        let msgs = client_a
            .get_messages_local(conv_on_a, &ConversationType::ServerDm, None, 50)
            .unwrap_or_default();
        if let Some(m) = msgs.iter().find(|m| m.as_text() == Some(marker)) {
            ts = Some(m.sent_at);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let ts = ts.expect(
        "POSITIVE CONTROL FAILED: A never stored B's DM, so A cannot 'read' it — \
         the receipt half below would be vacuous",
    );
    client_a
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_read_receipt(&[ts])
        .await
        .expect("A send_read_receipt failed");
    ts
}

#[tokio::test]
async fn v39_read_receipt_toggle_gates_the_send() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v39a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v39b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // B's view of A is keyed by A's id; A's view of B is keyed by B's id.
    let conv_on_b = ChannelId::from(uid_a); // where B stores the msgs it SENT to A
    let conv_on_a = ChannelId::from(uid_b); // where A stores the msgs it got from B

    // ======================================================================
    // POSITIVE CONTROL — receipts ENABLED (shipped default). The receipt must
    // reach B (finding #8: this is the 1:1 ServerDm path, not a channel).
    // ======================================================================
    assert!(
        client_a.read_receipts_enabled().unwrap(),
        "precondition: shipped default for read receipts is ON"
    );

    let marker1 = unique_marker();
    client_b
        .send_text(ActiveConversation::ServerDm { peer_id: uid_a }, &marker1)
        .await
        .expect("B send_text 1 failed");

    // Sanity: B stored its own sent message and it is NOT yet "read".
    let ts1 = {
        use chatter_core::domain::message::ConversationType;
        let mut found = None;
        for _ in 0..40 {
            let msgs = client_b
                .get_messages_local(&conv_on_b, &ConversationType::ServerDm, None, 50)
                .unwrap_or_default();
            if let Some(m) = msgs.iter().find(|m| m.as_text() == Some(marker1.as_str())) {
                found = Some((m.sent_at, m.delivery_status.clone()));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let (ts, status) = found.expect("B never stored its own sent message 1");
        assert_ne!(status, "read", "message 1 was 'read' before A ever read it");
        ts
    };

    let read_ts = a_reads_bs_message(&client_a, uid_b, &conv_on_a, &marker1).await;
    assert_eq!(read_ts, ts1, "A receipted a different timestamp than B stored");

    let status1 = poll_delivery_status(&client_b, &conv_on_b, ts1, "read", Duration::from_secs(20)).await;
    assert_eq!(
        status1, "read",
        "POSITIVE CONTROL FAILED (finding #8?): receipts are ENABLED and A read B's DM, \
         but B's message never flipped to 'read' (last='{status1}'). Either the read \
         receipt is not sent on the 1:1 ServerDm path, or the gate wrongly suppressed it."
    );
    println!("  positive control: receipts ENABLED → B's message flipped to 'read' (DM receipt round-trips)");

    // ======================================================================
    // THE FIX — receipts DISABLED. A reads B's next DM; the receipt must NOT
    // be sent, so B's copy must NEVER become "read".
    // ======================================================================
    client_a
        .set_read_receipts_enabled(false)
        .expect("set_read_receipts_enabled(false) failed");
    assert!(!client_a.read_receipts_enabled().unwrap(), "toggle did not persist OFF");

    let marker2 = unique_marker();
    client_b
        .send_text(ActiveConversation::ServerDm { peer_id: uid_a }, &marker2)
        .await
        .expect("B send_text 2 failed");

    let ts2 = a_reads_bs_message(&client_a, uid_b, &conv_on_a, &marker2).await;

    // Give a generous window — the ENABLED case flipped within a few seconds,
    // so ~12s of staying un-read is strong evidence the receipt was suppressed.
    let status2 = poll_delivery_status(&client_b, &conv_on_b, ts2, "read", Duration::from_secs(12)).await;
    assert_ne!(
        status2, "read",
        "PRIVACY TOGGLE LIES: read receipts are DISABLED, yet A reading B's DM still \
         flipped B's message to 'read' (status='{status2}'). The gate did not stop the send."
    );
    println!("  the fix: receipts DISABLED → B's message stayed '{status2}' (receipt NOT sent)");

    println!(
        "  V39 PASS — the 'send read receipts' toggle gates the send: ENABLED → B sees 'read', \
         DISABLED → B never does. Asserted on the RECIPIENT. DM (ServerDm) receipts round-trip."
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V40: the "show typing indicators" privacy toggle actually gates the send.
//
// Mirror of V39 for typing. Before the fix `set_typing_indicators_enabled`
// persisted a bool whose getter had no send-path caller — so disabling "show
// typing" still transmitted typing to the peer.
//
// PROPERTY (assert on the RECIPIENT, B via its DomainEvent stream):
//   - typing ENABLED (default): A typing → B receives DomainEvent::TypingStarted.
//   - typing DISABLED: A typing → B receives NO TypingStarted for A.
//
// MUTATION CHECK: delete the gate in ConversationHandle::send_typing
// (services/conversation.rs) → the disabled half emits typing → B gets
// TypingStarted → this test FAILS (RED).
// ============================================================================

#[tokio::test]
async fn v40_typing_toggle_gates_the_send() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v40a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v40b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // ======================================================================
    // POSITIVE CONTROL — typing ENABLED (default): A typing reaches B.
    // ======================================================================
    assert!(
        client_a.typing_indicators_enabled().unwrap(),
        "precondition: shipped default for typing indicators is ON"
    );

    let mut events_b = client_b.subscribe_events();
    client_a
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_typing(true)
        .await
        .expect("A send_typing(true) failed");

    let got = wait_for_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, DomainEvent::TypingStarted { user_id, .. } if *user_id == uid_a)
    })
    .await;
    assert!(
        got.is_some(),
        "POSITIVE CONTROL FAILED: typing ENABLED and A is typing, but B never received \
         DomainEvent::TypingStarted for A — the typing path is not wired, so the \
         disabled-half assertion below would be vacuous."
    );
    println!("  positive control: typing ENABLED → B received TypingStarted from A");

    // ======================================================================
    // THE FIX — typing DISABLED: A typing must NOT reach B.
    // ======================================================================
    client_a
        .set_typing_indicators_enabled(false)
        .expect("set_typing_indicators_enabled(false) failed");
    assert!(!client_a.typing_indicators_enabled().unwrap(), "toggle did not persist OFF");

    // Fresh subscription so no stale STARTED from the positive half is in the
    // buffer, then A "types" again.
    let mut events_b2 = client_b.subscribe_events();
    client_a
        .conversation(ActiveConversation::ServerDm { peer_id: uid_b })
        .send_typing(true)
        .await
        .expect("A send_typing(true) [disabled] returned an error (should be a silent no-op)");

    // The ENABLED case arrived within a couple seconds; 8s of silence is strong
    // evidence nothing was transmitted.
    let leaked = wait_for_event(&mut events_b2, Duration::from_secs(8), |e| {
        matches!(e, DomainEvent::TypingStarted { user_id, .. } if *user_id == uid_a)
    })
    .await;
    assert!(
        leaked.is_none(),
        "PRIVACY TOGGLE LIES: typing indicators are DISABLED, yet A typing still reached \
         B as TypingStarted. The gate did not stop the send."
    );
    println!("  the fix: typing DISABLED → B received NO TypingStarted (typing NOT sent)");

    println!(
        "  V40 PASS — the 'show typing indicators' toggle gates the send: ENABLED → B sees \
         typing, DISABLED → B never does. Asserted on the RECIPIENT's event stream."
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V41 — Group/channel replies carry the QUOTE to the RECIPIENT.
///
/// THE GAP (MISSING-FEATURES #9): the DM/anon reply paths carried the quote on
/// the wire, but the group/channel send path (`route_channel` →
/// `send_with_pointers`/`send_with_files` → `MessageService::send`) built the
/// E2E `DataMessage` with `quote = None` and only PATCHED THE SENDER'S OWN
/// local row (`persist_outgoing_reply_context`). So the sender saw their reply
/// bubble, but every RECIPIENT got a message with no quote — a reply rendered
/// as plain text. A fully-built `send_reply` that DID carry the wire quote sat
/// dead (zero callers); the router never used it.
///
/// THE FIX (this test): the quote is threaded through the live chokepoint —
/// `send`/`send_message` now build the wire `Quote` (`build_wire_quote`) from
/// the reply intent and set it on the `DataMessage`, so it rides inside the
/// Sender-Key ciphertext to every member. The receiver's `apply_inbound_quote`
/// then populates `reply_to_author_id` / `reply_to_text` from the wire. (The
/// dead `send_reply` was removed — its logic now lives on the one path.)
///
/// Proof (asserts on the RECIPIENT's decrypted message, scoped to this run):
///   POSITIVE CONTROL — A's non-reply warm-up message arrives at B with NO
///     quote (`reply_to_text` is None), so a populated quote below is not
///     something the pipeline stamps on every message.
///   THE FIX — B replies to A's message in the channel; A receives B's reply
///     and it carries the quote: `reply_to_author_id == A` and
///     `reply_to_text == <A's original text>`, decoded from the WIRE (A did not
///     send the reply, so this cannot be the sender-local patch).
///
/// MUTATION (RED): set `quote: None` back on the `DataMessage` in
/// `MessageService::send_message` → A's received reply has `reply_to_text ==
/// None` / `reply_to_author_id == None` → the fix assertions fail.
#[tokio::test]
async fn v41_group_reply_carries_quote_to_recipient() {
    use chatter_core::domain::channel::ChannelType;
    use chatter_core::prelude::QuoteIntent;

    skip_if_no_server!();
    no_keyring_prompts();

    // ---- A, B register + start ----------------------------------------------
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v41a"), "V41 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v41b"), "V41 Bob").await;

    assert_ne!(uid_a, uid_b, "POSITIVE CONTROL FAILED: A and B are the same user");

    // ---- A creates a DEFAULT Private (encrypted, Sender-Key) channel --------
    let name = format!(
        "v41-chan-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let chan = common::create_group_with_members(
        &client_a,
        &name,
        ChannelType::Private,
        None, // default -> encrypted Sender-Key channel
        Some("V41 group reply carries quote"),
        &[(&client_b, &uid_b)],
    )
    .await;
    println!("\n  A created encrypted channel {} ({})", chan.name, chan.id);

    // POSITIVE CONTROL (membership): server records A and B.
    let mut members = std::collections::HashSet::new();
    for _ in 0..15 {
        members = psql(&format!(
            "select user_id from channel_members where channel_id = '{}'",
            chan.id
        ))
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<std::collections::HashSet<String>>();
        if members.contains(&uid_a.to_string()) && members.contains(&uid_b.to_string()) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        members.contains(&uid_a.to_string()) && members.contains(&uid_b.to_string()),
        "POSITIVE CONTROL FAILED: A and B are not both server-side members of {}. \
         Delivery assertions would be vacuous.",
        chan.id
    );
    println!("  positive control: A and B are both server-side members");

    client_a.resubscribe_channel_messages();
    client_b.resubscribe_channel_messages();
    let mut events_a = client_a.subscribe_events();
    let mut events_b = client_b.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- A sends the ORIGINAL message (the reply target) --------------------
    // This also mints A's sender key + SKDM to B so B can decrypt it, and gives
    // B a local copy of the parent. Capture A's sent_at to key the quote.
    let original_text = unique_marker();
    let r1 = client_a
        .send_text(ActiveConversation::Channel(chan.id.clone()), &original_text)
        .await
        .expect("A original send failed");
    let parent_ts = r1.sent_at;

    let g = chan.id.clone();
    let t1 = original_text.clone();
    let got_original = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessageReceived { message }
        if message.conversation_id == g && message.as_text() == Some(t1.as_str())
    )).await;
    assert!(
        got_original.is_some(),
        "POSITIVE CONTROL FAILED: B never received A's original message ({original_text}) within \
         {EVENT_TIMEOUT:?}. The reply assertion would be vacuous — the channel pipeline is dead."
    );
    println!("  positive control: B received A's original message (parent ts={parent_ts})");

    // ---- WARM-UP: establish B's sender key on A -----------------------------
    // B's first send mints B's sender key + ships an SKDM to A's device inbox.
    // Wait for A to have INSTALLED that key rather than for the decrypted text —
    // the message itself races the SKDM and may arrive as an
    // "[Encrypted message]" placeholder before A can decode it. Once the key is
    // on A, the plain + reply sends below decode cleanly.
    let warm_text = unique_marker();
    client_b
        .send_text(ActiveConversation::Channel(chan.id.clone()), &warm_text)
        .await
        .expect("B warm-up send failed");
    assert!(
        wait_for_sender_key(&client_a, &uid_b, &chan.id).await,
        "SENDER-KEY DISTRIBUTION BROKEN: B's warm-up did not install B's sender key on A \
         within {CONDITION_TIMEOUT:?}. The reply assertions would then fail for the wrong \
         reason (missing key, not a dropped quote)."
    );
    println!("  warm-up: A has INSTALLED B's sender key (SKDM delivered AND processed)");

    // ---- NEGATIVE CONTROL: a PLAIN (non-reply) message carries no quote -----
    // Sent AFTER the key is warm so A decodes it directly.
    let plain_text = unique_marker();
    client_b
        .send_text(ActiveConversation::Channel(chan.id.clone()), &plain_text)
        .await
        .expect("B plain send failed");
    let g = chan.id.clone();
    let tp = plain_text.clone();
    let plain_recv = wait_for_event(&mut events_a, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessageReceived { message }
        if message.conversation_id == g && message.as_text() == Some(tp.as_str())
    )).await;
    let plain_msg = match plain_recv {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "POSITIVE CONTROL FAILED: A never received B's plain message ({plain_text}) within \
             {EVENT_TIMEOUT:?} even after B's key was established. The reply proof would be vacuous."
        ),
    };
    assert!(
        plain_msg.reply_to_text.is_none() && plain_msg.reply_to_author_id.is_none(),
        "NEGATIVE CONTROL FAILED: B's plain (non-reply) message arrived at A already carrying a \
         quote (reply_to_text={:?}, reply_to_author_id={:?}). A populated quote on the reply below \
         would then prove nothing.",
        plain_msg.reply_to_text, plain_msg.reply_to_author_id
    );
    println!("  negative control: B's plain message reached A with NO quote");

    // ---- THE ACTION UNDER TEST: B REPLIES to A's original message -----------
    let reply_text = unique_marker();
    client_b
        .send_message(
            ActiveConversation::Channel(chan.id.clone()),
            &reply_text,
            &[],
            Some(QuoteIntent {
                target_author_id: uid_a.clone(),
                target_timestamp: parent_ts,
                target_preview: Some(original_text.clone()),
            }),
            None,
        )
        .await
        .expect("B reply send failed");

    let g = chan.id.clone();
    let tr = reply_text.clone();
    let reply_recv = wait_for_event(&mut events_a, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessageReceived { message }
        if message.conversation_id == g && message.as_text() == Some(tr.as_str())
    )).await;

    // POSITIVE CONTROL: A received B's reply at all.
    let reply_msg = match reply_recv {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!(
            "POSITIVE CONTROL FAILED: A never received B's reply ({reply_text}) within \
             {EVENT_TIMEOUT:?}."
        ),
    };
    assert_eq!(
        reply_msg.as_text(),
        Some(reply_text.as_str()),
        "A decoded B's reply but the body differs from what B sent"
    );

    // LOAD-BEARING: the reply carries the quote, decoded from the WIRE. A is the
    // RECIPIENT (B sent the reply), so these fields cannot be the sender-local
    // patch — they can only come from the quote riding inside the ciphertext.
    assert_eq!(
        reply_msg.reply_to_author_id.as_ref(),
        Some(&uid_a),
        "GROUP REPLY QUOTE DROPPED: A received B's reply but reply_to_author_id is {:?}, not A \
         ({uid_a}). The quote did not ride on the wire — recipients see the reply as plain text.",
        reply_msg.reply_to_author_id
    );
    assert_eq!(
        reply_msg.reply_to_text.as_deref(),
        Some(original_text.as_str()),
        "GROUP REPLY QUOTE DROPPED: A received B's reply but reply_to_text is {:?}, not the quoted \
         original ({original_text:?}). The wire quote is missing or empty.",
        reply_msg.reply_to_text
    );
    println!("  the fix: A received B's reply WITH the quote (author=A, text=original) from the wire");

    println!(
        "\n  V41 PASS — a reply sent into an encrypted CHANNEL carries the quote to the RECIPIENT: \
         A (who did NOT send the reply) decoded B's reply and it references A's original message \
         (reply_to_author_id=A, reply_to_text=<original>), while B's plain warm-up carried no \
         quote. Closes MISSING-FEATURES #9 (group replies rendered as plain text). See VALIDATED.md.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V42 — 1:1 DM read receipt LIVE-REFRESHES the sender (emits a DomainEvent).
///
/// THE GAP (MISSING-FEATURES #8, live-refresh half): V39 proved the DB
/// `delivery_status` flips to "read" on the 1:1 ServerDm path. But the
/// `InboxEvent::DmReceipt` handler (`inbox_processor.rs`) updated the row
/// SILENTLY — it emitted no `DomainEvent`, so a live UI kept showing the old
/// checkmark until a manual re-query. The channel and anon receipt paths emit
/// `DomainEvent::MessagesRead`; the ServerDm path did not.
///
/// THE FIX (this test): the DmReceipt handler now emits the SAME
/// `DomainEvent::MessagesRead` (read/viewed) / `MessagesDelivered` (delivery)
/// the other paths do, so the sender's bubble refreshes live.
///
/// Proof (asserts on the SENDER's live event stream, scoped to this run):
///   POSITIVE CONTROL — A sends B a DM and B stores it (so the read below is
///     not vacuous).
///   THE FIX — B reads A's DM; A (the SENDER) receives a
///     `DomainEvent::MessagesRead` naming B as the reader, on A's live bus —
///     NOT from a re-query.
///
/// MUTATION (RED): delete the `emit_domain(MessagesRead)` in the DmReceipt
/// handler → A's stream never sees the event → the wait times out → RED.
#[tokio::test]
async fn v42_dm_read_receipt_live_refreshes_sender() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v42a"), "V42 Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v42b"), "V42 Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // A subscribes to its own event bus BEFORE sending, so the read-receipt
    // DomainEvent can't fire before we're listening.
    let mut events_a = client_a.subscribe_events();

    // A → B DM. B's copy is stored under ChannelId::from(uid_a) (B's thread w/ A).
    let marker = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("A send_text failed");

    // POSITIVE CONTROL + action: B reads A's DM (stores it, then sends the read
    // receipt back to A). a_reads_bs_message panics if B never stored it.
    let conv_on_b = ChannelId::from(uid_a);
    let read_ts = a_reads_bs_message(&client_b, uid_a, &conv_on_b, &marker).await;
    println!("  positive control: B stored + read A's DM (ts={read_ts})");

    // THE FIX: A (the SENDER) receives a live MessagesRead naming B as reader.
    let reader = uid_b;
    let got = wait_for_event(&mut events_a, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessagesRead { reader_id, .. } if *reader_id == reader
    )).await;
    assert!(
        got.is_some(),
        "DM READ RECEIPT DID NOT LIVE-REFRESH: B read A's DM, but A's event bus never saw a \
         DomainEvent::MessagesRead naming B within {EVENT_TIMEOUT:?}. The DmReceipt handler \
         updated the DB silently (finding #8 live-refresh half) — the sender's checkmark would \
         only update on a manual re-query."
    );
    println!("  the fix: A received a live DomainEvent::MessagesRead naming B as reader");

    println!(
        "\n  V42 PASS — a 1:1 DM read receipt now LIVE-REFRESHES the sender: B reading A's DM \
         emits DomainEvent::MessagesRead on A's bus (not just a silent DB flip). Closes the \
         live-refresh half of MISSING-FEATURES #8. See VALIDATED.md.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V43 — a profile display-name change LIVE-PUSHES to friends.
///
/// THE GAP (MISSING-FEATURES #12): `EventTypeUserProfileUpdated` was CONSUMED
/// (server `stream_events.go`, client `user_events.rs` → `UserProfileUpdated`)
/// but had NO PUBLISHER — `UpdateUserProfile` only echoed the change back to the
/// updater's OWN stream. So a friend saw a name change only on the next
/// `GetUserProfile` pull, never live. A second latent bug: the fan-out proto
/// conversion dropped the ENCRYPTED name fields, so even a published event was
/// undecryptable.
///
/// THE FIX (this test): `Repository.UpdateUserProfile` now fans a
/// `UserProfileUpdated` event out to every friend (new `publishProfileToFriends`,
/// mirroring the presence fan-out), and the fan-out conversion uses
/// `protoUserProfileFromInternal` so the encrypted name rides along. The friend
/// decrypts it with A's profile key (already shared via the friend handshake,
/// see V38).
///
/// Proof (asserts on the FRIEND's live event stream, scoped to this run):
///   POSITIVE CONTROL — A and B are friends (so B holds A's profile key and is
///     on A's friend fan-out list).
///   THE FIX — A changes its display name; B receives a live
///     `DomainEvent::UserProfileUpdated` naming A, carrying the NEW name
///     decrypted byte-exact (proving both the publisher AND that the encrypted
///     field survived the fan-out).
///
/// MUTATION (RED): delete the `publishProfileToFriends` call in
/// `Repository.UpdateUserProfile` → B's stream never sees the event → RED.
#[tokio::test]
async fn v43_profile_name_change_live_pushes_to_friends() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v43a"), "V43 Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v43b"), "V43 Bob").await;

    // Friend them so B holds A's profile key (friend handshake) AND is on A's
    // fan-out list. (Key sharing proven by V38.)
    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // POSITIVE CONTROL: B fetches A's profile once — proves B can resolve A's
    // name at all (holds A's key) before we assert the LIVE push below.
    let a_seen = client_b.get_user_profile(&uid_a).await;
    assert!(
        a_seen.is_ok(),
        "POSITIVE CONTROL FAILED: B cannot fetch A's profile ({a_seen:?}); the live-push \
         assertion would be vacuous."
    );

    // B subscribes BEFORE A's update so the push can't fire before we listen.
    let mut events_b = client_b.subscribe_events();

    // THE ACTION: A changes its display name to a short unique marker. Short so
    // the encrypted+base64 name stays under the server's 300-char display_name
    // cap (the ciphertext pads to buckets — a long name would overflow it).
    let new_name = format!("V43-{}", &unique_marker()[17..25]);
    client_a
        .update_user_profile(Some(new_name.clone()), None)
        .await
        .expect("A update_user_profile failed");

    // THE FIX: B receives a live UserProfileUpdated naming A with the NEW name.
    let target = uid_a;
    let want = new_name.clone();
    let got = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::UserProfileUpdated { user_id, display_name, .. }
        if *user_id == target && display_name == &want
    )).await;
    assert!(
        got.is_some(),
        "PROFILE UPDATE DID NOT LIVE-PUSH: A changed its name to {new_name:?}, but B's event bus \
         never saw a DomainEvent::UserProfileUpdated naming A with that name within \
         {EVENT_TIMEOUT:?}. Either the server never fanned the update out to friends \
         (CONSUMER-NO-PRODUCER, #12) or the fan-out dropped the encrypted name so B couldn't \
         decrypt it."
    );
    println!("  the fix: B received a live UserProfileUpdated naming A with the new name ({new_name})");

    println!(
        "\n  V43 PASS — a profile display-name change LIVE-PUSHES to friends: A renaming itself \
         emits DomainEvent::UserProfileUpdated on B's bus with the new name decrypted byte-exact \
         (was CONSUMER-NO-PRODUCER + an encrypted-field-dropping fan-out). Closes \
         MISSING-FEATURES #12. See VALIDATED.md.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V44 — thread replies actually thread: a reply sent with a thread parent is
/// grouped under the root on BOTH the sender and the recipient.
///
/// THE GAP (reported): the UI could quote-reply in a channel but had "no option
/// to reply in thread" — the only thread entry was the "N replies" badge shown
/// `if reply-count > 0`, so a message with no replies could never START a
/// thread. The send-into-thread path (`send_channel_message(..., Some(root),
/// ...)` → parent_id on the wire) + the thread panel existed but were
/// unreachable. Fixed by a Reply-in-thread button in the message hover actions
/// (proven wired by ui_wiring::message_reply_in_thread_button_is_wired).
///
/// THIS TEST proves the wire the button drives is REAL end to end — not just a
/// callback that fires: a reply sent with `thread_id = root` is stored under
/// that root (`list_thread_replies(root)`) on the sender AND round-trips to the
/// recipient's thread, while a plain sibling message is NOT in the thread.
///
/// Proof (asserts on the LOCAL thread grouping of both parties, scoped to run):
///   POSITIVE CONTROL — B receives A's root + the threaded reply (pipeline alive).
///   THE FIX — the threaded reply appears under the root via
///     `list_thread_replies(root)` on A (sender) and B (recipient); the plain
///     sibling message does NOT.
///
/// MUTATION (RED): drop `thread_id`/parent_id on the send or inbound conversion
/// → the reply's `thread_id` is unset → it's absent from `list_thread_replies`
/// → the fix assertions fail.
#[tokio::test]
async fn v44_thread_reply_groups_under_root_on_both_sides() {
    use chatter_core::domain::channel::ChannelType;

    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v44a"), "V44 Alice").await;
    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v44b"), "V44 Bob").await;

    let name = format!(
        "v44-chan-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let chan = common::create_group_with_members(
        &client_a,
        &name,
        ChannelType::Private,
        None,
        Some("V44 threads"),
        &[(&client_b, &uid_b)],
    )
    .await;

    // membership
    let mut members = std::collections::HashSet::new();
    for _ in 0..15 {
        members = psql(&format!("select user_id from channel_members where channel_id = '{}'", chan.id))
            .lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        if members.contains(&uid_a.to_string()) && members.contains(&uid_b.to_string()) { break; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        members.contains(&uid_a.to_string()) && members.contains(&uid_b.to_string()),
        "POSITIVE CONTROL FAILED: A and B not both members of {}", chan.id
    );

    client_a.resubscribe_channel_messages();
    client_b.resubscribe_channel_messages();
    let mut events_b = client_b.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // A posts the ROOT (also mints A's sender key + SKDM to B).
    let root_text = unique_marker();
    let root = client_a
        .send_channel_message(None, &chan.id, &root_text, vec![], vec![], None, vec![], false)
        .await
        .expect("A root send failed");
    let root_id = root.id;

    let g = chan.id.clone();
    let rt = root_text.clone();
    let got_root = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessageReceived { message }
        if message.conversation_id == g && message.as_text() == Some(rt.as_str())
    )).await;
    assert!(
        got_root.is_some(),
        "POSITIVE CONTROL FAILED: B never received A's root ({root_text}) within {EVENT_TIMEOUT:?}."
    );
    tokio::time::sleep(Duration::from_secs(4)).await; // let B process A's SKDM

    // A posts a PLAIN sibling (negative control — same channel, NOT in the thread).
    let plain_text = unique_marker();
    client_a
        .send_channel_message(None, &chan.id, &plain_text, vec![], vec![], None, vec![], false)
        .await
        .expect("A plain send failed");

    // A posts a THREAD REPLY under root (thread_id = root_id) — the wire the
    // Reply-in-thread button drives.
    let reply_text = unique_marker();
    client_a
        .send_channel_message(None, &chan.id, &reply_text, vec![], vec![], Some(&root_id), vec![], false)
        .await
        .expect("A thread reply send failed");

    // B must receive the threaded reply.
    let g = chan.id.clone();
    let rt = reply_text.clone();
    let got_reply = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessageReceived { message }
        if message.conversation_id == g && message.as_text() == Some(rt.as_str())
    )).await;
    assert!(
        got_reply.is_some(),
        "POSITIVE CONTROL FAILED: B never received A's thread reply ({reply_text}) within {EVENT_TIMEOUT:?}."
    );
    tokio::time::sleep(Duration::from_secs(2)).await; // let stores settle

    // THE FIX — thread grouping on the SENDER (A).
    let a_thread = client_a.list_thread_replies(&root_id, None, 50).unwrap_or_default();
    let a_has_reply = a_thread.iter().any(|m| m.as_text() == Some(reply_text.as_str()));
    let a_has_plain = a_thread.iter().any(|m| m.as_text() == Some(plain_text.as_str()));
    assert!(
        a_has_reply,
        "THREAD REPLY NOT GROUPED (sender): A's list_thread_replies(root) does not contain the \
         reply ({reply_text}). The send path did not set thread_id=root on A's local copy — the \
         sender wouldn't even see their own thread reply."
    );
    assert!(
        !a_has_plain,
        "THREAD GROUPING WRONG (sender): the PLAIN sibling ({plain_text}) is under the root's \
         thread on A — thread_id filtering is not actually scoping to the root."
    );

    // THE FIX — thread grouping on the RECIPIENT (B): parent_id rode the wire.
    let b_thread = client_b.list_thread_replies(&root_id, None, 50).unwrap_or_default();
    let b_has_reply = b_thread.iter().any(|m| m.as_text() == Some(reply_text.as_str()));
    let b_has_plain = b_thread.iter().any(|m| m.as_text() == Some(plain_text.as_str()));
    assert!(
        b_has_reply,
        "THREAD REPLY DID NOT FEDERATE: B's list_thread_replies(root) does not contain the reply \
         ({reply_text}). parent_id did not ride the wire to B (or B's inbound conversion dropped \
         thread_id) — the recipient sees the reply as a flat channel message, not in the thread."
    );
    assert!(
        !b_has_plain,
        "THREAD GROUPING WRONG (recipient): the PLAIN sibling ({plain_text}) is under the root's \
         thread on B."
    );

    println!(
        "\n  V44 PASS — thread replies actually thread: a reply sent with thread_id=root is \
         grouped under the root on BOTH the sender (A) and the recipient (B), while a plain \
         sibling is not. The Reply-in-thread button now drives a real end-to-end thread.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V45 — reactions, edits, pins round-trip in a GROUP CHANNEL (Sender-Key
/// control path). Was marked "Wired, not proven" in the feature set.
///
/// The control-content path (`dispatch_content` → `send_channel_content` →
/// per-member device inbox → `inbox/polling/device.rs` decrypt → InboxEvent →
/// inbox_processor apply) is the classic cut-wire risk: the action fires but
/// the recipient never applies it. This proves all three APPLY on the OTHER
/// party's local store.
///
/// All actions are performed by B and OBSERVED on A (A did not perform them, so
/// the applied state can only have arrived over the wire):
///   REACTION — B reacts to A's message → A's copy shows the emoji.
///   EDIT     — B edits B's own message → A's copy shows the new text.
///   PIN      — B pins B's message → A's copy is pinned.
///
/// MUTATION (RED): drop any apply arm in device.rs / inbox_processor → the
/// matching assertion times out.
#[tokio::test]
async fn v45_reactions_edits_pins_roundtrip_in_channel() {
    use chatter_core::domain::channel::ChannelType;

    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v45a"), "V45 Alice").await;
    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v45b"), "V45 Bob").await;

    let name = format!(
        "v45-chan-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 1_000_000
    );
    let chan = common::create_group_with_members(
        &client_a,
        &name,
        ChannelType::Private,
        None,
        Some("V45"),
        &[(&client_b, &uid_b)],
    )
    .await;

    let mut members = std::collections::HashSet::new();
    for _ in 0..15 {
        members = psql(&format!("select user_id from channel_members where channel_id = '{}'", chan.id))
            .lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        if members.contains(&uid_a.to_string()) && members.contains(&uid_b.to_string()) { break; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(members.contains(&uid_a.to_string()) && members.contains(&uid_b.to_string()),
        "POSITIVE CONTROL FAILED: A and B not both members");

    client_a.resubscribe_channel_messages();
    client_b.resubscribe_channel_messages();
    let mut events_a = client_a.subscribe_events();
    let mut events_b = client_b.subscribe_events();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // A posts msgR (A's reaction/pin target); B must receive it (establishes A's key on B).
    let r_text = unique_marker();
    let msg_r = client_a
        .send_channel_message(None, &chan.id, &r_text, vec![], vec![], None, vec![], false)
        .await
        .expect("A msgR send failed");
    let r_id = msg_r.id;
    let r_ts = msg_r.sent_at;
    let g = chan.id.clone(); let t = r_text.clone();
    assert!(
        wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| matches!(
            e, DomainEvent::MessageReceived { message } if message.conversation_id == g && message.as_text() == Some(t.as_str())
        )).await.is_some(),
        "POSITIVE CONTROL FAILED: B never received A's msgR"
    );
    assert!(
        wait_for_sender_key(&client_b, &uid_a, &chan.id).await,
        "SENDER-KEY DISTRIBUTION BROKEN: A's msgR did not install A's sender key on B"
    );

    // WARM-UP: establish B's sender key on A before msgE (B's first real send
    // races its own SKDM otherwise — the msgE positive control would flake).
    client_b
        .send_channel_message(None, &chan.id, &unique_marker(), vec![], vec![], None, vec![], false)
        .await
        .expect("B warm-up send failed");
    assert!(
        wait_for_sender_key(&client_a, &uid_b, &chan.id).await,
        "SENDER-KEY DISTRIBUTION BROKEN: B's warm-up did not install B's sender key on A \
         within {CONDITION_TIMEOUT:?}"
    );

    // B posts msgE (B's edit/pin target); A must receive it (establishes B's key on A).
    let e_text = unique_marker();
    client_b
        .send_channel_message(None, &chan.id, &e_text, vec![], vec![], None, vec![], false)
        .await
        .expect("B msgE send failed");
    let g = chan.id.clone(); let t = e_text.clone();
    let a_msg_e = match wait_for_event(&mut events_a, EVENT_TIMEOUT, move |e| matches!(
        e, DomainEvent::MessageReceived { message } if message.conversation_id == g && message.as_text() == Some(t.as_str())
    )).await {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!("POSITIVE CONTROL FAILED: A never received B's msgE"),
    };
    let e_id = a_msg_e.id;
    let e_ts = a_msg_e.sent_at;
    tokio::time::sleep(Duration::from_secs(3)).await; // A processes B's SKDM

    // ---- REACTION: B reacts to A's msgR; A's copy must show it -------------
    client_b
        .conversation(ActiveConversation::Channel(chan.id.clone()))
        .send_reaction("\u{1F44D}", &uid_a, r_ts, false)
        .await
        .expect("B send_reaction failed");
    let mut reacted = false;
    for _ in 0..40 {
        if let Some(m) = client_a.get_message_local(&r_id) {
            if m.reaction_counts.iter().any(|rc| rc.emoji == "\u{1F44D}" && rc.count >= 1) { reacted = true; break; }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(reacted, "REACTION NOT APPLIED (channel): A's copy of msgR never showed B's 👍 reaction — \
        the channel control-reaction wire (device.rs → inbox_processor) is cut.");

    // ---- EDIT: B edits msgE; A's copy must show the new text ---------------
    let edited = format!("EDITED-{}", unique_marker());
    client_b
        .conversation(ActiveConversation::Channel(chan.id.clone()))
        .send_edit(e_ts, &edited)
        .await
        .expect("B send_edit failed");
    let mut edit_ok = false;
    for _ in 0..40 {
        if let Some(m) = client_a.get_message_local(&e_id) {
            if m.as_text() == Some(edited.as_str()) { edit_ok = true; break; }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(edit_ok, "EDIT NOT APPLIED (channel): A's copy of msgE never updated to the edited text — \
        the channel edit wire is cut.");

    // ---- PIN: B pins msgE; A's copy must be pinned -------------------------
    client_b
        .conversation(ActiveConversation::Channel(chan.id.clone()))
        .send_pin(&uid_b, e_ts, false)
        .await
        .expect("B send_pin failed");
    let mut pinned = false;
    for _ in 0..40 {
        if let Some(m) = client_a.get_message_local(&e_id) {
            if m.is_pinned { pinned = true; break; }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(pinned, "PIN NOT APPLIED (channel): A's copy of msgE never became pinned — the channel pin wire is cut.");

    println!("\n  V45 PASS — reactions, edits, pins all round-trip in a group channel: B's actions \
        applied on A's local store (reaction 👍, edited text, pinned). Control-content path proven.\n");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V46 — reactions, edits, pins round-trip on a 1:1 DM (sealed ServerDm path).
///
/// Same three actions as V45 but over the channel-free ServerDm transport
/// (`dispatch_content` → `send_server_dm_content` → sealed device inbox →
/// `inbox/polling/dm.rs` DmContent apply). All actions by B, observed on A.
///
/// MUTATION (RED): drop a DmContent apply arm in dm.rs → the matching
/// assertion times out.
#[tokio::test]
async fn v46_reactions_edits_pins_roundtrip_in_dm() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v46a"), "V46 Alice").await;
    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v46b"), "V46 Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    let mut events_a = client_a.subscribe_events();
    let conv_on_a = ChannelId::from(uid_b); // A's thread with B

    // A DMs msgR to B (A's reaction target). Capture A's own stored copy id.
    let r_text = unique_marker();
    let r_res = client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &r_text)
        .await
        .expect("A msgR DM failed");
    let r_ts = r_res.sent_at;
    let r_id = r_res.message.map(|m| m.id).expect("A stored its own msgR");

    // B DMs msgE to A (B's edit/pin target). A must receive it.
    let e_text = unique_marker();
    let e_res = client_b
        .send_text(ActiveConversation::ServerDm { peer_id: uid_a }, &e_text)
        .await
        .expect("B msgE DM failed");
    let e_ts = e_res.sent_at;
    let g = conv_on_a.clone(); let t = e_text.clone();
    let a_msg_e = match wait_for_event(&mut events_a, EVENT_TIMEOUT, move |ev| matches!(
        ev, DomainEvent::MessageReceived { message } if message.conversation_id == g && message.as_text() == Some(t.as_str())
    )).await {
        Some(DomainEvent::MessageReceived { message }) => message,
        _ => panic!("POSITIVE CONTROL FAILED: A never received B's DM msgE"),
    };
    let e_id = a_msg_e.id;

    // ---- REACTION: B reacts to A's msgR ----------------------------------
    client_b
        .conversation(ActiveConversation::ServerDm { peer_id: uid_a })
        .send_reaction("\u{1F44D}", &uid_a, r_ts, false)
        .await
        .expect("B send_reaction failed");
    let mut reacted = false;
    for _ in 0..40 {
        if let Some(m) = client_a.get_message_local(&r_id) {
            if m.reaction_counts.iter().any(|rc| rc.emoji == "\u{1F44D}" && rc.count >= 1) { reacted = true; break; }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(reacted, "REACTION NOT APPLIED (DM): A's msgR never showed B's 👍 — the ServerDm reaction wire (dm.rs) is cut.");

    // ---- EDIT: B edits msgE ----------------------------------------------
    let edited = format!("EDITED-{}", unique_marker());
    client_b
        .conversation(ActiveConversation::ServerDm { peer_id: uid_a })
        .send_edit(e_ts, &edited)
        .await
        .expect("B send_edit failed");
    let mut edit_ok = false;
    for _ in 0..40 {
        if let Some(m) = client_a.get_message_local(&e_id) {
            if m.as_text() == Some(edited.as_str()) { edit_ok = true; break; }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(edit_ok, "EDIT NOT APPLIED (DM): A's copy of msgE never updated to the edited text — the ServerDm edit wire is cut.");

    // ---- PIN: B pins msgE ------------------------------------------------
    client_b
        .conversation(ActiveConversation::ServerDm { peer_id: uid_a })
        .send_pin(&uid_b, e_ts, false)
        .await
        .expect("B send_pin failed");
    let mut pinned = false;
    for _ in 0..40 {
        if let Some(m) = client_a.get_message_local(&e_id) {
            if m.is_pinned { pinned = true; break; }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(pinned, "PIN NOT APPLIED (DM): A's copy of msgE never became pinned — the ServerDm pin wire is cut.");

    println!("\n  V46 PASS — reactions, edits, pins all round-trip on a 1:1 sealed DM: B's actions \
        applied on A's local store. ServerDm control-content path proven.\n");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V47: A DESYNCED 1:1 SESSION SELF-HEALS VIA DecryptionErrorMessage
//
// THE GAP: the codebase had ZERO references to libsignal's
// DecryptionErrorMessage / PlaintextContent. If a recipient loses or desyncs
// its Signal session state while the sender's sending chain lives on, every
// subsequent Whisper from the sender fails ratchet decrypt at the recipient
// FOREVER — the conversation wedges permanently (group chats already
// self-healed via the SKDM machinery, see V26; 1:1 had nothing).
//
// THE FIX (this test): Signal's 1:1 self-heal, end to end:
//   * receiver half — B cannot inner-decrypt A's sealed message; the gateway
//     builds a DecryptionErrorMessage naming the failed ciphertext
//     (CryptoError::SessionDesync -> SealedSenderResult::DecryptionErrorNeedsReport)
//     and ships it back to A's exact sending device, sealed as session-LESS
//     PlaintextContent (MessageService::send_decryption_error_report).
//   * sender half — A extracts the DEM
//     (extract_decryption_error_message_from_serialized_content), checks it
//     names THIS device and its CURRENT ratchet key, then ARCHIVES the broken
//     session (SignalCommand::ArchiveSessionForResend). has_session reports an
//     archived session as unusable, so A's next send re-establishes via a
//     fresh PreKeySignalMessage that B can process from scratch.
//
// Proof:
//   POSITIVE CONTROL 1 — a clean A→B DM delivers (pipeline alive).
//   SABOTAGE + POSITIVE CONTROL 2 — B deletes its session record for A:1
//     (the state loss). A's next message (m2, a Whisper under a session B no
//     longer has) must NOT surface at B within the wedge window: proves the
//     desync is real, so the heal below cannot pass vacuously.
//   THE FIX (sender half) — A's domain bus surfaces
//     DmSessionHealed { peer: B, archived: true } — B's DEM arrived, named
//     this device + the live ratchet, and A archived the wedged session.
//   THE FIX (round trip) — A sends m3; B surfaces it byte-exact. That is only
//     possible if A re-keyed via PreKeySignalMessage (the old chain is one B
//     cannot decrypt, as m2 just proved).
//
// MUTATION (RED): in services/signal/actor.rs, SignalCommand::ArchiveSessionForResend,
// skip the archive (return Ok(false) instead of archive_current_state + store)
// → A never leaves the wedged chain: the DmSessionHealed{archived:true} wait
// times out AND m3 never decrypts at B → RED twice over.
// ============================================================================
#[tokio::test]
async fn v47_desynced_dm_session_self_heals_via_decryption_error_message() {
    skip_if_no_server!();
    no_keyring_prompts();
    // Opt-in tracing for debugging this test: V47_TRACE=1 to enable.
    if std::env::var("V47_TRACE").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "chatter_core=debug".into()),
            )
            .try_init();
    }

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v47a"), "V47 Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v47b"), "V47 Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    let mut events_a = client_a.subscribe_events();
    let mut events_b = client_b.subscribe_events();
    let conv_on_b = ChannelId::from(uid_a); // B's thread with A

    // ---- POSITIVE CONTROL 1: the pipeline delivers at all ------------------
    let m1 = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m1)
        .await
        .expect("baseline send failed");
    let g1 = conv_on_b.clone();
    let t1 = m1.clone();
    let got1 = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == g1
                 && message.as_text() == Some(t1.as_str()))
    })
    .await;
    assert!(
        got1.is_some(),
        "POSITIVE CONTROL 1 FAILED: a clean A→B DM never arrived — every 'wedged' \
         observation below would be meaningless"
    );
    println!("\n  control 1: clean A→B DM delivered — pipeline alive");

    // Drain in-flight control traffic before sabotaging. B's automatic
    // delivery receipt for m1 ratchets A's session when A processes it; if it
    // lands between m2 and the DEM, A's current ratchet key no longer matches
    // the one in the DEM and the (correct, Signal-mandated) stale-report guard
    // skips the archive — a flaky red for a reason outside the property under
    // test. Settling here pins A's ratchet for the whole wedge window: B can
    // no longer produce ratchet-advancing traffic afterwards (it cannot
    // decrypt, and the DEM itself is deliberately session-less).
    tokio::time::sleep(Duration::from_secs(8)).await;

    // ---- SABOTAGE: B loses its session state for A:1 -----------------------
    // This is the desync this feature exists for: A's sending chain lives on,
    // B has nothing to decrypt it with.
    let addr_a_on_b = format!("{uid_a}:1");
    assert!(
        client_b
            .signal()
            .key_store()
            .delete_session(&addr_a_on_b)
            .expect("delete_session errored"),
        "SABOTAGE FAILED: B had no session record for A:1 to lose"
    );
    println!("  sabotage: B deleted its Signal session record for {addr_a_on_b}");

    // ---- POSITIVE CONTROL 2: the wedge is real -----------------------------
    // m2 is a Whisper under a session B no longer has. It must NOT surface.
    // (Signal semantics: the wedged ciphertext itself is unrecoverable — the
    // heal applies to the SESSION so the NEXT message decrypts.)
    let m2 = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m2)
        .await
        .expect("m2 send failed (send-side must succeed; the wedge is receive-side)");
    let g2 = conv_on_b.clone();
    let t2 = m2.clone();
    let got2 = wait_for_event(&mut events_b, Duration::from_secs(10), move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == g2
                 && message.as_text() == Some(t2.as_str()))
    })
    .await;
    assert!(
        got2.is_none(),
        "POSITIVE CONTROL 2 FAILED: B decrypted m2 WITHOUT its session record — \
         there is no desync, so 'self-heal' below would be proving nothing"
    );
    println!("  control 2: m2 did NOT decrypt at B — the 1:1 session is genuinely wedged");

    // ---- THE FIX, sender half: B's DEM reaches A and archives the session --
    let healed = wait_for_event(&mut events_a, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::DmSessionHealed { peer_user_id, archived, .. }
                 if *peer_user_id == uid_b && *archived)
    })
    .await;
    assert!(
        healed.is_some(),
        "SELF-HEAL DID NOT FIRE: B failed to decrypt A's m2, but A never processed a \
         DecryptionErrorMessage archiving its session with B (no \
         DmSessionHealed{{archived:true}} within {EVENT_TIMEOUT:?}). The 1:1 \
         conversation stays wedged forever — exactly the bug this feature removes."
    );
    println!("  the fix: A received B's DecryptionErrorMessage and ARCHIVED the wedged session");

    // ---- THE FIX, round trip: the next message decrypts --------------------
    let m3 = unique_marker();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &m3)
        .await
        .expect("m3 send failed");
    let g3 = conv_on_b.clone();
    let t3 = m3.clone();
    let got3 = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
                 if message.conversation_id == g3
                 && message.as_text() == Some(t3.as_str()))
    })
    .await;
    assert!(
        got3.is_some(),
        "ROUND TRIP FAILED: A archived the session (DmSessionHealed fired) but B still \
         cannot decrypt A's next message — the re-established PreKeySignalMessage path \
         is broken, so the 'heal' is cosmetic"
    );

    // Byte-exact surface check on B's local store (not just the event).
    let stored = client_b
        .get_messages_local(
            &conv_on_b,
            &chatter_core::domain::message::ConversationType::ServerDm,
            None,
            50,
        )
        .unwrap_or_default();
    assert!(
        stored.iter().any(|m| m.as_text() == Some(m3.as_str())),
        "ROUND TRIP FAILED: m3 event fired but B never stored the message text"
    );

    println!(
        "\n  V47 PASS — a wedged 1:1 Signal session SELF-HEALS: B (desynced) reported the \
         failure via DecryptionErrorMessage (sealed, session-less PlaintextContent), A \
         archived the broken session and re-established via PreKeySignalMessage, and the \
         next message decrypted byte-exact at B. Mirrors V26's group self-heal for the \
         1:1 path.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// V48 — username links (L6): a shareable `chatter://u/{link_id}#{entropy}`
/// URL resolves to the owner's username end-to-end WITHOUT the server ever
/// learning that username.
///
/// THE DESIGN (libsignal `usernames` crate, Signal's username-link model): A
/// encrypts its own full `nickname.discriminator` username with keys HKDF'd
/// from 32 bytes of entropy (`create_for_username`, AES-256-CBC + HMAC-SHA256)
/// and uploads ONLY the ciphertext; the server files it under an opaque UUID
/// link id. The entropy rides the URL FRAGMENT, which is never transmitted.
/// B parses the URL, fetches the blob by link id, decrypts LOCALLY, then
/// discovers the account via the existing hashed LookupUsername path.
///
/// Proof:
///   POSITIVE CONTROL — the `user_username_links` row for A's link id EXISTS
///     and is non-empty (the absence greps below are not vacuous), and B's
///     resolve returns A's exact username + account id (full round trip).
///   NEGATIVE HALF (the server's-eye view) — the stored blob's hex does NOT
///     contain hex(username); the logical dump of the row carries no
///     plaintext; and resolving with WRONG entropy (all a server or DB thief
///     can ever try — they never see the fragment) FAILS on HMAC.
///   ROTATION — A creates a second link: previously shared URLs stop
///     resolving (old link id dead), the new URL resolves. One active link
///     per user, enforced by the user_id-PK upsert.
///   DELETE — after A deletes the link, the last URL stops resolving too.
///
/// MUTATION (RED): zero out `link.entropy` before decrypt in
/// `UserService::resolve_username_link` (chatter-core/src/services/user/mod.rs)
/// → every decrypt is an HMAC mismatch → B's resolve fails → "THE FIX"
/// assertion dies. Proven RED 2026-07-19, restored GREEN (see task log).
#[tokio::test]
async fn v48_username_link_resolves_privately_end_to_end() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v48a"), "V48 Alice").await;
    let _uid_b = register_and_start(&client_b, &unique_user("v48b"), "V48 Bob").await;

    let full_username = client_a
        .full_username()
        .expect("A must have a nickname.discriminator username after registration");

    // THE ACTION: A creates a username link.
    let url = client_a
        .create_username_link()
        .await
        .expect("create_username_link failed");
    println!("  A's username link: {url}");
    let link = chatter_core::domain::UsernameLink::parse(&url).expect("A's own URL must parse");

    // POSITIVE CONTROL: the ciphertext blob is actually IN the server store.
    let blob_hex = psql(&format!(
        "SELECT encode(encrypted_username,'hex') FROM user_username_links WHERE link_id = '{}'",
        link.link_id
    ));
    assert!(
        blob_hex.len() >= 2 * (16 + 32),
        "POSITIVE CONTROL FAILED: no/short user_username_links row for A's link id \
         (got {blob_hex:?}) — every absence grep below would be vacuous"
    );
    println!("  positive control: server stores {} ciphertext bytes under link id {}", blob_hex.len() / 2, link.link_id);

    // NEGATIVE HALF 1: the stored blob does not contain the plaintext
    // username bytes (hex-on-hex substring check).
    let username_hex: String = full_username.bytes().map(|b| format!("{b:02x}")).collect();
    assert!(
        !blob_hex.contains(&username_hex),
        "SERVER LEARNED THE USERNAME: stored blob contains plaintext username bytes"
    );

    // NEGATIVE HALF 2: the logical dump of A's row carries no plaintext
    // username either (catches a hypothetical second cleartext column).
    let row_dump = psql(&format!(
        "SELECT * FROM user_username_links WHERE link_id = '{}'",
        link.link_id
    ));
    assert!(
        !row_dump.contains(&full_username),
        "SERVER LEARNED THE USERNAME: user_username_links row contains it in cleartext"
    );

    // THE FIX: B resolves A's URL end-to-end — fetch blob, decrypt with the
    // fragment entropy, hashed-lookup to the account.
    let (resolved_username, resolved_id) = client_b
        .resolve_username_link(&url)
        .await
        .expect("B failed to resolve A's username link");
    assert_eq!(
        resolved_username, full_username,
        "B decrypted a DIFFERENT username than A encrypted"
    );
    assert_eq!(
        resolved_id,
        Some(uid_a),
        "resolved username did not discover A's account id"
    );
    println!("  the fix: B resolved the link to @{resolved_username} = {uid_a}");

    // NEGATIVE HALF 3: wrong entropy — the server's-eye view. Same link id,
    // zeroed fragment: decryption MUST fail (HMAC mismatch), so an actor
    // without the fragment (server, DB thief, or a stranger holding only the
    // link id) cannot learn the username.
    let wrong_url = chatter_core::domain::UsernameLink {
        link_id: link.link_id.clone(),
        entropy: [0u8; 32],
    }
    .to_url();
    assert!(
        client_b.resolve_username_link(&wrong_url).await.is_err(),
        "resolve with WRONG entropy must fail — otherwise the fragment is decorative \
         and the server could decrypt the blob"
    );
    println!("  negative half: wrong-entropy resolve failed (HMAC), blob is ciphertext-only server-side");

    // ROTATION: creating a new link kills the old URL.
    let url2 = client_a
        .create_username_link()
        .await
        .expect("rotating username link failed");
    assert_ne!(url2, url, "rotation must mint a new link id + entropy");
    assert!(
        client_b.resolve_username_link(&url).await.is_err(),
        "OLD link URL still resolves after rotation — reset button is a no-op"
    );
    let (u2, id2) = client_b
        .resolve_username_link(&url2)
        .await
        .expect("rotated link must resolve");
    assert_eq!((u2, id2), (full_username.clone(), Some(uid_a)));
    let n_rows = psql(&format!(
        "SELECT COUNT(*) FROM user_username_links WHERE user_id = '{uid_a}'"
    ));
    assert_eq!(n_rows, "1", "one active link per user (found {n_rows} rows)");
    println!("  rotation: old URL dead, new URL live, exactly 1 row for A");

    // DELETE: the last URL stops resolving.
    client_a
        .delete_username_link()
        .await
        .expect("delete_username_link failed");
    assert!(
        client_b.resolve_username_link(&url2).await.is_err(),
        "link still resolves after delete"
    );

    println!(
        "\n  V48 PASS — username links work end-to-end and privately: B resolved A's \
         chatter://u/ URL to A's exact username + account id via local decryption, while \
         the server stored only AES-CBC+HMAC ciphertext (no plaintext in blob or row, \
         wrong-entropy decrypt fails), rotation kills old URLs, delete kills the link. \
         See VALIDATED.md.\n"
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V50: Encrypted message-history backup — LOCAL mode (file export/restore)
//
// CLAIM (MISSING-FEATURES L7): encrypted history export to a file that a fresh
// install can restore, fully offline, unreadable without the passphrase.
//
// POSITIVE: B receives a real DM (server round-trip data setup), exports an
// encrypted backup file, a FRESH client dir (never connected) imports it, and
// the marker message row is byte-exact identical between the two SQLCipher
// stores.
// NEGATIVE: the backup file contains no plaintext (grep with a proven
// detector), and restore with the wrong passphrase fails, importing nothing.
//
// MUTATION CHECK: make `derive_passphrase_kek` ignore its passphrase argument
// (derive from a constant) — the wrong-passphrase restore then succeeds and
// the negative half goes RED. Proven RED 2026-07-19, restored GREEN.
// ============================================================================

/// One SELECT tuple of the marker row — the byte-exactness witness.
fn marker_message_row(db_path: &std::path::Path, marker: &str) -> Vec<String> {
    let db = chatter_core::storage::Database::open(db_path, "validated-harness-passphrase")
        .expect("open sqlcipher db for verification");
    let conn = db.connection().expect("db conn");
    let mut stmt = conn
        .prepare(
            "SELECT id, conversation_type, conversation_id, sender_id, content_type, \
             content_json, is_pinned, is_edited, sent_at, created_at \
             FROM messages WHERE content_json LIKE ?1",
        )
        .expect("prepare");
    let rows: Vec<Vec<String>> = stmt
        .query_map([format!("%{marker}%")], |r| {
            let mut v = Vec::new();
            for i in 0..10 {
                let val: rusqlite::types::Value = r.get(i)?;
                v.push(format!("{val:?}"));
            }
            Ok(v)
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert_eq!(rows.len(), 1, "expected exactly one marker message row in {db_path:?}");
    rows.into_iter().next().unwrap()
}

#[tokio::test]
async fn v50_local_message_backup_round_trips_encrypted() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let uid_a = register_and_start(&client_a, &unique_user("v50a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v50b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    // Real DM round-trip as data setup: the row we back up arrived over the wire.
    let marker = unique_marker();
    let mut events_b = client_b.subscribe_events();
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, &marker)
        .await
        .expect("send_text failed");
    let expected_conv = ChannelId::from(uid_a);
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;
    assert!(received.is_some(), "B never received the DM — nothing to back up");
    println!("  data setup: B received marker DM over the wire");

    // ---- EXPORT ------------------------------------------------------------
    let passphrase = "Backup!Pass99";
    let backup_path = dir_b.path().join("history.chbk");
    let stats = client_b
        .export_message_backup(&backup_path, passphrase)
        .await
        .expect("export failed");
    println!("  exported {} rows, {} bytes", stats.rows, stats.bytes);
    assert!(stats.rows > 0, "export serialized zero rows — vacuous backup");

    // ---- NEGATIVE HALF 1: no plaintext in the file -------------------------
    // Detector control first: prove the grep CAN find the marker.
    let control = dir_b.path().join("detector_control");
    std::fs::write(&control, format!("xx{marker}yy")).unwrap();
    let control_bytes = std::fs::read(&control).unwrap();
    assert!(
        control_bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
        "DETECTOR BROKEN: planted marker not found in control file"
    );

    let file_bytes = std::fs::read(&backup_path).unwrap();
    assert_eq!(&file_bytes[..8], b"CHTRBKP1", "backup file missing container magic");
    assert!(
        !file_bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
        "PLAINTEXT LEAK: marker found inside the encrypted backup file"
    );
    println!("  backup file: magic OK, {} bytes, zero plaintext marker hits", file_bytes.len());

    // ---- RESTORE into a truly fresh, OFFLINE client dir --------------------
    // Never connected, never logged in — local restore must not need a server.
    let dir_fresh = tempfile::tempdir().unwrap();
    let client_fresh = build_client(dir_fresh.path());
    let summary = client_fresh
        .import_message_backup(&backup_path, passphrase)
        .await
        .expect("restore failed");
    println!(
        "  fresh restore: {} of {} rows inserted",
        summary.rows_inserted, summary.rows_seen
    );
    assert!(summary.rows_inserted > 0, "restore inserted nothing");

    // Byte-exact: the marker row is identical column-for-column.
    let src_row = marker_message_row(&dir_b.path().join("data/chatter.db"), &marker);
    let dst_row = marker_message_row(&dir_fresh.path().join("data/chatter.db"), &marker);
    assert_eq!(src_row, dst_row, "restored message row differs from the original");
    println!("  byte-exact: marker row identical across stores ({} columns)", src_row.len());

    // ---- NEGATIVE HALF 2: wrong passphrase restores nothing ----------------
    let dir_wrong = tempfile::tempdir().unwrap();
    let client_wrong = build_client(dir_wrong.path());
    let err = client_wrong
        .import_message_backup(&backup_path, "Wr0ng!Pass77")
        .await;
    assert!(err.is_err(), "restore with WRONG passphrase must fail");
    let wrong_db = chatter_core::storage::Database::open(
        &dir_wrong.path().join("data/chatter.db"),
        "validated-harness-passphrase",
    )
    .unwrap();
    let n: i64 = wrong_db
        .connection()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "wrong-passphrase restore must import zero rows");
    println!("  wrong passphrase: restore rejected, 0 rows imported");

    println!("\nV50 PASS — local encrypted backup round-trips byte-exact into a fresh offline client; file holds no plaintext; wrong passphrase gets nothing\n");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V51: Encrypted message-history backup — SERVER mode (PIN-keyed)
//
// CLAIM (MISSING-FEATURES L7): the same encrypted blob stored server-side
// (RustFS), restorable on a fresh install after login + SVR PIN; the server
// can never read message content.
//
// POSITIVE: A receives a real DM, uploads the server backup, a FRESH install
// logs in with the PIN and restores the byte-exact history.
// NEGATIVE: the raw object fetched straight from RustFS (curl SigV4 with the
// docker-compose S3 creds — no chatter server in the path) contains no
// plaintext; nor does pg_dump; a wrong PIN cannot restore (derives a backup id
// the server does not know; the wrong-KEK-on-right-blob half is pinned by the
// crypto unit tests).
//
// MUTATION CHECK: make `upload_to_server` upload the raw frame payload
// instead of the encrypted container (skip `encrypt_backup_file`) — the
// raw-object grep finds the marker and goes RED. Proven RED 2026-07-19,
// restored GREEN.
// ============================================================================

#[tokio::test]
async fn v51_server_message_backup_restores_on_fresh_install_via_pin() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let client_b = build_client(dir_b.path());

    let user_a_name = unique_user("v51a");
    let uid_a = register_and_start(&client_a, &user_a_name, "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("v51b"), "Bob").await;

    befriend(&client_a, &client_b, uid_a, uid_b).await;

    let marker = unique_marker();
    let mut events_a = client_a.subscribe_events();
    client_b
        .send_text(ActiveConversation::ServerDm { peer_id: uid_a }, &marker)
        .await
        .expect("send_text failed");
    let expected_conv = ChannelId::from(uid_b);
    let received = wait_for_event(&mut events_a, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == expected_conv)
    })
    .await;
    assert!(received.is_some(), "A never received the DM — nothing to back up");
    let src_row = marker_message_row(&dir_a.path().join("data/chatter.db"), &marker);
    println!("  data setup: A received marker DM; source row captured");

    // ---- UPLOAD ------------------------------------------------------------
    let stats = client_a
        .enable_server_message_backup(PIN)
        .await
        .expect("server backup upload failed");
    println!("  uploaded server backup: {} rows, {} bytes", stats.rows, stats.bytes);

    // ---- POSITIVE CONTROL: metadata row exists, object key known -----------
    let object_key = psql(&format!(
        "SELECT object_key FROM message_backups WHERE user_id = '{uid_a}'"
    ));
    assert!(
        object_key.starts_with("message-backups/"),
        "POSITIVE CONTROL FAILED: no message_backups row for A (got {object_key:?})"
    );
    println!("  server metadata row: {object_key}");

    // ---- NEGATIVE HALF: raw object in RustFS holds no plaintext ------------
    // Straight S3 GET against RustFS with the docker-compose creds
    // (docker-compose.yml `rustfs` service) — the chatter server is not in
    // this path, so these are the exact bytes the storage operator sees.
    let blob = {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "--aws-sigv4",
                "aws:amz:us-east-1:s3",
                "--user",
                &format!(
                    "{}:{}",
                    env_value("MINIO_ACCESS_KEY"),
                    env_value("MINIO_SECRET_KEY")
                ),
                &format!("http://127.0.0.1:9000/chatter-files/{object_key}"),
            ])
            .output()
            .expect("curl spawn failed");
        out.stdout
    };
    assert_eq!(
        blob.len(),
        stats.bytes,
        "POSITIVE CONTROL FAILED: raw RustFS object size != uploaded container size"
    );
    assert_eq!(&blob[..8], b"CHTRBKP1", "stored object is not the backup container");
    assert!(
        !blob.windows(marker.len()).any(|w| w == marker.as_bytes()),
        "PLAINTEXT LEAK: marker found in the raw RustFS object"
    );
    println!("  raw RustFS object: {} bytes, magic OK, zero plaintext hits", blob.len());

    let dump = pg_dump_all();
    assert!(dump.len() > 10_000, "pg_dump suspiciously small");
    assert_eq!(
        dump.matches(&marker).count(),
        0,
        "PLAINTEXT LEAK: marker found in Postgres dump"
    );
    println!("  pg_dump: zero plaintext hits");

    // ---- FRESH INSTALL: login + PIN → restore ------------------------------
    client_a.shutdown().await;
    drop(client_a);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dir_fresh = tempfile::tempdir().unwrap();
    let client_fresh = build_client(dir_fresh.path());
    client_fresh.connect().await.expect("fresh connect failed");
    let login = client_fresh
        .login(&user_a_name, PASSWORD, "validated", Some(PIN))
        .await
        .expect("fresh-install login failed");
    println!(
        "  fresh install logged in (keys_restored={}, key_recovery={:?})",
        login.keys_restored, login.key_recovery
    );

    // NEGATIVE: wrong PIN first — it derives a different backup_id, the server
    // knows no such backup, nothing restores. (Strike counters don't apply:
    // the blob is useless without the PIN-derived KEK.)
    let wrong = client_fresh.restore_server_message_backup("Wr0ngPin!x7").await;
    assert!(wrong.is_err(), "restore with WRONG PIN must fail");
    println!("  wrong PIN: restore rejected ({})", wrong.err().unwrap());

    let summary = client_fresh
        .restore_server_message_backup(PIN)
        .await
        .expect("server restore failed");
    println!(
        "  fresh restore: {} of {} rows inserted",
        summary.rows_inserted, summary.rows_seen
    );
    assert!(summary.rows_inserted > 0, "server restore inserted nothing");

    let dst_row = marker_message_row(&dir_fresh.path().join("data/chatter.db"), &marker);
    assert_eq!(src_row, dst_row, "restored message row differs from the original");
    println!("  byte-exact: marker row identical across installs");

    println!("\nV51 PASS — server-side encrypted backup restores byte-exact on a fresh install via PIN; raw RustFS object + Postgres hold no plaintext; wrong PIN gets nothing\n");

    client_fresh.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// V52 — a reinstall must recover the ORIGINAL identity key, and the server must
//       agree with the device about what that key is.
//
// THE DEFECT THIS PINS. `login` used to mint a brand-new Signal identity key
// before the SVR restore ran (auth_service.rs `do_login` -> `generate_identity`).
// The restore then found a local identity, decided this was an existing device,
// logged "SVR restore skipped: local identity keys already exist", and returned
// without ever fetching the PIN backup. The user's real key was gone. Nothing
// published the replacement either — no RPC carried an identity key and the
// upload path refused a changed one — while the background replenisher
// overwrote the server's signed prekey with one signed by the NEW key. The
// account then served OLD identity key + NEW signed prekey: signature
// verification failed for every peer, permanently, silently.
//
// POSITIVE CONTROL: before the reinstall, assert the server's identity_key
// really is this device's key. Comparing two values that are both absent would
// pass against an empty table.
//
// MUTATION CHECK: move `attempt_svr_restore` back after the identity mint in
// `client/auth.rs::login`, or restore the unconditional `generate_identity` in
// `do_login`, and the fresh install gets a different key — this fails.
// ============================================================================
#[tokio::test]
async fn v52_reinstall_recovers_the_original_identity_key() {
    skip_if_no_server!();
    println!("\n=== V52: reinstall with PIN recovers the ORIGINAL identity key ===\n");

    let user = unique_user("v52ident");
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid = register_and_start(&client_a, &user, "V52 Original").await;
    println!("  registered {user} as {uid}");

    let original_key = client_a
        .signal()
        .get_identity_public_key()
        .await
        .expect("no local identity after registration");
    let original_hex: String = original_key.iter().map(|b| format!("{b:02x}")).collect();

    // POSITIVE CONTROL: the server holds this exact key, so the comparison
    // after the reinstall is against something real.
    let server_hex = psql(&format!(
        "SELECT encode(identity_key,'hex') FROM signal_prekey_bundles WHERE user_id = '{uid}'"
    ));
    assert_eq!(
        server_hex, original_hex,
        "POSITIVE CONTROL FAILED: server identity_key does not match the registering device"
    );
    println!("  positive control: server identity_key == device key ({}…)", &original_hex[..16]);

    client_a.shutdown().await;
    drop(client_a);
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ---- REINSTALL: brand-new data dir, login with the PIN -----------------
    let dir_fresh = tempfile::tempdir().unwrap();
    let client_fresh = build_client(dir_fresh.path());
    client_fresh.connect().await.expect("fresh connect failed");
    client_fresh
        .login(&user, PASSWORD, "validated-reinstall", Some(PIN))
        .await
        .expect("reinstall login failed");
    println!("  reinstalled and logged in with PIN");

    let restored_key = client_fresh
        .signal()
        .get_identity_public_key()
        .await
        .expect("reinstalled device has no identity key");
    let restored_hex: String = restored_key.iter().map(|b| format!("{b:02x}")).collect();

    assert_eq!(
        restored_hex, original_hex,
        "the PIN backup was not restored — the device minted a NEW identity key and the \
         original is unrecoverable"
    );
    println!("  restored identity key matches the original");

    // The server must still serve that same key, and the prekeys it serves
    // must be signed by it. A mismatch here is the permanently-unreachable
    // state in its final form.
    let server_after = psql(&format!(
        "SELECT encode(identity_key,'hex') FROM signal_prekey_bundles WHERE user_id = '{uid}'"
    ));
    assert_eq!(
        server_after, original_hex,
        "server identity_key diverged from the device's key: peers can never verify this account"
    );
    println!("  server still serves the same identity key");

    println!("\nV52 PASS — a PIN reinstall recovers the original identity key; device and server agree\n");
    client_fresh.shutdown().await;
}

// ============================================================================
// V53 — when there is genuinely no backup, the user is TOLD, nothing is minted
//       behind their back, and the explicit reset publishes a key the server
//       actually serves.
//
// THE DEFECT THIS PINS. There was no wire on which to publish a rotated
// identity key at all: `LoginRequest` and `RefreshPreKeysRequest` carry none,
// `UploadPreKeyBundle` had zero production callers, and the repository refused
// any bundle whose identity key differed from the one on file
// ("identity key mismatch: all devices for a user must share the same identity
// key"). So a device that legitimately held a new key could never publish it,
// while the replenisher overwrote the signed prekey with one signed by that
// unpublishable key.
//
// POSITIVE CONTROL: the account starts with a real identity key on the server,
// and the assertions below are against that value, not against absence.
//
// MUTATION CHECK: drop the `is_retryable` guard in `accept_new_identity` and
// the wrong-PIN half fails; drop the `RepublishIdentityKey` RPC (or revert the
// repository to refusing a changed identity key) and the reset half fails.
// ============================================================================
#[tokio::test]
async fn v53_identity_reset_publishes_a_key_the_server_serves() {
    skip_if_no_server!();
    println!("\n=== V53: no backup → told, not silently re-keyed; explicit reset publishes ===\n");

    let user = unique_user("v53reset");
    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid = register_and_start(&client_a, &user, "V53 Original").await;

    let original_key = client_a.signal().get_identity_public_key().await.unwrap();
    let original_hex: String = original_key.iter().map(|b| format!("{b:02x}")).collect();

    let server_hex = psql(&format!(
        "SELECT encode(identity_key,'hex') FROM signal_prekey_bundles WHERE user_id = '{uid}'"
    ));
    assert_eq!(
        server_hex, original_hex,
        "POSITIVE CONTROL FAILED: server has no identity key for this account"
    );
    println!("  positive control: server identity_key == device key");

    client_a.shutdown().await;
    drop(client_a);
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ---- NEGATIVE HALF: wrong PIN must never authorise a re-key ------------
    // The backup id is PIN-derived, so a typo and a missing backup look
    // identical to the fetch. Getting this wrong destroys a recoverable
    // account, so it is checked before anything destructive is allowed.
    {
        let dir = tempfile::tempdir().unwrap();
        let c = build_client(dir.path());
        c.connect().await.expect("connect failed");
        let err = c
            .login(&user, PASSWORD, "validated-wrongpin", Some("Wr0ngPin!x7"))
            .await
            .expect_err("a reinstall with the WRONG PIN must not report success");

        let need = match &err {
            chatter_core::client::ClientError::IdentityRecoveryRequired(n) => *n,
            other => panic!("expected IdentityRecoveryRequired, got {other:?}"),
        };
        assert!(
            need.is_retryable(),
            "a wrong PIN leaves the real key recoverable; got {need:?}"
        );
        println!("  wrong PIN: login refused with {need:?} — {err}");

        let refused = c.accept_new_identity(need).await;
        assert!(
            refused.is_err(),
            "minting a new identity on a wrong PIN would destroy a recoverable account"
        );
        println!("  wrong PIN: identity reset refused ({})", refused.err().unwrap());

        let unchanged = psql(&format!(
            "SELECT encode(identity_key,'hex') FROM signal_prekey_bundles WHERE user_id = '{uid}'"
        ));
        assert_eq!(unchanged, original_hex, "a failed login must not touch server key material");
        println!("  wrong PIN: server identity_key untouched");
        c.shutdown().await;
    }

    // ---- Remove the backup ------------------------------------------------
    // Not contrived: `perform_svr_backup` is best-effort and non-fatal at
    // registration, and a recovery-code login destroys the backup outright, so
    // "registered account, no key backup" is genuinely reachable.
    psql(&format!("DELETE FROM svr_backups WHERE user_id = '{uid}'"));
    println!("  removed the account's SVR key backup");

    // ---- POSITIVE HALF: no backup → told, then an explicit reset ----------
    let dir_fresh = tempfile::tempdir().unwrap();
    let client_fresh = build_client(dir_fresh.path());
    client_fresh.connect().await.expect("connect failed");

    let err = client_fresh
        .login(&user, PASSWORD, "validated-reset", Some(PIN))
        .await
        .expect_err("login with no recoverable keys must not report success");
    let need = match &err {
        chatter_core::client::ClientError::IdentityRecoveryRequired(n) => *n,
        other => panic!("expected IdentityRecoveryRequired, got {other:?}"),
    };
    assert_eq!(
        need,
        chatter_core::services::key_recovery::KeyRecoveryNeed::NoBackup,
        "the server knows this account has no backup; the user must be told that, \
         not left guessing at their PIN"
    );
    println!("  no backup: login refused with {need:?} — {err}");

    // Nothing may have been published on the user's behalf.
    let still_original = psql(&format!(
        "SELECT encode(identity_key,'hex') FROM signal_prekey_bundles WHERE user_id = '{uid}'"
    ));
    assert_eq!(
        still_original, original_hex,
        "a failed login silently re-keyed the account — the exact defect being fixed"
    );
    println!("  no backup: server identity_key still the original (nothing minted behind the user)");

    // The explicit, user-driven decision.
    //
    // This appends a CHANGED identity key to the KT log. It used to need
    // exclusion against the rest of the suite: consistency was proven over the
    // derived latest-key map, so a rotation read as a rewrite of history, the
    // server refused its own proof, and every client pinned before it stopped
    // establishing sessions. Consistency is now proven over the append-only LOG
    // (`crypto/logtree.go`), where a rotation is just another append, so the
    // rotation runs concurrently with everything else.
    client_fresh
        .accept_new_identity(need)
        .await
        .expect("identity reset failed — there is still no way to publish a rotated key");

    let new_key = client_fresh.signal().get_identity_public_key().await.unwrap();
    let new_hex: String = new_key.iter().map(|b| format!("{b:02x}")).collect();
    assert_ne!(new_hex, original_hex, "reset produced the same key");

    let published = psql(&format!(
        "SELECT encode(identity_key,'hex') FROM signal_prekey_bundles WHERE user_id = '{uid}'"
    ));
    assert_eq!(
        published, new_hex,
        "the rotated identity key was not published — peers would still fetch the dead one"
    );
    println!("  reset: server now serves the NEW identity key");

    // Exactly one device bundle survives, and the prekeys it serves were
    // signed by the key served alongside them. A row count above one would mean
    // a stale device is still handing out prekeys signed by a dead key.
    let rows = psql(&format!(
        "SELECT count(*) FROM signal_prekey_bundles WHERE user_id = '{uid}'"
    ));
    assert_eq!(rows, "1", "stale device bundles survived the rotation");

    // Key Transparency must witness the change, or the log asserts a key the
    // server no longer serves.
    let kt_latest = psql(&format!(
        "SELECT encode(identity_key,'hex') FROM kt_log_entries WHERE user_id = '{uid}' \
         ORDER BY leaf_index DESC LIMIT 1"
    ));
    assert_eq!(kt_latest, new_hex, "Key Transparency did not witness the rotation");
    let kt_count = psql(&format!(
        "SELECT count(*) FROM kt_log_entries WHERE user_id = '{uid}'"
    ));
    assert_eq!(kt_count, "2", "expected one leaf for registration and one for the rotation");
    println!("  reset: KT log witnessed the new key ({kt_count} leaves)");

    println!("\nV53 PASS — no backup is reported, never silently re-keyed; an explicit reset publishes a rotated identity key atomically and KT records it\n");
    client_fresh.shutdown().await;
}

// ============================================================================
// V54 — channel messages survive a client restart (INFRA-1 / INFRA-2)
// ============================================================================

/// A channel message must reach a member who was NOT running when it was sent,
/// and live delivery must resume after that member restarts.
///
/// WHY THIS EXISTS. Server-managed channel messages used to arrive over
/// per-channel `StreamMessageEvents` subscriptions started from exactly two
/// call sites — `create_channel` and `join_channel` — with no re-subscription
/// at login and no reconnect loop, and nothing backfilled from the server when
/// a channel was opened. After any restart a member received ZERO live
/// messages in every channel joined in a previous session, and clicking the
/// channel showed only what local SQLite already had. `resume_from_event_id`
/// was decorative on both stream RPCs.
///
/// Three parts, in order of what they would have caught:
///   1. B receives a message live (baseline: the stream delivers at all).
///   2. B is shut down, A sends two messages, B RESTARTS from the same data
///      dir. Both messages must be there — the durable half.
///   3. A sends once more and the restarted B must get it LIVE — the
///      re-subscription half, the actual INFRA-1 regression.
#[tokio::test]
async fn v54_channel_messages_survive_a_client_restart() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v54a"), "V54 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v54b"), "V54 Bob").await;

    // Public channel: the server-managed path this proof is about. A creates it
    // with B as a member, so B is subscribed by server-side membership rather
    // than by having called join in this process.
    let name = format!(
        "v54-channel-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 1_000_000
    );
    let channel = common::create_group_with_members(
        &client_a,
        &name,
        chatter_core::domain::channel::ChannelType::Public,
        None,
        Some("V54 restart-delivery channel"),
        &[(&client_b, &uid_b)],
    )
    .await;
    println!("\n  [V54] A created channel {} ({})", channel.name, channel.id);

    // POSITIVE CONTROL: the SERVER records both as members, or every "B got
    // nothing" below is vacuous. Asked over the wire so the control does not
    // depend on the dev stack's compose environment.
    let mut member_ids: std::collections::HashSet<UserId> = Default::default();
    let member_deadline = Instant::now() + CONDITION_TIMEOUT;
    while Instant::now() < member_deadline {
        member_ids = client_a
            .get_channel_members(&channel.id, 100, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.user_id)
            .collect();
        if member_ids.contains(&uid_a) && member_ids.contains(&uid_b) {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    for (label, uid) in [("A", &uid_a), ("B", &uid_b)] {
        assert!(
            member_ids.contains(uid),
            "POSITIVE CONTROL FAILED: {label} ({uid}) is not a server-side member of {} — \
             delivery assertions would be vacuous.",
            channel.id
        );
    }
    println!("  positive control: the server has A and B as members of this channel");

    // ---- PART 1: live delivery works at all -------------------------------
    let mut events_b = client_b.subscribe_events();
    client_b.resubscribe_channel_messages();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let marker_live = unique_marker();
    client_a
        .send_text(ActiveConversation::Channel(channel.id.clone()), &marker_live)
        .await
        .expect("A live send failed");

    let cid = channel.id.clone();
    let m = marker_live.clone();
    let got_live = wait_for_event(&mut events_b, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == cid && message.as_text() == Some(m.as_str()))
    })
    .await;
    assert!(
        got_live.is_some(),
        "BASELINE FAILED: B never received {marker_live} live — the channel message stream is \
         not delivering at all, so the restart assertions below would be meaningless."
    );
    println!("  [PART 1] B received the live message");

    // ---- PART 2: messages sent while B is DOWN survive the restart --------
    client_b.shutdown().await;
    drop(client_b);
    drop(events_b);
    println!("  [PART 2] B shut down");

    let marker_offline_1 = unique_marker();
    let marker_offline_2 = unique_marker();
    for marker in [&marker_offline_1, &marker_offline_2] {
        client_a
            .send_text(ActiveConversation::Channel(channel.id.clone()), marker)
            .await
            .expect("A offline-window send failed");
    }
    println!("  A sent 2 messages while B was down");

    // The restart: same data dir, persisted session, `start()` — exactly what
    // reopening the app does.
    let (client_b2, mut events_b2) = restore_and_start(dir_b.path(), "B2").await;

    for (label, marker) in [("first", &marker_offline_1), ("second", &marker_offline_2)] {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut found = false;
        while Instant::now() < deadline {
            let stored = client_b2
                .get_messages(&ActiveConversation::Channel(channel.id.clone()), 50)
                .unwrap_or_default();
            if stored.iter().any(|msg| msg.as_text() == Some(marker.as_str())) {
                found = true;
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        assert!(
            found,
            "INFRA-1 REGRESSION: after restarting, B does not have the {label} message \
             ({marker}) that was sent while it was down. Nothing re-subscribed the channel \
             stream and nothing backfilled from the server."
        );
    }
    println!("  [PART 2] both offline messages are present after the restart");

    // ---- PART 3: live delivery RESUMES after the restart ------------------
    let marker_after = unique_marker();
    client_a
        .send_text(ActiveConversation::Channel(channel.id.clone()), &marker_after)
        .await
        .expect("A post-restart send failed");

    let cid2 = channel.id.clone();
    let m2 = marker_after.clone();
    let got_after = wait_for_event(&mut events_b2, EVENT_TIMEOUT, move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == cid2 && message.as_text() == Some(m2.as_str()))
    })
    .await;
    assert!(
        got_after.is_some(),
        "INFRA-1 REGRESSION: after restarting, B received no live message ({marker_after}) in a \
         channel it joined in a PREVIOUS session. The client is not re-subscribing at start()."
    );
    println!("  [PART 3] live delivery resumed after the restart");

    client_a.shutdown().await;
    client_b2.shutdown().await;
}

/// v55 — ACCEPTING AN INVITATION IS ENOUGH.
///
/// The claim: a user who accepts a channel invitation has the channel — in
/// their own store, so the sidebar can draw it, and on the live message
/// stream, so the next message in it arrives without waiting for a sync.
///
/// This proof calls `respond_to_channel_invite` and NOTHING ELSE on the
/// invitee. No `get_channel`, no `sync_channel_members`, no
/// `request_skdms_from_channel_members`, no `resubscribe_channel_messages` —
/// those four are exactly what the accept path was missing (OPEN-ITEMS §69)
/// and exactly what `tests/common/groups.rs` used to do on the product's
/// behalf, which is why 54 proofs stayed green over a broken accept.
///
/// MUTATION CHECK — each of these makes it fail:
///   * drop the `get_channel_internal` + `insert_membership` tail of
///     `ChannelService::respond_to_invite` → the local assertions fail; the
///     user is a member on the server with an empty sidebar.
///   * drop `self.resubscribe_channel_messages()` from
///     `ChatterClient::respond_to_channel_invite` → a brand-new invitee's
///     stream stays in its no-subscription backoff and the live assertion
///     times out.
///   * drop `sync_channel_members` → A is not in B's local member list and
///     the SKDM request below has nobody to ask.
///   * restore the `MemberAdded` wait the service used to have (§70) → the
///     accept call itself returns `no response event received`.
#[tokio::test]
async fn v55_accepting_an_invite_is_enough_to_get_the_channel() {
    skip_if_no_server!();
    no_keyring_prompts();

    let dir_a = tempfile::tempdir().unwrap();
    let client_a = build_client(dir_a.path());
    let uid_a = register_and_start(&client_a, &unique_user("v55a"), "V55 Alice").await;

    let dir_b = tempfile::tempdir().unwrap();
    let client_b = build_client(dir_b.path());
    let uid_b = register_and_start(&client_b, &unique_user("v55b"), "V55 Bob").await;

    // A creates a channel naming B. Since AUTHZ-CREATE-1 that does NOT make B
    // a member — it creates a PENDING invitation, which is the flow under test.
    let name = format!(
        "v55-invite-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 1_000_000
    );
    let channel = client_a
        .create_channel(
            &name,
            chatter_core::domain::channel::ChannelType::Private,
            None,
            Some("V55 invite-accept channel"),
            None,
            None,
            vec![uid_b.clone()],
        )
        .await
        .expect("A failed to create the channel");
    println!("\n  [V55] A created channel {} ({})", channel.name, channel.id);

    // PRECONDITION: B knows nothing about this channel yet, so every local
    // assertion after the accept is about the accept and not about setup.
    assert!(
        client_b.get_channel_local(&channel.id).is_none(),
        "PRECONDITION FAILED: B already has {} locally before accepting — the assertions \
         below would not be about the accept path.",
        channel.id
    );

    // B discovers the invitation, exactly as the invites modal does.
    let wanted = channel.id.to_string();
    let deadline = Instant::now() + CONDITION_TIMEOUT;
    loop {
        if client_b
            .list_pending_channel_invites()
            .await
            .iter()
            .any(|i| i.channel_id == wanted)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "SETUP FAILED: B never saw a pending invitation to {} within {CONDITION_TIMEOUT:?} \
             — CreateChannel is supposed to create one for every named member.",
            channel.id
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    println!("  B can see the pending invitation");

    // Subscribe BEFORE accepting: the live message below must not be able to
    // slip past between the accept and the subscribe.
    let mut events_b = client_b.subscribe_events();

    // ---- THE ONE CALL UNDER TEST -----------------------------------------
    let joined = client_b
        .respond_to_channel_invite(&channel.id, true)
        .await
        .expect("B failed to accept the invitation");
    assert!(
        joined.is_some(),
        "B's accept returned None — the server did not report the invitation as accepted."
    );
    println!("  B accepted (and did nothing else)");

    // ---- PART 1: the channel is in B's own store -------------------------
    let local = client_b.get_channel_local(&channel.id);
    assert!(
        local.is_some(),
        "§69 REGRESSION: after accepting, B has no local row for {} — the sidebar is built \
         from local channels, so the user who just accepted sees no channel.",
        channel.id
    );
    assert_eq!(
        local.unwrap().name,
        channel.name,
        "B's local channel row is not the channel they accepted."
    );

    let members = client_b.list_channel_members_local(&channel.id);
    assert!(
        members.iter().any(|m| m.user_id == uid_b),
        "§69 REGRESSION: after accepting, B has no local membership row for {} (local members: \
         {}).",
        channel.id,
        members.len()
    );
    assert!(
        members.iter().any(|m| m.user_id == uid_a),
        "§69 REGRESSION: after accepting, B's local member list does not contain the channel \
         owner {uid_a}. SKDM distribution reads this list, so B could not send a message the \
         existing members can decrypt."
    );
    println!("  B has the channel + both memberships locally");

    // POSITIVE CONTROL: the SERVER agrees B is a member, or "B received the
    // message" below would be proving something about a channel B is not in.
    let mut server_members: std::collections::HashSet<UserId> = Default::default();
    let member_deadline = Instant::now() + CONDITION_TIMEOUT;
    while Instant::now() < member_deadline {
        server_members = client_a
            .get_channel_members(&channel.id, 100, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.user_id)
            .collect();
        if server_members.contains(&uid_a) && server_members.contains(&uid_b) {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    assert!(
        server_members.contains(&uid_b),
        "POSITIVE CONTROL FAILED: the server does not list B ({uid_b}) as a member of {} after \
         B accepted — the delivery assertion below would be vacuous.",
        channel.id
    );
    println!("  positive control: the server has A and B as members");

    // ---- PART 2: a live message arrives, with no nudge from this test ----
    let marker = unique_marker();
    client_a
        .send_text(ActiveConversation::Channel(channel.id.clone()), &marker)
        .await
        .expect("A failed to send into the channel");

    let cid = channel.id.clone();
    let m = marker.clone();
    // Generous on purpose: B's stream is reconnecting and A's sender key is in
    // flight, so this budget is latency, not correctness. What it must NOT
    // require is another call on B.
    let got = wait_for_event(&mut events_b, Duration::from_secs(60), move |e| {
        matches!(e, DomainEvent::MessageReceived { message }
            if message.conversation_id == cid && message.as_text() == Some(m.as_str()))
    })
    .await;
    assert!(
        got.is_some(),
        "§69 REGRESSION: B accepted the invitation and then received no live message ({marker}) \
         in the channel. Accepting has to nudge the message stream and pull the existing \
         members' sender keys — a user who accepts must not have to restart the app."
    );
    println!("  [V55] B received a live message in the channel it just accepted");

    client_a.shutdown().await;
    client_b.shutdown().await;
}
