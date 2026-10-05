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
//! The gate (server review H1): the solution's HIGH 32 bits must also make
//!
//! ```text
//! SHA-256(gate_domain || token || be32(s >> 32))
//! ```
//!
//! start with `gate` zero bits — the challenge's `gate_bits`, 18 when the
//! server is calm and up to 23 while a flood oversubscribes its verification
//! budget (the server checks the copy MACed into the token; clients refuse
//! more than [`MAX_GATE_BITS`]) — where `gate_domain` is
//! `"chatter-pow-v3/gate/register\0"` or `"chatter-pow-v3/gate/login\0"`. The
//! server checks it before spending anything on a submission, so junk costs
//! its sender ~2^gate SHA-256. [`solve`] finds one gate-passing high half per
//! challenge, then searches the low 32 bits with Argon2id: the gate is paid
//! once, the Argon2id count is unchanged.
//!
//! Only symmetric, hash-based primitives (Argon2id is BLAKE2b inside):
//! nothing here weakens under a quantum computer beyond Grover's square root.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use argon2::{Algorithm, Argon2, Block, Params, Version};
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
    fn gate_domain(self) -> &'static [u8] {
        match self {
            Purpose::Register => b"chatter-pow-v3/gate/register\0",
            Purpose::Login => b"chatter-pow-v3/gate/login\0",
        }
    }

    fn domain(self) -> &'static [u8] {
        match self {
            Purpose::Register => b"chatter-pow-v3/argon2id/register\0",
            Purpose::Login => b"chatter-pow-v3/argon2id/login\0",
        }
    }
}

/// Length of a challenge token (server `antiabuse.TokenSize`).
pub const TOKEN_LEN: usize = 76;

/// The most gate bits a client pays (server `antiabuse.MaxGateBits`). The
/// server's cap is 23 (2^23 SHA-256, ESTIMATED ~0.25 s mean on a Galaxy S9's
/// 6 solver threads with the ARMv8 SHA2 instructions); 26 leaves headroom
/// without letting a hostile server ask for minutes.
pub const MAX_GATE_BITS: u32 = 26;

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

/// The gate hash of a high half.
pub fn gate_hash(purpose: Purpose, token: &[u8], high: u32) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(purpose.gate_domain());
    h.update(token);
    h.update(high.to_be_bytes());
    h.finalize().into()
}

fn gate_ok(purpose: Purpose, token: &[u8], solution: u64, gate: u32) -> bool {
    leading_zero_bits(&gate_hash(purpose, token, (solution >> 32) as u32)) >= gate
}

/// Whether `solution` meets `bits` (Argon2id) and `gate` (SHA-256 on the
/// high half) for `token`. Difficulty 0 accepts anything.
pub fn verify(purpose: Purpose, token: &[u8], solution: u64, bits: u32, gate: u32) -> bool {
    bits == 0
        || (gate_ok(purpose, token, solution, gate)
            && leading_zero_bits(&pow_hash(purpose, token, solution)) >= bits)
}

