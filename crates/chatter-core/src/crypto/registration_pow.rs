//! Registration proof of work (server OPEN-ITEMS §130).
//!
//! A registration challenge carries `pow_difficulty`; `Register` must carry a
//! `pow_solution` such that
//!
//! ```text
//! SHA-256("chatter-pow-v1\0" || challenge_nonce || big-endian u64 solution)
//! ```
//!
//! starts with at least `pow_difficulty` zero bits. The server requires it over
//! the onion service, where per-client rate limits cannot bound sign-up, and
//! checks it against the single-use challenge, so a solution is bound to one
//! challenge. Must match the server
//! (`internal/services/auth/service/registration_pow.go`).
//!
//! Domain + nonce + solution is 55 bytes: one SHA-256 block per attempt.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::signal::identity::REGISTRATION_CHALLENGE_NONCE_LEN;
use crate::error::CryptoError;

const POW_DOMAIN: &[u8] = b"chatter-pow-v1\0";

/// The most a server may ask for (the proto caps `pow_difficulty` at it).
/// 32 bits is ~4.3e9 hashes: well past what the challenge's 5-minute expiry
/// allows a phone, so a hostile server cannot make a client spin forever.
pub const MAX_POW_BITS: u32 = 32;

/// The hash a solution is judged by.
pub fn pow_hash(nonce: &[u8], solution: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(POW_DOMAIN);
    h.update(nonce);
    h.update(solution.to_be_bytes());
    h.finalize().into()
}

fn leading_zero_bits(h: &[u8]) -> u32 {
    let mut n = 0;
    for &b in h {
        if b != 0 {
            return n + b.leading_zeros();
        }
        n += 8;
    }
    n
}

/// Whether `solution` meets `bits` for `nonce`. Difficulty 0 accepts anything.
pub fn verify(nonce: &[u8], solution: u64, bits: u32) -> bool {
    bits == 0 || leading_zero_bits(&pow_hash(nonce, solution)) >= bits
}

/// Search `start, start + step, ...` until a solution, `stop`, or `deadline`.
fn search(
    nonce: &[u8],
    bits: u32,
    start: u64,
    step: u64,
    stop: &AtomicBool,
    deadline: Instant,
) -> Option<u64> {
    let mut prefix = Sha256::new();
    prefix.update(POW_DOMAIN);
    prefix.update(nonce);
    let mut s = start;
    let mut n: u32 = 0;
    loop {
        let mut h = prefix.clone();
        h.update(s.to_be_bytes());
        if leading_zero_bits(&h.finalize()) >= bits {
            return Some(s);
        }
        s = s.wrapping_add(step);
        n = n.wrapping_add(1);
        if n % 0x1_0000 == 0 && (stop.load(Ordering::Relaxed) || Instant::now() >= deadline) {
            return None;
        }
    }
}

/// Find a solution on every available core. Blocking: call from
/// [`solve_async`] (or a test), never on an async runtime thread.
///
/// Fails if the nonce is not a challenge nonce, the difficulty is above
/// [`MAX_POW_BITS`], or no solution is found before `deadline` (the
/// challenge's expiry: a later solution would be refused anyway).
pub fn solve(nonce: &[u8], bits: u32, deadline: Instant) -> Result<u64, CryptoError> {
    if nonce.len() != REGISTRATION_CHALLENGE_NONCE_LEN {
        return Err(CryptoError::Protocol(format!(
            "registration challenge nonce must be {REGISTRATION_CHALLENGE_NONCE_LEN} bytes, got {}",
            nonce.len()
        )));
    }
    if bits > MAX_POW_BITS {
        return Err(CryptoError::Protocol(format!(
            "registration proof of work of {bits} bits exceeds the {MAX_POW_BITS}-bit maximum"
        )));
    }
    if bits == 0 {
        return Ok(0);
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    let stop = AtomicBool::new(false);
    let found = AtomicU64::new(0);
    let solved = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (stop, found, solved) = (&stop, &found, &solved);
            scope.spawn(move || {
                if let Some(s) = search(nonce, bits, t, threads, stop, deadline) {
                    if !solved.swap(true, Ordering::AcqRel) {
                        found.store(s, Ordering::Release);
                    }
                    stop.store(true, Ordering::Relaxed);
                }
            });
        }
    });
    if solved.load(Ordering::Acquire) {
        Ok(found.load(Ordering::Acquire))
    } else {
        Err(CryptoError::Protocol(
            "registration proof of work not found before the challenge expired".into(),
        ))
    }
}

/// [`solve`] on the blocking pool, so the async runtime (and the UI it
/// drives) keeps running while the cores work.
pub async fn solve_async(nonce: Vec<u8>, bits: u32, deadline: Instant) -> Result<u64, CryptoError> {
    tokio::task::spawn_blocking(move || solve(&nonce, bits, deadline))
        .await
        .map_err(|e| CryptoError::Protocol(format!("proof-of-work task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn vector_nonce() -> Vec<u8> {
        (0u8..32).collect()
    }

    /// Shared with the server: internal/services/auth/service/registration_pow_test.go
    /// (powVector*). The smallest 16-bit solution for nonce 00..1f.
    #[test]
    fn go_vector() {
        let nonce = vector_nonce();
        let stop = AtomicBool::new(false);
        let far = Instant::now() + Duration::from_secs(60);
        let s = search(&nonce, 16, 0, 1, &stop, far).unwrap();
        assert_eq!(s, 28975);
        assert_eq!(
            hex::encode(pow_hash(&nonce, s)),
            "000088be2895c2cac9358be9de619cfcd3f3ed1f2fc3646f346049aac7db100f"
        );
        assert!(verify(&nonce, s, 16));
        assert!(!verify(&nonce, s, 17));
        assert!(!verify(&nonce, 28974, 16));
    }

    #[test]
    fn solver_finds_a_valid_solution() {
        let nonce = [7u8; 32];
        let far = Instant::now() + Duration::from_secs(60);
        for bits in [0, 1, 8, 18] {
            let s = solve(&nonce, bits, far).unwrap();
            assert!(verify(&nonce, s, bits), "{bits} bits: {s} does not verify");
        }
    }

    #[test]
    fn refuses_bad_input_and_respects_deadline() {
        let far = Instant::now() + Duration::from_secs(60);
        assert!(solve(&[0u8; 31], 8, far).is_err(), "short nonce");
        assert!(solve(&[0u8; 32], MAX_POW_BITS + 1, far).is_err(), "over the cap");
        // 32 bits cannot be found in 0 s.
        assert!(solve(&[0u8; 32], 32, Instant::now()).is_err(), "past deadline");
    }

    #[test]
    fn leading_zero_bits_counts_bits() {
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0x40]), 9);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }

    /// Solve times around the server's default onion difficulty (23 bits). Run with
    /// `cargo test -p chatter-core --release registration_pow::tests::bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench() {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let far = Instant::now() + Duration::from_secs(600);
        let t = Instant::now();
        let n = 1u64 << 22;
        for s in 0..n {
            std::hint::black_box(pow_hash(&[1u8; 32], s));
        }
        let rate = n as f64 / t.elapsed().as_secs_f64();
        println!("single core: {:.1} M hash/s", rate / 1e6);
        for bits in [20u32, 22, 23, 24] {
            let mut total = Duration::ZERO;
            let runs = 20;
            for r in 0..runs {
                let nonce = [r as u8; 32];
                let t = Instant::now();
                let s = solve(&nonce, bits, far).unwrap();
                total += t.elapsed();
                assert!(verify(&nonce, s, bits));
            }
            println!("{bits} bits, {threads} threads: mean {:?}", total / runs);
        }
    }
}
