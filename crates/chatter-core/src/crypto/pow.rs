//! Proof of work for the server's anti-abuse challenges (server
//! `internal/antiabuse`, OPEN-ITEMS §134). One solver for every purpose.
//!
//! The server hands out a `PowChallenge`: a 75-byte token it authenticated
//! with HMAC-SHA256 (bound to the purpose, the request's identifying fields
//! and the transport) and a difficulty. The client echoes the token with a
//! solution `s` such that
//!
//! ```text
//! SHA-256(domain || token || big-endian u64 s)
//! ```
//!
//! starts with at least `difficulty` zero bits, where `domain` is
//! `"chatter-pow-v2/register\0"` or `"chatter-pow-v2/login\0"`. The token is
//! opaque here: the server checks what it binds, the client only hashes it.
//! `domain || token` is over one SHA-256 block, so [`search`] hashes that
//! block once and pays one compression per attempt. Must match the server
//! (`internal/antiabuse/antiabuse.go` `Hash`).
//!
//! Only SHA-256: nothing here weakens under a quantum computer beyond Grover's
//! square root on the server's 256-bit HMAC key.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::error::CryptoError;

/// What a challenge pays for. The server binds it into the token and the
/// work's domain, so a solution for one is never one for the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Register,
    Login,
}

impl Purpose {
    fn domain(self) -> &'static [u8] {
        match self {
            Purpose::Register => b"chatter-pow-v2/register\0",
            Purpose::Login => b"chatter-pow-v2/login\0",
        }
    }
}

/// Length of a challenge token (server `antiabuse.TokenSize`).
pub const TOKEN_LEN: usize = 75;

/// The most a server may ask for (the proto caps `difficulty` at it). 32 bits
/// is ~4.3e9 hashes: well past what a challenge's expiry allows a phone, so a
/// hostile server cannot make a client spin forever.
pub const MAX_POW_BITS: u32 = 32;