/// Run `test` over 0, 1, 2, ... on `threads` threads, each with its own
/// `init()` state, until one value passes, or `deadline`. A shared counter
/// rather than fixed strides: big and little cores each take the next
/// untried value, so a slow core never holds up the search.
///
/// Each thread claims `chunk` consecutive values per counter update: for the
/// cheap gate, one shared atomic per SHA-256 would cost as much as the hash.
fn race<S, I, T>(threads: usize, chunk: u64, deadline: Instant, init: I, test: T) -> Option<u64>
where
    I: Fn() -> S + Sync,
    T: Fn(&mut S, u64) -> bool + Sync,
{
    let next = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let found = AtomicU64::new(0);
    let solved = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (next, stop, found, solved, init, test) =
                (&next, &stop, &found, &solved, &init, &test);
            scope.spawn(move || {
                let mut state = init();
                loop {
                    if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        return;
                    }
                    let first = next.fetch_add(chunk, Ordering::Relaxed);
                    for v in first..first + chunk {
                        if v > u64::from(u32::MAX) {
                            return;
                        }
                        if test(&mut state, v) {
                            if !solved.swap(true, Ordering::AcqRel) {
                                found.store(v, Ordering::Release);
                            }
                            stop.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
    });
    solved
        .load(Ordering::Acquire)
        .then(|| found.load(Ordering::Acquire))
}

/// Find a solution on up to [`MAX_SOLVER_THREADS`] cores. Blocking: call from
/// [`solve_async`] (or a test), never on an async runtime thread.
///
/// Fails if the token is not a challenge token, the difficulty is above
/// [`MAX_POW_BITS`], or no solution is found before `deadline` (the
/// challenge's expiry: a later solution would be refused anyway).
pub fn solve(
    purpose: Purpose,
    token: &[u8],
    bits: u32,
    gate: u32,
    deadline: Instant,
) -> Result<u64, CryptoError> {
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
    if gate > MAX_GATE_BITS {
        return Err(CryptoError::Protocol(format!(
            "proof-of-work gate of {gate} bits exceeds the {MAX_GATE_BITS}-bit maximum"
        )));
    }
    if bits == 0 {
        return Ok(0);
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_SOLVER_THREADS);
    let expired =
        || CryptoError::Protocol("proof of work not found before the challenge expired".into());
    // The gate, once: ~2^gate SHA-256 over the high half. The constant
    // prefix (domain || token) is hashed once and cloned per attempt.
    let mut prefix = Sha256::new();
    prefix.update(purpose.gate_domain());
    prefix.update(token);
    let high = race(
        threads,
        4096,
        deadline,
        || (),
        |_, h| {
            let mut d = prefix.clone();
            d.update((h as u32).to_be_bytes());
            leading_zero_bits(&d.finalize()) >= gate
        },
    )
    .ok_or_else(expired)?;
    // Then Argon2id over the low half, one attempt per value, each thread in
    // its own reused memory.
    let salt = salt(purpose, token);
    let low = race(
        threads,
        1,
        deadline,
        || {
            let a = argon2();
            let memory = vec![Block::default(); a.params().block_count()];
            (a, memory)
        },
        |(a, memory), low| {
            leading_zero_bits(&hash_with(a, &salt, high << 32 | low, memory)) >= bits
        },
    )
    .ok_or_else(expired)?;
    Ok(high << 32 | low)
}

/// [`solve`] on the blocking pool, so the async runtime (and the UI it
/// drives) keeps running while the cores work.
pub async fn solve_async(
    purpose: Purpose,
    token: Vec<u8>,
    bits: u32,
    gate: u32,
    deadline: Instant,
) -> Result<u64, CryptoError> {
    tokio::task::spawn_blocking(move || solve(purpose, &token, bits, gate, deadline))
        .await
        .map_err(|e| CryptoError::Protocol(format!("proof-of-work task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 00 01 .. 4b with byte 3 (the token's gate byte) = 18.
    fn vector_token() -> Vec<u8> {
        let mut t: Vec<u8> = (0u8..TOKEN_LEN as u8).collect();
        t[3] = 18;
        t
    }

    /// The reference search (server `Guard.Solve`): smallest gate-passing
    /// high half, then smallest low half.
    fn smallest(purpose: Purpose, token: &[u8], bits: u32, gate: u32) -> u64 {
        let high = (0u64..)
            .find(|&h| gate_ok(purpose, token, h << 32, gate))
            .unwrap();
        (0u64..)
            .map(|l| high << 32 | l)
            .find(|&s| verify(purpose, token, s, bits, gate))
            .unwrap()
    }

    /// Shared with the server: internal/antiabuse/antiabuse_test.go
    /// (TestPoW_SharedVectors). Token 00..4a, production parameters.
    #[test]
    fn go_vectors() {
        let token = vector_token();
        assert_eq!(
            hex::encode(pow_hash(Purpose::Register, &token, 0)),
            "85673cd8dfaeb5f771e4a875273f8b60a4bf332d6a5a795ce37994e45b0e2a32"
        );
        assert_eq!(
            hex::encode(pow_hash(Purpose::Login, &token, 0)),
            "64654f89a80bee3b2e09c2ce78434d1144b36e3155b043bcd65c12a13f084c48"
        );
        for (purpose, s, gate, work) in [
            (
                Purpose::Register,
                375_667_904_479_234u64, // high 87467, low 2
                "00000f025f8476a283ac75d06a594a96ee0b63598ef06cab3edd6df4a879227f",
                "00e10279033c192facb1550879c9b44e606c4809fde7c7056b3b26809c7a8477",
            ),
            (
                Purpose::Login,
                2_593_228_238_880_785u64, // high 603783, low 17
                "000031bc7a3752f78dc989d98584f57a28cd447bd7f3a852cde0d83ebe8de637",
                "0851e7fc0caa00982202dfd63ded3c367b59bc7ced4351fbe286cd245fae1936",
            ),
        ] {
            assert_eq!(smallest(purpose, &token, 4, 18), s, "{purpose:?}");
            assert_eq!(
                hex::encode(gate_hash(purpose, &token, (s >> 32) as u32)),
                gate
            );
            assert_eq!(hex::encode(pow_hash(purpose, &token, s)), work);
        }
        // A low half alone (high 0) does not pass the gate.
        assert!(!verify(Purpose::Register, &token, 2, 4, 18));
        // A raised gate rejects a solution that only meets 18.
        assert!(!verify(
            Purpose::Register,
            &token,
            375_667_904_479_234,
            4,
            24
        ));
    }

    #[test]
    fn solver_finds_a_valid_solution() {
        let token = [7u8; TOKEN_LEN];
        let far = Instant::now() + Duration::from_secs(120);
        for purpose in [Purpose::Register, Purpose::Login] {
            for (bits, gate) in [(0, 18), (1, 18), (4, 18), (4, 20)] {
                let s = solve(purpose, &token, bits, gate, far).unwrap();
                assert!(
                    verify(purpose, &token, s, bits, gate),
                    "{purpose:?} {bits}/{gate} bits: {s} does not verify"
                );
            }
        }
    }

    #[test]
    fn refuses_bad_input_and_respects_deadline() {
        let far = Instant::now() + Duration::from_secs(60);
        assert!(
            solve(Purpose::Login, &[0u8; 32], 4, 18, far).is_err(),
            "not a token"
        );
        assert!(
            solve(Purpose::Login, &[0u8; TOKEN_LEN], MAX_POW_BITS + 1, 18, far).is_err(),
            "over the cap"
        );
        assert!(
            solve(Purpose::Login, &[0u8; TOKEN_LEN], 4, MAX_GATE_BITS + 1, far).is_err(),
            "gate over the cap"
        );
        // 16 bits cannot be found in 0 s.
        assert!(
            solve(
                Purpose::Login,
                &[0u8; TOKEN_LEN],
                MAX_POW_BITS,
                18,
                Instant::now()
            )
            .is_err(),
            "past deadline"
        );
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
        println!(
            "one thread: {per:?} per Argon2id ({} MiB)",
            POW_MEMORY_KIB / 1024
        );
        let g0 = Instant::now();
        let n_sha = 1u32 << 20;
        for h in 0..n_sha {
            std::hint::black_box(gate_hash(Purpose::Register, &[1u8; TOKEN_LEN], h));
        }
        let rate = f64::from(n_sha) / g0.elapsed().as_secs_f64();
        println!(
            "one thread: {:.1} M SHA-256/s; gate 2^18 expected {:?}, 2^24 {:?} on one thread",
            rate / 1e6,
            Duration::from_secs_f64(f64::from(1u32 << 18) / rate),
            Duration::from_secs_f64(f64::from(1u32 << 24) / rate)
        );
        // The gate alone (difficulty 1 = ~2 Argon2id) at the floor and cap.
        for gate in [18u32, 21, 24] {
            let runs = 20u32;
            let mut times: Vec<Duration> = (0..runs)
                .map(|r| {
                    let tok = [r as u8 ^ 0x5a; TOKEN_LEN];
                    let t = Instant::now();
                    solve(
                        Purpose::Login,
                        &tok,
                        1,
                        gate,
                        Instant::now() + Duration::from_secs(600),
                    )
                    .unwrap();
                    t.elapsed()
                })
                .collect();
            times.sort();
            let mean = times.iter().sum::<Duration>() / runs;
            println!(
                "gate {gate} (+1 Argon2 bit), {threads} threads, {runs} runs: mean {mean:?} p90 {:?}",
                times[17]
            );
        }
        let far = Instant::now() + Duration::from_secs(600);
        for (bits, runs) in [(4u32, 40), (6, 40), (8, 20)] {
            let mut times: Vec<Duration> = (0..runs)
                .map(|r| {
                    let tok = [r as u8 ^ (bits as u8) << 4; TOKEN_LEN];
                    let t = Instant::now();
                    let s = solve(Purpose::Register, &tok, bits, 18, far).unwrap();
                    let d = t.elapsed();
                    assert!(verify(Purpose::Register, &tok, s, bits, 18));
                    d
                })
                .collect();
            times.sort();
            let mean = times.iter().sum::<Duration>() / runs;
            let p90 = times[(runs as usize * 9) / 10 - 1];
            println!(
                "{bits} bits, {threads} threads, {runs} runs: mean {mean:?} p90 {p90:?} max {:?}",
                times[runs as usize - 1]
            );
        }
    }
}
