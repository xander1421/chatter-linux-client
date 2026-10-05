//! No send or call path may request mailbox or call-room tokens (NDSS'21 §VI-F).
//!
//! An authenticated `GetMailboxTokens` followed a moment later by the
//! anonymous operation that spends its output links the two in time. Sends
//! spend only via `TokenManager::claim_prefetched_token` and calls only via
//! `claim_prefetched_call_room_token`; the pools are
//! refilled by `CoreServices::spawn_token_refill` (and the login prefetch),
//! which live in `app/core_services.rs`. Any other caller of
//! `request_tokens` or `request_call_room_tokens` reintroduces
//! request-then-spend.

use std::path::Path;

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn request_tokens_is_only_called_by_the_refill() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let allowed = [
        src.join("app/core_services.rs"),
        src.join("mailbox/token_manager.rs"),
    ];
    let offenders: Vec<_> = files
        .iter()
        .filter(|f| !allowed.contains(f))
        .filter(|f| {
            let src = std::fs::read_to_string(f).unwrap();
            src.contains(".request_tokens(") || src.contains(".request_call_room_tokens(")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "request-then-spend reintroduced in: {offenders:?}"
    );
}
