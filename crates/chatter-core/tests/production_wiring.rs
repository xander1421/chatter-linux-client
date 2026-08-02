//! PRODUCTION WIRING — does the security control have a caller?
//!
//! The worst defects in this codebase were not wrong implementations. They were
//! *correct* implementations that nothing called:
//!
//!   * `accept_peer_identity_change` — merged and reported as the fix for the
//!     identity-change wedge while being unreachable from any shipped path.
//!   * the mesh announce trust policy — `evaluate_announce` sat with zero
//!     production callers, so peers in radio range were unauthenticated, while
//!     `announce.rs`'s own unit tests were a wall of green.
//!   * `verify_consistency_proof` — called only from `tests/validated.rs`, which
//!     is why V25 read as "the client verifies the log is append-only" when no
//!     client code did. Wired on 2026-08-03; the entry below is now `Wired`, and
//!     this file is what will notice if that caller ever disappears.
//!
//! Every one of those had passing unit tests. Unit tests prove a function does
//! what it says; they cannot prove anyone asks. Nothing in the suite failed when
//! the last production caller went away, so nothing reported it.
//!
//! This file closes that. It reads the crate sources, deletes `#[cfg(test)]`
//! blocks and comments, and counts real call sites. It is deliberately crude —
//! textual, not a call graph — because the alternative that exists today is
//! nothing at all, and the defect it catches is coarse: the caller count going
//! to zero.
//!
//! Two directions, both load-bearing:
//!
//!   `WIRED` — the control has a production caller and must keep one. Delete the
//!   call and this test goes red. This is the regression guard.
//!
//!   `DEAD` — the control has NO production caller today, and that is a KNOWN
//!   DEFECT recorded here rather than in a comment nobody reads. Wire one up and
//!   this test goes red too, on purpose: whoever fixes it is told to move the
//!   entry and correct the claim in `VALIDATED.md` / `CLAUDE.md`, so the docs
//!   cannot drift back out of step with the code. A green `DEAD` entry is not an
//!   endorsement — it is an open bug with a test holding its place.
//!
//! Runs with no server, no `INTEGRATION_TESTS`, and no feature flags, unlike the
//! V-series, which skips silently when the stack is absent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ============================================================================
// The table
// ============================================================================

/// What the entry asserts about a symbol's production call sites.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expect {
    /// At least one production caller outside the defining file.
    Wired,
    /// Zero production callers outside the defining file — a recorded defect.
    Dead,
}

struct Entry {
    /// Function name, matched as `name(` after `#[cfg(test)]` and comments are
    /// stripped.
    symbol: &'static str,
    /// Path suffix of the file that DEFINES it. Call sites inside this file are
    /// not counted: a helper calling itself proves nothing about reachability
    /// from the shipped application.
    defined_in: &'static str,
    expect: Expect,
    /// What breaks in production if this entry's expectation flips.
    why: &'static str,
}