/// The hash a solution is judged by.
pub fn pow_hash(purpose: Purpose, token: &[u8], solution: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(purpose.domain());
    h.update(token);
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

/// Whether `solution` meets `bits` for `token`. Difficulty 0 accepts anything.
pub fn verify(purpose: Purpose, token: &[u8], solution: u64, bits: u32) -> bool {
    bits == 0 || leading_zero_bits(&pow_hash(purpose, token, solution)) >= bits
}

/// Search `start, start + step, ...` until a solution, `stop`, or `deadline`.
fn search(
    purpose: Purpose,
    token: &[u8],
    bits: u32,
    start: u64,
    step: u64,
    stop: &AtomicBool,
    deadline: Instant,
) -> Option<u64> {
    // The constant prefix is hashed once; each clone carries its state.
    let mut prefix = Sha256::new();
    prefix.update(purpose.domain());
    prefix.update(token);
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
/// Fails if the token is not a challenge token, the difficulty is above
/// [`MAX_POW_BITS`], or no solution is found before `deadline` (the
/// challenge's expiry: a later solution would be refused anyway).
pub fn solve(purpose: Purpose, token: &[u8], bits: u32, deadline: Instant) -> Result<u64, CryptoError> {
    if token.len() != TOKEN_LEN {
        return Err(CryptoError::Protocol(format!(
            "proof-of-work token must be {TOKEN_LEN} bytes, got {}",
            token.len()
        )));
    }
    if bits > MAX_POW_BITS {
        return Err(CryptoError::Protocol(format!(
            "proof of work of {bits} bits exceeds the {MAX_POW_BITS}-bit maximum"
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
                if let Some(s) = search(purpose, token, bits, t, threads, stop, deadline) {
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
            "proof of work not found before the challenge expired".into(),
        ))
    }
}

/// [`solve`] on the blocking pool, so the async runtime (and the UI it
/// drives) keeps running while the cores work.
pub async fn solve_async(
    purpose: Purpose,
    token: Vec<u8>,
    bits: u32,
    deadline: Instant,
) -> Result<u64, CryptoError> {
    tokio::task::spawn_blocking(move || solve(purpose, &token, bits, deadline))
        .await
        .map_err(|e| CryptoError::Protocol(format!("proof-of-work task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn vector_token() -> Vec<u8> {
        (0u8..TOKEN_LEN as u8).collect()
    }

    fn smallest(purpose: Purpose, token: &[u8], bits: u32) -> u64 {
        let stop = AtomicBool::new(false);
        search(purpose, token, bits, 0, 1, &stop, Instant::now() + Duration::from_secs(60)).unwrap()
    }

    /// Shared with the server: internal/antiabuse/antiabuse_test.go
    /// (TestPoW_SharedVectors). Token 00..4a, 16 bits, smallest solution.
    #[test]
    fn go_vectors() {
        let token = vector_token();
        let reg = smallest(Purpose::Register, &token, 16);
        assert_eq!(reg, 9698);
        assert_eq!(
            hex::encode(pow_hash(Purpose::Register, &token, reg)),
            "00004c2db2fdc8a697deac2bae017bd172aaa63101462c654881b37256e069b3"
        );
        let login = smallest(Purpose::Login, &token, 16);
        assert_eq!(login, 59593);
        assert_eq!(
            hex::encode(pow_hash(Purpose::Login, &token, login)),
            "0000716b9f2c12eb592076c47ec7b89652e8813cef130fcf3ba5cbf6b4a4abb3"
        );
        // The domain separates purposes.
        assert!(!verify(Purpose::Login, &token, reg, 16));
        assert!(!verify(Purpose::Register, &token, login, 16));
        // Smallest: nothing below it verifies.
        assert!((0..reg).all(|s| !verify(Purpose::Register, &token, s, 16)));
    }

    #[test]
    fn solver_finds_a_valid_solution() {
        let token = [7u8; TOKEN_LEN];
        let far = Instant::now() + Duration::from_secs(60);
        for purpose in [Purpose::Register, Purpose::Login] {
            for bits in [0, 1, 8, 18] {
                let s = solve(purpose, &token, bits, far).unwrap();
                assert!(verify(purpose, &token, s, bits), "{purpose:?} {bits} bits: {s} does not verify");
            }
        }
    }

    #[test]
    fn refuses_bad_input_and_respects_deadline() {
        let far = Instant::now() + Duration::from_secs(60);
        assert!(solve(Purpose::Login, &[0u8; 32], 8, far).is_err(), "not a token");
        assert!(solve(Purpose::Login, &[0u8; TOKEN_LEN], MAX_POW_BITS + 1, far).is_err(), "over the cap");
        // 32 bits cannot be found in 0 s.
        assert!(solve(Purpose::Login, &[0u8; TOKEN_LEN], 32, Instant::now()).is_err(), "past deadline");
    }

    #[test]
    fn leading_zero_bits_counts_bits() {
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0x40]), 9);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }

    /// Solve times around the server's onion sign-up effort (23-26 bits). Run with
    /// `cargo test -p chatter-core --release pow::tests::bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench() {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let far = Instant::now() + Duration::from_secs(600);
        let token = [1u8; TOKEN_LEN];
        let n = 1u64 << 22;
        let rate = {
            let t0 = Instant::now();
            let mut prefix = Sha256::new();
            prefix.update(Purpose::Register.domain());
            prefix.update(token);
            for s in 0..n {
                let mut h = prefix.clone();
                h.update(s.to_be_bytes());
                std::hint::black_box(h.finalize());
            }
            n as f64 / t0.elapsed().as_secs_f64()
        };
        println!("single core: {:.1} M hash/s", rate / 1e6);
        for bits in [20u32, 22, 23, 24] {
            let mut total = Duration::ZERO;
            let runs = 20;
            for r in 0..runs {
                let tok = [r as u8; TOKEN_LEN];
                let t = Instant::now();
                let s = solve(Purpose::Register, &tok, bits, far).unwrap();
                total += t.elapsed();
                assert!(verify(Purpose::Register, &tok, s, bits));
            }
            println!("{bits} bits, {threads} threads: mean {:?}", total / runs);
        }
    }
}
