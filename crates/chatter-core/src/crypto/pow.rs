//! Proof of work for the server's anti-abuse challenges (server
//! `internal/antiabuse`, OPEN-ITEMS §134). One solver for every purpose.
//!
//! The server hands out a `PowChallenge`: a 75-byte token it authenticated
//! with HMAC-SHA256 (bound to the purpose, the request's identifying fields
//! and the transport) and a difficulty `k`. The client echoes the token with
//! a solution `s` such that
//!
//! ```text
//! Argon2id(password = be64(s), salt = domain || token,
//!          m = POW_MEMORY_KIB, t = 1, p = 1, 32-byte output)
//! ```
//!
//! starts with at least `k` zero bits, where `domain` is
//! `"chatter-pow-v3/argon2id/register\0"` or `"chatter-pow-v3/argon2id/login\0"`.
//! Expected cost 2^k Argon2id evaluations; the server verifies with ONE. Each
//! attempt has to fill and read POW_MEMORY_KIB of memory, so the search is
//! bounded by memory bandwidth and capacity rather than by hash throughput,
//! which is where a GPU or an ASIC beats a phone by orders of magnitude on
//! SHA-256 (threat model §3.15). The token is opaque here: the server checks
//! what it binds, the client only hashes it. Must match the server
//! (`internal/antiabuse/antiabuse.go` `Hash`).
//!
//! Only symmetric, hash-based primitives (Argon2id is BLAKE2b inside):
//! nothing here weakens under a quantum computer beyond Grover's square root.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use argon2::{Algorithm, Argon2, Block, Params, Version};

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
            Purpose::Register => b"chatter-pow-v3/argon2id/register\0",
            Purpose::Login => b"chatter-pow-v3/argon2id/login\0",
        }
    }
}

/// Length of a challenge token (server `antiabuse.TokenSize`).
pub const TOKEN_LEN: usize = 75;

/// Argon2id memory per attempt, KiB (server `antiabuse.MemoryKiB`).
pub const POW_MEMORY_KIB: u32 = 16 * 1024;

/// The most a server may ask for (server `antiabuse.MaxBits`). Every bit
/// doubles the expected number of Argon2id evaluations; the challenge's
/// expiry bounds the search anyway, so a hostile server cannot make a client
/// spin forever.
pub const MAX_POW_BITS: u32 = 16;

/// Solver threads at most. Each holds POW_MEMORY_KIB, so this bounds the
/// solver's memory (6 x 16 MiB = 96 MiB) on a 4 GB phone.
const MAX_SOLVER_THREADS: usize = 6;

fn argon2() -> Argon2<'static> {
    // Constant parameters inside Argon2's limits: cannot fail.
    let params = Params::new(POW_MEMORY_KIB, 1, 1, Some(32)).expect("valid Argon2id parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn salt(purpose: Purpose, token: &[u8]) -> Vec<u8> {
    [purpose.domain(), token].concat()
}

fn hash_with(a: &Argon2<'_>, salt: &[u8], solution: u64, memory: &mut [Block]) -> [u8; 32] {
    let mut out = [0u8; 32];
    a.hash_password_into_with_memory(&solution.to_be_bytes(), salt, &mut out, memory)
        .expect("Argon2id over fixed-size inputs");
    out
}

