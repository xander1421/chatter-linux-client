//! The AES half of random-access CBC decryption.
//!
//! `services::attachment::CbcRangeDecryptor` in chatter-core owns the mmap,
//! the size checks and the trace probe; the block loop lives here so the
//! `Aes256` instantiation is compiled at this crate's opt-level.
//!
//! File layout: `IV(16) ‖ AES-256-CBC ciphertext (block-aligned, raw/no padding) ‖ HMAC-SHA256(32)`.
//!
//! CBC is randomly readable: `plaintext[b] = AES_dec(C[b]) ⊕ C[b-1]`, with
//! `C[-1]` = IV. So any range can be decrypted by touching only its covering
//! ciphertext blocks plus the one preceding block for the chain.
//!
//! ## Security
//!
//! Range reads are **unauthenticated on their own**. The caller MUST verify the
//! whole-file HMAC (via [`crate::StreamingDecryptor::verify`]) first.

use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, KeyInit};

const IV_LEN: usize = 16;
const AES_BLOCK: usize = 16;

/// AES-256 key schedule for decrypting plaintext byte-ranges of a CBC blob.
pub struct CbcRangeCipher {
    cipher: Aes256,
}

impl CbcRangeCipher {
    pub fn new(aes_key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256::new(aes_key.into()),
        }
    }

    /// Ciphertext block `b` (0-based), 16 bytes.
    #[inline]
    fn cipher_block(file: &[u8], b: usize) -> &[u8] {
        let start = IV_LEN + b * AES_BLOCK;
        &file[start..start + AES_BLOCK]
    }

    /// CBC chaining input for block `b`: the previous ciphertext block, or the
    /// IV for block 0.
    #[inline]
    fn chain_block(file: &[u8], b: usize) -> &[u8] {
        if b == 0 {
            &file[0..IV_LEN]
        } else {
            Self::cipher_block(file, b - 1)
        }
    }

    /// Decrypt plaintext bytes `[off, off + output.len())` of `file`, clamped to
    /// `plaintext_size`, into `output`. Returns the number of bytes written.
    ///
    /// `file` is the whole ciphertext blob; the caller has checked that it is
    /// block-aligned and that `plaintext_size` fits inside it.
    pub fn decrypt_range_into(
        &self,
        file: &[u8],
        plaintext_size: usize,
        off: usize,
        output: &mut [u8],
    ) -> usize {
        if off >= plaintext_size || output.is_empty() {
            return 0;
        }
        let end = off.saturating_add(output.len()).min(plaintext_size);
        let start_block = off / AES_BLOCK;
        let end_block = (end - 1) / AES_BLOCK; // inclusive; end <= encrypted_len so in range
        for b in start_block..=end_block {
            let mut blk = [0u8; AES_BLOCK];
            blk.copy_from_slice(Self::cipher_block(file, b));
            self.cipher.decrypt_block((&mut blk).into());
            let chain = Self::chain_block(file, b);
            for i in 0..AES_BLOCK {
                blk[i] ^= chain[i];
            }
            let block_start = b * AES_BLOCK;
            let from = off.max(block_start);
            let to = end.min(block_start + AES_BLOCK);
            output[from - off..to - off]
                .copy_from_slice(&blk[from - block_start..to - block_start]);
            blk.fill(0);
        }
        end - off
    }
}
