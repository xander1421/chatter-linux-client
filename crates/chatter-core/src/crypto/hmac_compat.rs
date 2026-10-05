//! HMAC-SHA256 streaming shim with an OpenSSL-shaped surface.
//!
//! Wraps `hmac::Hmac<sha2::Sha256>` from RustCrypto so attachment crypto can
//! keep its existing call shape (`update(&mut self, &[u8]) -> Result<(), _>`,
//! consumed `finalize(self) -> Result<Vec<u8>, _>`) without taking a hard
//! dependency on OpenSSL's `PKey::hmac` + `Signer` pair.
//!
//! OpenSSL's HMAC API ties `Signer<'a>` to a borrowed `PKey<T>`, which forces
//! a self-referential struct or per-call rebuild. RustCrypto's `Hmac<Sha256>`
//! is owned and 'static, so the shim is straightforward. SHA-NI acceleration
//! kicks in automatically on x86_64 (Zen3+, Ice Lake+) and ARMv8 Crypto.

use std::convert::Infallible;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// SHA-256 streaming context with an OpenSSL `Hasher`-shaped surface
/// (`new`/`update`/`finish` returning `Result`) so attachment crypto can drop
/// `openssl::hash::Hasher` without rewriting every call site. Backed by
/// RustCrypto `sha2` (ARMv8 SHA2 accelerated). Errors are impossible; the
/// `Result` shape is preserved only for call-site compatibility.
pub struct Sha256Stream(Sha256);

impl Sha256Stream {
    pub fn new() -> Result<Self, Infallible> {
        Ok(Self(Sha256::new()))
    }
    pub fn update(&mut self, data: &[u8]) -> Result<(), Infallible> {
        self.0.update(data);
        Ok(())
    }
    /// Consumes the stream, returns the 32-byte SHA-256 digest.
    pub fn finish(self) -> Result<Vec<u8>, Infallible> {
        Ok(self.0.finalize().to_vec())
    }
}

/// HMAC-SHA256 streaming context.
///
/// Mirrors `boring::hmac::Hmac`'s `update`/`finalize` shape so callers
/// don't have to choose between OpenSSL's borrow-checker dance and a
/// full RustCrypto port. The returned `Vec<u8>` is always 32 bytes.
pub struct HmacStream(HmacSha256);

impl HmacStream {
    pub fn update(&mut self, data: &[u8]) -> Result<(), Infallible> {
        self.0.update(data);
        Ok(())
    }

    /// Consumes the stream, returns the 32-byte HMAC-SHA256 tag.
    pub fn finalize(self) -> Result<Vec<u8>, Infallible> {
        Ok(self.0.finalize().into_bytes().to_vec())
    }
}

/// Initialize an HMAC-SHA256 stream over `key`.
///
/// Errors are impossible in practice (the constructor only fails on an
/// allocation that this RustCrypto path doesn't take), but the signature
/// preserves the `Result` shape so call sites can use `?` / `.expect()`
/// the same way as the old OpenSSL path.
pub fn hmac_init(key: &[u8]) -> Result<HmacStream, Infallible> {
    let mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    Ok(HmacStream(mac))
}