/// The hash a solution is judged by.
pub fn pow_hash(purpose: Purpose, token: &[u8], solution: u64) -> [u8; 32] {
    let a = argon2();
    let mut memory = vec![Block::default(); a.params().block_count()];
    hash_with(&a, &salt(purpose, token), solution, &mut memory)
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

/// Try solutions handed out by `next` until one meets `bits`, `stop`, or
/// `deadline`. One Argon2id evaluation per attempt, in this thread's own
/// reused memory.
fn search(
    purpose: Purpose,
    token: &[u8],
    bits: u32,
    next: &AtomicU64,
    stop: &AtomicBool,
    deadline: Instant,
) -> Option<u64> {
    let a = argon2();
    let salt = salt(purpose, token);
    let mut memory = vec![Block::default(); a.params().block_count()];
    loop {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        let s = next.fetch_add(1, Ordering::Relaxed);
        if leading_zero_bits(&hash_with(&a, &salt, s, &mut memory)) >= bits {
            return Some(s);
        }
    }
}

/// Find a solution on up to [`MAX_SOLVER_THREADS`] cores. Blocking: call from
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
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_SOLVER_THREADS);
    // A shared counter rather than fixed strides: big and little cores each
    // take the next untried value, so a slow core never holds up the search.
    let next = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let found = AtomicU64::new(0);
    let solved = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (next, stop, found, solved) = (&next, &stop, &found, &solved);
            scope.spawn(move || {
                if let Some(s) = search(purpose, token, bits, next, stop, deadline) {
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
        (0u64..).find(|&s| verify(purpose, token, s, bits)).unwrap()
    }

    /// Shared with the server: internal/antiabuse/antiabuse_test.go
    /// (TestPoW_SharedVectors). Token 00..4a, production parameters.
    #[test]
    fn go_vectors() {
        let token = vector_token();
        assert_eq!(
            hex::encode(pow_hash(Purpose::Register, &token, 0)),
            "39b8971f2934348a6bd5b9eed1b08725ff74b11d4a05ba6c3f9eaf9570943018"
        );
        assert_eq!(
            hex::encode(pow_hash(Purpose::Login, &token, 0)),
            "05b30e27f5221912155173c2dc77f6261cbd93c7a234f28870282de2a1e35772"
        );
        assert_eq!(
            hex::encode(pow_hash(Purpose::Register, &token, 24)),
            "0e72473d0d03ba23731ccc1bf965943d7d74506fe898da7f4ded1d7e1caf2663"
        );
        // Smallest 4-bit solutions.
        assert_eq!(smallest(Purpose::Register, &token, 4), 24);
        assert_eq!(smallest(Purpose::Login, &token, 4), 0);
        // The domain separates purposes.
        assert!(!verify(Purpose::Register, &token, 0, 4));
    }

    #[test]
    fn solver_finds_a_valid_solution() {
        let token = [7u8; TOKEN_LEN];
        let far = Instant::now() + Duration::from_secs(120);
        for purpose in [Purpose::Register, Purpose::Login] {
            for bits in [0, 1, 4] {
                let s = solve(purpose, &token, bits, far).unwrap();
                assert!(verify(purpose, &token, s, bits), "{purpose:?} {bits} bits: {s} does not verify");
            }
        }
    }

    #[test]
    fn refuses_bad_input_and_respects_deadline() {
        let far = Instant::now() + Duration::from_secs(60);
        assert!(solve(Purpose::Login, &[0u8; 32], 4, far).is_err(), "not a token");
        assert!(solve(Purpose::Login, &[0u8; TOKEN_LEN], MAX_POW_BITS + 1, far).is_err(), "over the cap");
        // 16 bits cannot be found in 0 s.
        assert!(solve(Purpose::Login, &[0u8; TOKEN_LEN], MAX_POW_BITS, Instant::now()).is_err(), "past deadline");
    }

    #[test]
    fn leading_zero_bits_counts_bits() {
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0x40]), 9);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }

    /// Argon2id rate and solve times at the server's efforts. Run with
    /// `cargo test -p chatter-core --release --lib pow::tests::bench -- --ignored --nocapture`
    /// (or cross-compile the test binary for a phone and run it there).
    #[test]
    #[ignore]
    fn bench() {
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_SOLVER_THREADS);
        let a = argon2();
        let salt = salt(Purpose::Register, &[1u8; TOKEN_LEN]);
        let mut memory = vec![Block::default(); a.params().block_count()];
        let n = 64u64;
        let t0 = Instant::now();
        for s in 0..n {
            std::hint::black_box(hash_with(&a, &salt, s, &mut memory));
        }
        let per = t0.elapsed() / n as u32;
        println!("one thread: {per:?} per Argon2id ({} MiB)", POW_MEMORY_KIB / 1024);
        let far = Instant::now() + Duration::from_secs(600);
        for (bits, runs) in [(4u32, 40), (6, 40), (8, 20)] {
            let mut times: Vec<Duration> = (0..runs)
                .map(|r| {
                    let tok = [r as u8 ^ (bits as u8) << 4; TOKEN_LEN];
                    let t = Instant::now();
                    let s = solve(Purpose::Register, &tok, bits, far).unwrap();
                    let d = t.elapsed();
                    assert!(verify(Purpose::Register, &tok, s, bits));
                    d
                })
                .collect();
            times.sort();
            let mean = times.iter().sum::<Duration>() / runs;
            let p90 = times[(runs as usize * 9) / 10 - 1];
            println!("{bits} bits, {threads} threads, {runs} runs: mean {mean:?} p90 {p90:?} max {:?}", times[runs as usize - 1]);
        }
    }
}
