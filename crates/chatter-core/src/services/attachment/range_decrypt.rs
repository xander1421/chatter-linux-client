//! Random-access AES-256-CBC decryption for streaming attachment playback.
//!
//! Lets the media pipeline pull arbitrary plaintext byte-ranges of a large
//! attachment on demand instead of decrypting the whole file into memory
//! before playback starts.
//!
//! File layout (same as [`crate::crypto::streaming_decrypt`]):
//! `IV(16) ‖ AES-256-CBC ciphertext (block-aligned, raw/no padding) ‖ HMAC-SHA256(32)`.
//!
//! CBC is randomly readable: `plaintext[b] = AES_dec(C[b]) ⊕ C[b-1]`, with
//! `C[-1]` = IV. So any range can be decrypted by touching only its covering
//! ciphertext blocks plus the one preceding block for the chain.
//!
//! ## Security
//!
//! Range reads are **unauthenticated on their own**. The caller MUST verify the
//! whole-file HMAC (via [`crate::crypto::streaming_decrypt::StreamingDecryptor::verify`])
//! before constructing this — see `AttachmentService::open_range_decryptor`.

use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use memmap2::Mmap;

const IV_LEN: usize = 16;
const HMAC_LEN: usize = 32;
const AES_BLOCK: usize = 16;

/// Decrypts plaintext byte-ranges of a CBC attachment on demand.
pub struct CbcRangeDecryptor {
    mmap: Mmap,
    cipher: Aes256,
    plaintext_size: usize,
}

impl CbcRangeDecryptor {
    /// Build from an mmap of the on-disk ciphertext and the 32-byte AES key.
    ///
    /// `plaintext_size` is the true decrypted length (raw CBC leaves the last
    /// block zero-padded up to a 16-byte boundary; we never read past this).
    pub fn new(mmap: Mmap, aes_key: &[u8; 32], plaintext_size: usize) -> Result<Self, String> {
        let total = mmap.len();
        if total < IV_LEN + HMAC_LEN {
            return Err(format!("ciphertext too small: {total} bytes"));
        }
        let encrypted_len = total - IV_LEN - HMAC_LEN;
        if encrypted_len % AES_BLOCK != 0 {
            return Err(format!(
                "ciphertext not block-aligned: {encrypted_len} bytes"
            ));
        }
        if plaintext_size > encrypted_len {
            return Err(format!(
                "plaintext_size {plaintext_size} exceeds ciphertext {encrypted_len}"
            ));
        }
        let cipher = Aes256::new(aes_key.into());
        Ok(Self {
            mmap,
            cipher,
            plaintext_size,
        })
    }

    /// Total decrypted size — what the media source reports as stream length.
    #[inline]
    pub fn plaintext_size(&self) -> usize {
        self.plaintext_size
    }

    /// Ciphertext block `b` (0-based), 16 bytes.
    #[inline]
    fn cipher_block(&self, b: usize) -> &[u8] {
        let start = IV_LEN + b * AES_BLOCK;
        &self.mmap[start..start + AES_BLOCK]
    }

    /// CBC chaining input for block `b`: the previous ciphertext block, or the
    /// IV for block 0.
    #[inline]
    fn chain_block(&self, b: usize) -> &[u8] {
        if b == 0 {
            &self.mmap[0..IV_LEN]
        } else {
            self.cipher_block(b - 1)
        }
    }

    /// Decrypt plaintext bytes `[off, off + len)`, clamped to `plaintext_size`.
    ///
    /// Returns fewer than `len` bytes (or empty) near/at EOF.
    pub fn read_range(&self, off: usize, len: usize) -> Vec<u8> {
        if off >= self.plaintext_size || len == 0 {
            return Vec::new();
        }
        let end = off.saturating_add(len).min(self.plaintext_size);
        let start_block = off / AES_BLOCK;
        let end_block = (end - 1) / AES_BLOCK; // inclusive; end <= encrypted_len so in range

        let mut window = Vec::with_capacity((end_block - start_block + 1) * AES_BLOCK);
        for b in start_block..=end_block {
            let mut blk = [0u8; AES_BLOCK];
            blk.copy_from_slice(self.cipher_block(b));
            self.cipher.decrypt_block((&mut blk).into());
            let chain = self.chain_block(b);
            for i in 0..AES_BLOCK {
                blk[i] ^= chain[i];
            }
            window.extend_from_slice(&blk);
        }

        // Slice the requested sub-range out of the block-aligned window.
        let lo = off - start_block * AES_BLOCK;
        let hi = lo + (end - off);
        window[lo..hi].to_vec()
    }
}

/// `Read + Seek` adapter over a [`CbcRangeDecryptor`], for consumers that pull
/// bytes through `std::io` (e.g. the rodio audio `Decoder`). Decrypts on demand
/// like the video AppSrc, so audio plays without a full decrypt-to-memory.
///
/// Cheap to construct (`Arc` clone + a cursor); build a fresh one per decoder.
pub struct CbcRangeReader {
    dec: std::sync::Arc<CbcRangeDecryptor>,
    pos: u64,
}

impl CbcRangeReader {
    pub fn new(dec: std::sync::Arc<CbcRangeDecryptor>) -> Self {
        Self { dec, pos: 0 }
    }

    /// Plaintext length — pass as the decoder's byte-length hint for seeking.
    pub fn len(&self) -> u64 {
        self.dec.plaintext_size() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.dec.plaintext_size() == 0
    }
}

