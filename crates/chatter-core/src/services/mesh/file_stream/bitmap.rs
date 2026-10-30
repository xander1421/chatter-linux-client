/// Which chunks of a transfer have landed.
///
/// Sans-IO and separately testable: the resume logic is bit arithmetic, and the
/// consequence of getting it wrong is a transfer that either never completes or
/// completes with a hole in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkBitmap {
    bits: Vec<u8>,
    count: u32,
    present: u32,
}

impl ChunkBitmap {
    #[must_use]
    pub fn new(count: u32) -> Self {
        Self {
            bits: vec![0u8; (count as usize).div_ceil(8)],
            count,
            present: 0,
        }
    }

    /// Restores a bitmap from its persisted bytes.
    ///
    /// Rejects a byte length that does not match `count` rather than padding or
    /// truncating: a bitmap of the wrong width for its manifest describes a
    /// different transfer, and guessing which one would resume into a hole.
    pub fn from_bytes(bytes: &[u8], count: u32) -> Option<Self> {
        if bytes.len() != (count as usize).div_ceil(8) {
            return None;
        }
        // Bits past `count` in the final byte must be clear, or `present` and
        // `is_complete` disagree with `missing`.
        let mut map = Self {
            bits: bytes.to_vec(),
            count,
            present: 0,
        };
        map.present = (0..count).filter(|index| map.contains(*index)).count() as u32;
        if map.bits.iter().map(|b| b.count_ones()).sum::<u32>() != map.present {
            return None;
        }
        Some(map)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        if index >= self.count {
            return false;
        }
        let byte = (index / 8) as usize;
        self.bits
            .get(byte)
            .is_some_and(|bits| bits & (1 << (index % 8)) != 0)
    }

    /// Marks a chunk received. `true` when this was news.
    pub fn insert(&mut self, index: u32) -> bool {
        if index >= self.count || self.contains(index) {
            return false;
        }
        let byte = (index / 8) as usize;
        if let Some(bits) = self.bits.get_mut(byte) {
            *bits |= 1 << (index % 8);
            self.present += 1;
            return true;
        }
        false
    }

    #[must_use]
    pub fn present(&self) -> u32 {
        self.present
    }

    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.present == self.count
    }

    /// The first `limit` chunks still missing, ascending.
    ///
    /// Ascending and bounded because that is what a retransmit request can carry:
    /// asking for the earliest gaps first means repeated rounds walk forward
    /// through the file instead of re-asking for an arbitrary subset.
    #[must_use]
    pub fn missing(&self, limit: usize) -> Vec<u32> {
        (0..self.count)
            .filter(|index| !self.contains(*index))
            .take(limit)
            .collect()
    }
}
