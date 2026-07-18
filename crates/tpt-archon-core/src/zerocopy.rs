//! Zero-allocation primitives for page-sized I/O and record framing.
//!
//! These types exist *because there is no `tpt-zero-bytes` crate* in the TPT
//! ecosystem (it was never built). They live here so the storage engine's
//! read/write hot path performs no heap allocation and no `serde`-style
//! reflection.
//!
//! Two things are provided:
//!
//! - [`FixedBuf`] — a `const`-sized, stack/inline byte buffer with a tracked
//!   length, suitable for building a page or a WAL record without touching the
//!   heap.
//! - [`Cursor`] and [`Reader`] — little-endian, bounds-checked, zero-copy
//!   encode/decode helpers for page headers and WAL records. No `unsafe`.

use core::fmt;

/// Error produced when a zero-copy read/write would exceed the buffer bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfSpace {
    /// Bytes requested.
    pub needed: usize,
    /// Bytes actually available.
    pub available: usize,
}

impl fmt::Display for OutOfSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "zero-copy buffer out of space: needed {}, available {}",
            self.needed, self.available
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for OutOfSpace {}

/// A fixed-capacity, inline byte buffer with a tracked used length.
///
/// `N` bytes are stored inline; no allocation ever occurs. Use it to assemble
/// a page (`FixedBuf<4096>`) or a WAL record on the stack before handing the
/// bytes to a [`BlockDevice`](crate::block::BlockDevice).
#[derive(Clone, Copy)]
pub struct FixedBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> FixedBuf<N> {
    /// Creates an empty buffer (all zero bytes, length 0).
    #[inline]
    pub const fn new() -> Self {
        Self {
            bytes: [0u8; N],
            len: 0,
        }
    }

    /// Total capacity in bytes (`N`).
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of bytes currently used.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no bytes are used.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Remaining free bytes.
    #[inline]
    pub const fn remaining(&self) -> usize {
        N - self.len
    }

    /// The used portion of the buffer.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The full backing array, regardless of used length.
    ///
    /// Useful for handing a full page to a block device.
    #[inline]
    pub fn as_array(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Mutable view of the full backing array.
    #[inline]
    pub fn as_array_mut(&mut self) -> &mut [u8; N] {
        &mut self.bytes
    }

    /// Sets the used length. Panics if `len > N`.
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= N, "set_len({len}) exceeds capacity {N}");
        self.len = len;
    }

    /// Appends `data`, returning [`OutOfSpace`] if it would overflow.
    pub fn extend_from_slice(&mut self, data: &[u8]) -> Result<(), OutOfSpace> {
        if data.len() > self.remaining() {
            return Err(OutOfSpace {
                needed: data.len(),
                available: self.remaining(),
            });
        }
        self.bytes[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
        Ok(())
    }

    /// Builds a buffer from an existing array, marking all `N` bytes used.
    #[inline]
    pub const fn from_array(bytes: [u8; N]) -> Self {
        Self { bytes, len: N }
    }
}

impl<const N: usize> Default for FixedBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Debug for FixedBuf<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixedBuf")
            .field("capacity", &N)
            .field("len", &self.len)
            .finish()
    }
}

/// A little-endian, bounds-checked writer over a mutable byte slice.
///
/// Zero-copy: it writes directly into the slice you give it, no allocation.
pub struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Wraps a mutable slice.
    #[inline]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes written so far.
    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&mut [u8], OutOfSpace> {
        let available = self.buf.len() - self.pos;
        if n > available {
            return Err(OutOfSpace {
                needed: n,
                available,
            });
        }
        let start = self.pos;
        self.pos += n;
        Ok(&mut self.buf[start..start + n])
    }

    /// Writes a `u8`.
    pub fn put_u8(&mut self, v: u8) -> Result<(), OutOfSpace> {
        self.take(1)?[0] = v;
        Ok(())
    }

    /// Writes a little-endian `u16`.
    pub fn put_u16(&mut self, v: u16) -> Result<(), OutOfSpace> {
        self.take(2)?.copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    /// Writes a little-endian `u32`.
    pub fn put_u32(&mut self, v: u32) -> Result<(), OutOfSpace> {
        self.take(4)?.copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    /// Writes a little-endian `u64`.
    pub fn put_u64(&mut self, v: u64) -> Result<(), OutOfSpace> {
        self.take(8)?.copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    /// Writes raw bytes.
    pub fn put_bytes(&mut self, data: &[u8]) -> Result<(), OutOfSpace> {
        self.take(data.len())?.copy_from_slice(data);
        Ok(())
    }
}

/// A little-endian, bounds-checked reader over a byte slice.
///
/// Zero-copy: [`Reader::read_bytes`] borrows directly from the underlying
/// slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Wraps a slice.
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes consumed so far.
    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes remaining.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], OutOfSpace> {
        if n > self.remaining() {
            return Err(OutOfSpace {
                needed: n,
                available: self.remaining(),
            });
        }
        let start = self.pos;
        self.pos += n;
        Ok(&self.buf[start..start + n])
    }

    /// Reads a `u8`.
    pub fn read_u8(&mut self) -> Result<u8, OutOfSpace> {
        Ok(self.take(1)?[0])
    }

    /// Reads a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, OutOfSpace> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Reads a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, OutOfSpace> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a little-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, OutOfSpace> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Borrows `n` raw bytes with no copy.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], OutOfSpace> {
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_buf_extend_and_bounds() {
        let mut b = FixedBuf::<8>::new();
        assert!(b.is_empty());
        b.extend_from_slice(&[1, 2, 3]).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b.as_slice(), &[1, 2, 3]);
        assert_eq!(b.remaining(), 5);
        assert_eq!(
            b.extend_from_slice(&[0; 6]),
            Err(OutOfSpace {
                needed: 6,
                available: 5
            })
        );
    }

    #[test]
    fn cursor_reader_round_trip() {
        let mut backing = [0u8; 32];
        {
            let mut c = Cursor::new(&mut backing);
            c.put_u8(0xAB).unwrap();
            c.put_u16(0x1234).unwrap();
            c.put_u32(0xDEAD_BEEF).unwrap();
            c.put_u64(0x0102_0304_0506_0708).unwrap();
            c.put_bytes(b"hi").unwrap();
            assert_eq!(c.position(), 1 + 2 + 4 + 8 + 2);
        }
        let mut r = Reader::new(&backing);
        assert_eq!(r.read_u8().unwrap(), 0xAB);
        assert_eq!(r.read_u16().unwrap(), 0x1234);
        assert_eq!(r.read_u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.read_u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.read_bytes(2).unwrap(), b"hi");
    }

    #[test]
    fn reader_bounds_checked() {
        let data = [0u8; 2];
        let mut r = Reader::new(&data);
        assert!(r.read_u32().is_err());
        assert_eq!(r.remaining(), 2);
    }
}