const TABLE: &[Entry] = &[
    // ---- Signal: sealed sender ------------------------------------------
    Entry {
        symbol: "unratcheted_plaintext_rejected",
        defined_in: "chatter-core/src/services/gateway/decryption.rs",
        expect: Expect::Wired,
        why: "A `CiphertextMessageType::Plaintext` sealed-sender message skipped the \
              Double Ratchet: no is_trusted_identity, no replay protection, no forward \
              secrecy. If no path outside decryption.rs refuses it, unratcheted \
              attacker-chosen bytes reach `Content::decode` looking exactly like \
              ratchet-authenticated plaintext. This is the leak ab0a1242 fixed on four \
              paths at once.",
    },
    // ---- Signal: key transparency ---------------------------------------
    Entry {
        symbol: "verify_kt_proof",
        defined_in: "chatter-core/src/crypto/mod.rs",
        expect: Expect::Wired,
        why: "Inclusion-proof verification on the send path (services/signal/session.rs). \
              Lose this caller and Key Transparency is decorative again — a server-forged \
              identity key is accepted silently. That was finding F9; V14/V24/V37 all \
              call the verifier DIRECTLY, so none of them would notice.",
    },
    Entry {
        symbol: "verify_consistency_proof",
        defined_in: "chatter-core/src/crypto/keytransparency.rs",
        expect: Expect::Wired,
        why: "The append-only/fork check. Called from services/signal/kt_anchor.rs, which \
              services/signal/session.rs invokes on every KT-verified send. Lose this \
              caller and inclusion proofs are verified against whatever head the server \
              supplied in the same response — each one valid, all of them possibly from \
              different trees — so a fork, a rollback and a per-user split view all become \
              undetectable again. Dead until 2026-08-03; that is why this file exists.",
    },
    // ---- Signal: identity change ----------------------------------------
    Entry {
        symbol: "note_identity_change",
        defined_in: "chatter-core/src/services/signal/identity.rs",
        expect: Expect::Wired,
        why: "Parks a peer's identity change in `pending_identity_changes` (v063) from \
              the four send paths in session.rs. Without it the client silently encrypts \
              to a ratchet the peer no longer holds.",
    },
    Entry {
        symbol: "get_pending_identity_change",
        defined_in: "chatter-core/src/storage/key_store.rs",
        expect: Expect::Wired,
        why: "session.rs:172 consults it before every send and refuses a stale session \
              with UntrustedIdentity. Lose this caller and the refusal stops happening.",
    },
    Entry {
        symbol: "accept_peer_identity_change",
        defined_in: "chatter-core/src/services/signal/identity.rs",
        expect: Expect::Wired,
        why: "FIXED — this entry was recorded as DEAD and this test caught it coming \
              alive, which is the outcome it exists for. It is the ONLY code that \
              DELETEs a `pending_identity_changes` row. While unwired, production wrote \
              rows (note_identity_change) and read them (session.rs, returning \
              UntrustedIdentity) but could never clear one, so a peer reinstall wedged \
              that conversation permanently. Now reached from ChatterClient and the \
              in-conversation accept prompt. If this loses its caller again the wedge \
              comes straight back, silently — that is what this entry now guards.",
    },
    Entry {
        symbol: "pending_identity_change",
        defined_in: "chatter-core/src/services/signal/identity.rs",
        expect: Expect::Wired,
        why: "FIXED alongside the entry above. This is the read-back API that SHOWS the \
              user a pending change; while it was unwired the wedge was not merely \
              unfixable but invisible. Now read by the conversation's identity-change \
              prompt. Losing this caller would hide the prompt while still refusing to \
              send — the worst of both states.",
    },
    // ---- Signal: session recovery ---------------------------------------
    Entry {
        symbol: "archive_current_state",
        defined_in: "NONE (libsignal SessionRecord method)",
        expect: Expect::Wired,
        why: "Signal requires session ARCHIVAL, not deletion, on a DecryptionErrorMessage \
              (GRA-607). If the last caller goes, the 1:1 self-heal V47 proves either \
              stops happening or degrades to a hard delete that destroys in-flight \
              messages.",
    },
    Entry {
        symbol: "delete_sessions_for_user",
        defined_in: "chatter-core/src/storage/key_store.rs",
        expect: Expect::Wired,
        why: "InvalidatePeerSessions — the only way a peer's stale sessions are dropped \
              so the next send re-fetches their bundle.",
    },
    // ---- Mesh: peer authentication --------------------------------------
    Entry {
        symbol: "evaluate_announce",
        defined_in: "chatter-mesh-link/src/announce.rs",
        expect: Expect::Wired,
        why: "The whole inbound announce trust policy, including the load-bearing check \
              that the sender ID equals SHA256(announced noise key)[..8]. This function \
              HAD zero production callers — mesh peers were unauthenticated on the live \
              path while announce.rs's unit tests passed. It is wired now \
              (services/mesh/ble.rs); this entry stops it regressing. Note that \
              CLAUDE.md still describes it as dead.",
    },
    Entry {
        symbol: "verify_packet",
        defined_in: "chatter-mesh-noise/src/sign.rs",
        expect: Expect::Wired,
        why: "The Ed25519 check actually used on the live mesh path. Without it an \
              announce is an unauthenticated stranger claiming a nickname and two keys.",
    },
    Entry {
        symbol: "upsert_verified_announce",
        defined_in: "chatter-mesh-link/src/registry.rs",
        expect: Expect::Wired,
        why: "Records a peer only AFTER verification. bitchat drops an unverified \
              announce outright; recording one would be identity poisoning.",
    },
    Entry {
        symbol: "verify_announce",
        defined_in: "chatter-mesh-noise/src/sign.rs",
        expect: Expect::Dead,
        why: "KNOWN DEAD, and a trap for a reader. `sign_announce`/`verify_announce`/\
              `canonical_announce_bytes` are a complete, tested, and entirely unused \
              announce-specific signature scheme. Production authenticates announces \
              with the PACKET-level `verify_packet` instead. Anyone citing the \
              announce preimage as what protects the mesh is reading dead code.",
    },
    Entry {
        symbol: "plan_announce_response",
        defined_in: "chatter-mesh-link/src/announce.rs",
        expect: Expect::Dead,
        why: "KNOWN DEAD. The second half of the ported bitchat announce handler \
              (notify-connected, initial sync, announce-back, afterglow). None of that \
              follow-up behaviour happens in production even though evaluate_announce \
              is live.",
    },
];