impl std::io::Read for CbcRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Seeking past EOF is allowed, so `pos` can exceed usize on 32-bit.
        let off = usize::try_from(self.pos).unwrap_or(usize::MAX);
        let chunk = self.dec.read_range(off, buf.len());
        let n = chunk.len();
        buf[..n].copy_from_slice(&chunk);
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for CbcRangeReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        // Offsets come from a decoder parsing a file the sender chose, so the
        // arithmetic is checked rather than trusted not to wrap.
        let new_pos = match pos {
            SeekFrom::Start(o) => Some(o),
            SeekFrom::End(o) => (self.dec.plaintext_size() as u64).checked_add_signed(o),
            SeekFrom::Current(o) => self.pos.checked_add_signed(o),
        };
        // Seeking past EOF is allowed; the next read returns 0 bytes.
        self.pos = new_pos.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek out of range")
        })?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::attachment_crypto::AttachmentCrypto;
    use std::io::{Read, Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;

    fn make_decryptor(plaintext: &[u8]) -> (std::sync::Arc<CbcRangeDecryptor>, NamedTempFile) {
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&encrypted.ciphertext).unwrap();
        file.flush().unwrap();
        let mmap = unsafe { Mmap::map(file.as_file()).unwrap() };
        let mut aes_key = [0u8; 32];
        aes_key.copy_from_slice(&encrypted.key[..32]);
        let dec = CbcRangeDecryptor::new(mmap, &aes_key, encrypted.plaintext_size).unwrap();
        (std::sync::Arc::new(dec), file)
    }

    /// Random-access reads must match the plaintext exactly for arbitrary
    /// (offset, len) windows — including block-unaligned and EOF-crossing ones.
    #[test]
    fn read_range_matches_plaintext() {
        // ~300KB so we span many 16-byte blocks and several 64KB pull windows.
        let plaintext: Vec<u8> = (0..300_000usize).map(|i| (i % 251) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&encrypted.ciphertext).unwrap();
        file.flush().unwrap();
        let mmap = unsafe { Mmap::map(file.as_file()).unwrap() };

        let mut aes_key = [0u8; 32];
        aes_key.copy_from_slice(&encrypted.key[..32]);
        let dec = CbcRangeDecryptor::new(mmap, &aes_key, encrypted.plaintext_size).unwrap();

        assert_eq!(dec.plaintext_size(), plaintext.len());

        // Fixed edge cases: start, block-unaligned, single byte, whole file,
        // a window that runs off the end, and past-EOF.
        let cases: &[(usize, usize)] = &[
            (0, 16),
            (0, 65536),
            (1, 15),
            (13, 40),
            (16, 16),
            (100_000, 65536),
            (plaintext.len() - 5, 100), // crosses EOF -> clamps to 5 bytes
            (plaintext.len(), 10),      // fully past EOF -> empty
            (0, plaintext.len()),       // whole file
        ];
        for &(off, len) in cases {
            let got = dec.read_range(off, len);
            let end = (off + len).min(plaintext.len());
            let want = if off >= plaintext.len() {
                &[][..]
            } else {
                &plaintext[off..end]
            };
            assert_eq!(got.as_slice(), want, "mismatch at off={off} len={len}");
        }

        // A spread of deterministic pseudo-random windows (no rng dep).
        let mut seed = 0x9e3779b97f4a7c15u64;
        for _ in 0..500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let off = (seed >> 17) as usize % plaintext.len();
            let len = 1 + ((seed >> 33) as usize % 70_000);
            let got = dec.read_range(off, len);
            let end = (off + len).min(plaintext.len());
            assert_eq!(
                got.as_slice(),
                &plaintext[off..end],
                "mismatch at off={off} len={len}"
            );
        }
    }

    /// The Read+Seek adapter must reproduce the plaintext under sequential reads
    /// and after seeks (what the audio decoder does).
    #[test]
    fn range_reader_read_and_seek() {
        let plaintext: Vec<u8> = (0..200_000usize).map(|i| (i % 251) as u8).collect();
        let (dec, _keep) = make_decryptor(&plaintext);

        // Sequential read in odd-sized chunks must yield the whole plaintext.
        let mut reader = CbcRangeReader::new(dec.clone());
        assert_eq!(reader.len(), plaintext.len() as u64);
        let mut out = Vec::new();
        let mut buf = [0u8; 7000];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, plaintext);

        // Seek (Start/End/Current) then read must land on the right bytes.
        let mut reader = CbcRangeReader::new(dec);
        reader.seek(SeekFrom::Start(50_003)).unwrap();
        let mut b = [0u8; 100];
        let n = reader.read(&mut b).unwrap();
        assert_eq!(&b[..n], &plaintext[50_003..50_003 + n]);

        let p = reader.seek(SeekFrom::End(-20)).unwrap();
        assert_eq!(p, plaintext.len() as u64 - 20);
        let n = reader.read(&mut b).unwrap();
        assert_eq!(&b[..n], &plaintext[plaintext.len() - 20..]);

        // Seek past EOF -> read returns 0.
        reader
            .seek(SeekFrom::Start(plaintext.len() as u64 + 999))
            .unwrap();
        assert_eq!(reader.read(&mut b).unwrap(), 0);

        // Offsets a hostile media file can steer a decoder into: errors or
        // EOF, never a wrap or a panic.
        reader.seek(SeekFrom::Start(u64::MAX)).unwrap();
        assert_eq!(reader.read(&mut b).unwrap(), 0);
        assert!(reader.seek(SeekFrom::Current(i64::MAX)).is_err());
        assert!(reader.seek(SeekFrom::End(i64::MIN)).is_err());
    }
}
