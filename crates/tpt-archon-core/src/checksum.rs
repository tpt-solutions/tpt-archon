//! Dependency-free CRC32 (IEEE 802.3, reflected) used for on-disk integrity
//! checks, and the reserved byte count per block for a page checksum.
//!
//! Keeping this tiny primitive in its own module lets the WAL frames
//! (`crate::wal`) and the page frames (`crate::page`) share one implementation
//! instead of each carrying a private copy.

/// The number of trailing bytes reserved in every on-disk block for a CRC32
/// page checksum. The logical page stays [`PAGE_SIZE`](crate::page::PAGE_SIZE)
/// bytes; the on-disk block is `PAGE_SIZE + PAGE_CRC` bytes.
pub const PAGE_CRC: usize = 4;

/// CRC32 (IEEE 802.3, reflected) over `data`.
///
/// Bit-by-bit (no table) so it stays `no_std`/zero-alloc and trivially
/// auditable. Performance is irrelevant here — checksums run once per block
/// on the read/write path, not in any hot inner loop.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_is_stable_and_distinguishes_inputs() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_ne!(crc32(b"hello"), crc32(b"hellp"));
    }
}