// ============================================================================
// Source scanning
// ============================================================================

/// Workspace `crates/` directory, derived from this crate's manifest dir.
fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("chatter-core lives under crates/")
        .to_path_buf()
}

fn rust_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(crates_dir()).expect("read crates/") {
        let src = entry.expect("dir entry").path().join("src");
        if src.is_dir() {
            walk(&src, &mut out);
        }
    }
    assert!(
        out.len() > 100,
        "expected to find the workspace sources under {}, found {} files — the path \
         derivation is wrong and this test is asserting nothing",
        crates_dir().display(),
        out.len()
    );
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strip `//` comments, respecting string literals, so a function NAMED in a doc
/// comment is never mistaken for a call to it. Several of these symbols are
/// discussed at length in comments right next to their call sites.
fn strip_comments(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            out.push(c);
            escaped = true;
        } else if c == '"' {
            in_string = !in_string;
            out.push(c);
        } else if !in_string && c == '/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Count `{` and net brace depth on a line, ignoring braces inside strings and
/// after a `//`.
fn braces(line: &str) -> (i32, i32) {
    let code = strip_comments(line);
    let mut opens = 0;
    let mut net = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in code.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_string = !in_string;
        } else if !in_string {
            if c == '{' {
                opens += 1;
                net += 1;
            } else if c == '}' {
                net -= 1;
            }
        }
    }
    (opens, net)
}

/// Drop every `#[cfg(test)]` item, brace-matched. Without this the unit tests
/// beside a dead function would count as callers and the whole table would be
/// vacuously green — which is precisely the illusion this file exists to break.
fn strip_cfg_test(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut kept = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("#[cfg(test)]") {
            let mut j = i + 1;
            let mut depth = 0;
            let mut started = false;
            while j < lines.len() {
                let (opens, net) = braces(lines[j]);
                if opens > 0 {
                    started = true;
                }
                depth += net;
                j += 1;
                if started && depth <= 0 {
                    break;
                }
            }
            i = j;
            continue;
        }
        kept.push(lines[i].to_string());
        i += 1;
    }
    kept
}

