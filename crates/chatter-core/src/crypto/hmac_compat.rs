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

use hmac::{Hmac, Mac};
use openssl::error::ErrorStack;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 streaming context.
///
/// Mirrors `boring::hmac::Hmac`'s `update`/`finalize` shape so callers
/// don't have to choose between OpenSSL's borrow-checker dance and a
/// full RustCrypto port. The returned `Vec<u8>` is always 32 bytes.
pub struct HmacStream(HmacSha256);

impl HmacStream {
    pub fn update(&mut self, data: &[u8]) -> Result<(), ErrorStack> {
        self.0.update(data);
        Ok(())
    }

    /// Consumes the stream, returns the 32-byte HMAC-SHA256 tag.
    pub fn finalize(self) -> Result<Vec<u8>, ErrorStack> {
        Ok(self.0.finalize().into_bytes().to_vec())
    }
}

/// Initialize an HMAC-SHA256 stream over `key`.
///
/// Errors are impossible in practice (the constructor only fails on an
/// allocation that this RustCrypto path doesn't take), but the signature
/// preserves the `Result` shape so call sites can use `?` / `.expect()`
/// the same way as the old OpenSSL path.
pub fn hmac_init(key: &[u8]) -> Result<HmacStream, ErrorStack> {
    let mac = <HmacSha256 as Mac>::new_from_slice(key)
        .expect("HMAC-SHA256 accepts any key length");
    Ok(HmacStream(mac))
}