/// Is `symbol` CALLED on this line? A definition (`fn symbol`) is not a call, and
/// neither is `use path::symbol;` — a call needs the paren.
fn is_call_site(code: &str, symbol: &str) -> bool {
    if code.contains(&format!("fn {symbol}")) {
        return false;
    }
    let mut rest = code;
    while let Some(pos) = rest.find(symbol) {
        let before_ok = pos == 0
            || !rest[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = rest[pos + symbol.len()..].trim_start();
        if before_ok && after.starts_with('(') {
            return true;
        }
        rest = &rest[pos + symbol.len()..];
    }
    false
}

/// Production call sites of `symbol`, excluding its defining file.
fn production_callers(symbol: &str, defined_in: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for path in rust_sources() {
        let display = path.to_string_lossy().replace('\\', "/");
        if defined_in != "NONE (libsignal SessionRecord method)" && display.ends_with(defined_in) {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read source");
        for (idx, line) in strip_cfg_test(&src).iter().enumerate() {
            let code = strip_comments(line);
            if is_call_site(&code, symbol) {
                hits.push(format!("{display}:{} | {}", idx + 1, line.trim()));
            }
        }
    }
    hits
}

// ============================================================================
// The tests
// ============================================================================

/// A control with no caller is not a control.
///
/// MUTATION: delete the `evaluate_announce(` call at `services/mesh/ble.rs`, or
/// the `verify_kt_proof` calls in `services/signal/session.rs`. This test goes
/// red naming the symbol. Nothing else in the suite does — `announce.rs`'s own
/// unit tests and V14/V24/V37 all call those functions directly.
#[test]
fn security_controls_have_at_least_one_production_caller() {
    let mut unwired: Vec<String> = Vec::new();

    for entry in TABLE.iter().filter(|e| e.expect == Expect::Wired) {
        let callers = production_callers(entry.symbol, entry.defined_in);
        if callers.is_empty() {
            unwired.push(format!(
                "\n  {} — ZERO PRODUCTION CALLERS outside {}\n      {}",
                entry.symbol, entry.defined_in, entry.why
            ));
        }
    }

    assert!(
        unwired.is_empty(),
        "A security control lost its last production caller. It still compiles, its own \
         unit tests still pass, and it now does nothing:\n{}\n\nIf the call moved rather \
         than vanished, update `defined_in`. If the control was deliberately retired, \
         delete the entry AND the function — do not leave it reachable-looking.",
        unwired.join("\n")
    );
}

/// The recorded dead wires are still dead — and if one gets fixed, the docs get
/// fixed with it.
///
/// MUTATION: add any production call to `accept_peer_identity_change` outside
/// `services/signal/identity.rs`. This test goes red and tells you to move the
/// entry to `Expect::Wired` and correct the claim in the docs.
#[test]
fn recorded_dead_wires_have_not_silently_come_alive() {
    let mut revived: Vec<String> = Vec::new();

    for entry in TABLE.iter().filter(|e| e.expect == Expect::Dead) {
        let callers = production_callers(entry.symbol, entry.defined_in);
        if !callers.is_empty() {
            revived.push(format!(
                "\n  {} — now has {} production caller(s):\n      {}\n      RECORDED AS: {}",
                entry.symbol,
                callers.len(),
                callers.join("\n      "),
                entry.why
            ));
        }
    }

    assert!(
        revived.is_empty(),
        "A control recorded here as DEAD now has a production caller. That is good news \
         and a required doc change, not a test to silence:\n{}\n\n  1. Flip the entry to \
         `Expect::Wired` so the new caller can never silently vanish.\n  2. Correct the \
         claim in VALIDATED.md / CLAUDE.md that says this path is unreachable.",
        revived.join("\n")
    );
}

/// Every production consumer of a sealed-sender decrypt must refuse
/// `CiphertextMessageType::Plaintext`.
///
/// `SealedSenderManager::decrypt` returns Plaintext contents VERBATIM — those
/// bytes never entered `message_decrypt_*`. The carve-out exists only for the
/// DecryptionErrorMessage ride-along. It leaked to three other paths
/// (`gateway/decryption.rs` process_dm_message, `services/pairwise.rs`,
/// `inbox/processor.rs`) and stayed leaked until someone read all four by hand.
///
/// A file-level check is coarse on purpose: every one of those three files had
/// NO mention of `Plaintext` anywhere, so this would have caught all three. The
/// expected-set assertion is the other half — a NEW consumer file must justify
/// itself rather than inherit the carve-out by default.
///
/// MUTATION: delete the `if msg_type == CiphertextMessageType::Plaintext` guard
/// from `services/pairwise.rs`. This test goes red; no other test in the crate
/// notices, because the only existing coverage
/// (`sealed_sender_decrypt_labels_unratcheted_plaintext`) asserts the LABEL is
/// produced, never that a caller acts on it.
#[test]
fn every_sealed_sender_decrypt_consumer_refuses_unratcheted_plaintext() {
    /// Files that consume a sealed-sender decrypt and are known to refuse
    /// Plaintext. A new entry here is a deliberate security review, not a typo
    /// fix.
    const EXPECTED_CONSUMERS: &[&str] = &[
        "chatter-core/src/services/gateway/decryption.rs",
        "chatter-core/src/services/pairwise.rs",
        "chatter-core/src/inbox/processor.rs",
        // Plumbing: forwards the (result, type) tuple to the consumers above and
        // never interprets the bytes itself.
        "chatter-core/src/services/signal/actor.rs",
    ];

    let mut consumers: BTreeMap<String, bool> = BTreeMap::new();

    for path in rust_sources() {
        let display = path.to_string_lossy().replace('\\', "/");
        let src = std::fs::read_to_string(&path).expect("read source");
        let kept = strip_cfg_test(&src);
        let code: Vec<String> = kept.iter().map(|l| strip_comments(l)).collect();

        let decrypts = code.iter().any(|l| {
            is_call_site(l, "decrypt_message") || l.contains("SealedSenderManager::decrypt(")
        });
        if !decrypts {
            continue;
        }
        // The primitive itself is where the carve-out is DEFINED; it labels
        // rather than refuses.
        if display.ends_with("chatter-core/src/crypto/signal/sealed_sender.rs") {
            continue;
        }
        let refuses = code
            .iter()
            .any(|l| l.contains("CiphertextMessageType::Plaintext"));
        consumers.insert(display, refuses);
    }

    let unguarded: Vec<&String> = consumers
        .iter()
        .filter(|(_, refuses)| !**refuses)
        .map(|(f, _)| f)
        .collect();
    assert!(
        unguarded.is_empty(),
        "These files decrypt a sealed-sender message but never mention \
         `CiphertextMessageType::Plaintext`, so unratcheted attacker-chosen bytes reach \
         them looking exactly like ratchet-authenticated plaintext:\n  {}\n\nRefuse the \
         type (see `unratcheted_plaintext_rejected`) unless the file is the \
         DecryptionErrorMessage handler.",
        unguarded
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let found: Vec<String> = consumers.keys().cloned().collect();
    let unexpected: Vec<&String> = found
        .iter()
        .filter(|f| !EXPECTED_CONSUMERS.iter().any(|e| f.ends_with(e)))
        .collect();
    assert!(
        unexpected.is_empty(),
        "A NEW sealed-sender decrypt consumer appeared:\n  {}\n\nIt mentions Plaintext, \
         so it probably refuses it — confirm that it does, then add it to \
         EXPECTED_CONSUMERS. This assertion exists because the last time a consumer was \
         added, the Plaintext carve-out was inherited by accident on three paths.",
        unexpected
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    for expected in EXPECTED_CONSUMERS {
        assert!(
            found.iter().any(|f| f.ends_with(expected)),
            "{expected} no longer calls a sealed-sender decrypt. If the path was removed \
             that is fine — drop it from EXPECTED_CONSUMERS. If it was refactored, make \
             sure the refusal moved with it."
        );
    }
}

/// The scanner itself must be able to fail.
///
/// A source-grep test that silently matches nothing is worse than no test: it is
/// a green light wired to nothing. This is the V0 detector self-check applied to
/// this file — prove the machinery finds a caller that IS there, and does not
/// invent one that is not.
#[test]
fn scanner_self_check() {
    let real = production_callers("evaluate_announce", "chatter-mesh-link/src/announce.rs");
    assert!(
        !real.is_empty(),
        "positive control failed: evaluate_announce IS called from services/mesh/ble.rs, \
         so a scanner that reports zero here is broken, and every Dead verdict in this \
         file is meaningless"
    );

    let fictional = production_callers("no_such_function_exists_anywhere", "NONE");
    assert!(
        fictional.is_empty(),
        "negative control failed: the scanner matched a function that does not exist"
    );

    // A call that appears ONLY inside `#[cfg(test)]` must not count, or every
    // dead function with unit tests would read as wired.
    let src = "\
fn f() { real_call(); }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { test_only_call(); }
}
";
    let kept = strip_cfg_test(src).join("\n");
    assert!(kept.contains("real_call"), "stripper ate production code");
    assert!(
        !kept.contains("test_only_call"),
        "stripper left a #[cfg(test)] call in place — dead functions with unit tests \
         would then read as wired, which is the exact illusion this file exists to break"
    );

    // A name in a comment is not a call.
    assert!(
        !is_call_site(&strip_comments("// see evaluate_announce() for the policy"), "evaluate_announce"),
        "a function named in a comment counted as a caller"
    );
    assert!(
        !is_call_site("fn evaluate_announce(", "evaluate_announce"),
        "the definition counted as a call to itself"
    );
    assert!(
        is_call_site("let x = evaluate_announce(a, b);", "evaluate_announce"),
        "a real call was missed"
    );
}
