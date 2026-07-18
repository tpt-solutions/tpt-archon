//! Append-only, LSN-ordered write-ahead log with crash-recovery replay.
//!
//! Every page modification is framed as a [`WalRecord`] and appended to the log
//! *before* the page is written back to main storage (the write-ahead
//! invariant). Each record carries a monotonically increasing
//! [`Lsn`](Log Sequence Number) and a CRC32 checksum so a torn tail write can
//! be detected and truncated during recovery.
//!
//! # Recovery consistency
//!
//! [`Wal::replay`] walks the log from the start, verifying each record's
//! checksum and stopping at the first record that fails to parse or checksum.
//! Because records are only ever appended and each is self-describing, replay
//! after a crash yields exactly the prefix of records that were fully durable —
//! the property `tpt-telos` is intended to prove for this module (see
//! `formal-proofs/`). Until that proof exists, the invariant is exercised by
//! the crash-simulation tests below.
//!
//! The record framing here uses only the zero-copy helpers from
//! [`crate::zerocopy`]; no `serde`, no allocation on the encode path.

use alloc::vec::Vec;

use crate::zerocopy::{Cursor, OutOfSpace, Reader};

/// A Log Sequence Number: a monotonically increasing record identifier.
pub type Lsn = u64;

/// The kind of operation a [`WalRecord`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// A full-page image written before the page reaches main storage.
    PageWrite = 1,
    /// A transaction commit marker.
    Commit = 2,
    /// A checkpoint marker: everything before it is known durable in main
    /// storage.
    Checkpoint = 3,
}

impl RecordKind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(RecordKind::PageWrite),
            2 => Some(RecordKind::Commit),
            3 => Some(RecordKind::Checkpoint),
            _ => None,
        }
    }
}

/// A single write-ahead log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// This record's sequence number.
    pub lsn: Lsn,
    /// What the record describes.
    pub kind: RecordKind,
    /// The block the record pertains to (0 for markers that have no block).
    pub block_id: u64,
    /// Opaque payload (e.g. a full page image for `PageWrite`).
    pub payload: Vec<u8>,
}

/// Errors from WAL encoding/decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalError {
    /// The record did not fit in the provided buffer.
    OutOfSpace,
    /// The on-disk bytes were too short or malformed to decode.
    Malformed,
    /// The record's checksum did not match (torn/corrupt write).
    BadChecksum,
    /// An unknown [`RecordKind`] tag was encountered.
    UnknownKind(u8),
}

impl From<OutOfSpace> for WalError {
    fn from(_: OutOfSpace) -> Self {
        WalError::OutOfSpace
    }
}

// Frame layout (little-endian):
//   u32 total_len (of everything after this field, including crc)
//   u64 lsn
//   u8  kind
//   u64 block_id
//   u32 payload_len
//   [payload_len] payload
//   u32 crc32   (over: lsn..=payload)
const HEADER_LEN: usize = 8 + 1 + 8 + 4; // lsn + kind + block_id + payload_len

/// CRC32 (IEEE) over `data`, implemented inline (no external crate).
fn crc32(data: &[u8]) -> u32 {
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

impl WalRecord {
    /// The number of bytes this record occupies on disk (including framing).
    pub fn encoded_len(&self) -> usize {
        4 + HEADER_LEN + self.payload.len() + 4
    }

    /// Encodes the record into `out`, returning the number of bytes written.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WalError> {
        let body_len = HEADER_LEN + self.payload.len() + 4; // + crc
        let end_before_crc;
        {
            let mut c = Cursor::new(out);
            c.put_u32(body_len as u32)?;
            c.put_u64(self.lsn)?;
            c.put_u8(self.kind as u8)?;
            c.put_u64(self.block_id)?;
            c.put_u32(self.payload.len() as u32)?;
            c.put_bytes(&self.payload)?;
            end_before_crc = c.position();
        }
        // CRC over the body excluding the leading total_len and the crc itself.
        let crc = crc32(&out[4..end_before_crc]);
        let mut c = Cursor::new(&mut out[end_before_crc..]);
        c.put_u32(crc)?;
        Ok(end_before_crc + 4)
    }

    /// Decodes one record from the front of `data`, returning the record and
    /// the number of bytes consumed.
    pub fn decode(data: &[u8]) -> Result<(WalRecord, usize), WalError> {
        let mut r = Reader::new(data);
        let body_len = r.read_u32().map_err(|_| WalError::Malformed)? as usize;
        if r.remaining() < body_len {
            return Err(WalError::Malformed);
        }
        let body_start = r.position();
        let lsn = r.read_u64().map_err(|_| WalError::Malformed)?;
        let kind_tag = r.read_u8().map_err(|_| WalError::Malformed)?;
        let kind = RecordKind::from_u8(kind_tag).ok_or(WalError::UnknownKind(kind_tag))?;
        let block_id = r.read_u64().map_err(|_| WalError::Malformed)?;
        let payload_len = r.read_u32().map_err(|_| WalError::Malformed)? as usize;
        let payload = r
            .read_bytes(payload_len)
            .map_err(|_| WalError::Malformed)?
            .to_vec();
        let stored_crc = r.read_u32().map_err(|_| WalError::Malformed)?;
        let crc_region_end = r.position() - 4;
        let computed = crc32(&data[body_start..crc_region_end]);
        if computed != stored_crc {
            return Err(WalError::BadChecksum);
        }
        Ok((
            WalRecord {
                lsn,
                kind,
                block_id,
                payload,
            },
            4 + body_len,
        ))
    }
}

/// An in-memory write-ahead log.
///
/// The log owns a byte buffer of appended records. In a full deployment this
/// would be flushed to a dedicated log region on a
/// [`BlockDevice`](crate::block::BlockDevice); the append/replay semantics and
/// the on-disk framing are identical, so persistence is a matter of writing
/// [`Wal::as_bytes`] out and re-loading via [`Wal::from_bytes`].
#[derive(Debug, Default, Clone)]
pub struct Wal {
    bytes: Vec<u8>,
    next_lsn: Lsn,
}

impl Wal {
    /// Creates an empty log; the first appended record gets LSN 0.
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            next_lsn: 0,
        }
    }

    /// The LSN the next appended record will receive.
    pub fn next_lsn(&self) -> Lsn {
        self.next_lsn
    }

    /// The raw log bytes, suitable for persisting to a device.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Appends a record with `kind`/`block_id`/`payload`, assigning it the next
    /// LSN. Returns the assigned LSN.
    ///
    /// This is the write-ahead step: callers must append here *before* writing
    /// the corresponding page to main storage.
    pub fn append(&mut self, kind: RecordKind, block_id: u64, payload: &[u8]) -> Lsn {
        let lsn = self.next_lsn;
        let rec = WalRecord {
            lsn,
            kind,
            block_id,
            payload: payload.to_vec(),
        };
        let mut frame = alloc::vec![0u8; rec.encoded_len()];
        let n = rec.encode(&mut frame).expect("sized buffer");
        self.bytes.extend_from_slice(&frame[..n]);
        self.next_lsn += 1;
        lsn
    }

    /// Replays the log, invoking `f` for each intact record in LSN order.
    ///
    /// Replay stops at the first malformed or bad-checksum record (a torn tail
    /// write after a crash) and returns the number of records successfully
    /// replayed. Records already applied are never lost, and no partial record
    /// is ever surfaced — the crash-consistency invariant.
    pub fn replay<F: FnMut(&WalRecord)>(&self, mut f: F) -> usize {
        let mut offset = 0;
        let mut count = 0;
        while offset < self.bytes.len() {
            match WalRecord::decode(&self.bytes[offset..]) {
                Ok((rec, consumed)) => {
                    f(&rec);
                    offset += consumed;
                    count += 1;
                }
                Err(_) => break, // torn/corrupt tail: stop, keep the good prefix
            }
        }
        count
    }

    /// Reconstructs a log from previously persisted bytes, truncating any torn
    /// tail. `next_lsn` is set to one past the last intact record.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut offset = 0;
        let mut last_lsn: Option<Lsn> = None;
        while offset < bytes.len() {
            match WalRecord::decode(&bytes[offset..]) {
                Ok((rec, consumed)) => {
                    last_lsn = Some(rec.lsn);
                    offset += consumed;
                }
                Err(_) => break,
            }
        }
        Self {
            bytes: bytes[..offset].to_vec(),
            next_lsn: last_lsn.map(|l| l + 1).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips() {
        let rec = WalRecord {
            lsn: 7,
            kind: RecordKind::PageWrite,
            block_id: 42,
            payload: alloc::vec![1, 2, 3, 4, 5],
        };
        let mut buf = alloc::vec![0u8; rec.encoded_len()];
        let n = rec.encode(&mut buf).unwrap();
        let (decoded, consumed) = WalRecord::decode(&buf[..n]).unwrap();
        assert_eq!(decoded, rec);
        assert_eq!(consumed, n);
    }

    #[test]
    fn append_assigns_monotonic_lsns_and_replays_in_order() {
        let mut wal = Wal::new();
        assert_eq!(wal.append(RecordKind::PageWrite, 1, b"a"), 0);
        assert_eq!(wal.append(RecordKind::PageWrite, 2, b"bb"), 1);
        assert_eq!(wal.append(RecordKind::Commit, 0, b""), 2);

        let mut seen = Vec::new();
        let n = wal.replay(|r| seen.push((r.lsn, r.kind, r.block_id)));
        assert_eq!(n, 3);
        assert_eq!(seen[0], (0, RecordKind::PageWrite, 1));
        assert_eq!(seen[1], (1, RecordKind::PageWrite, 2));
        assert_eq!(seen[2], (2, RecordKind::Commit, 0));
    }

    #[test]
    fn torn_tail_write_is_truncated_on_replay() {
        let mut wal = Wal::new();
        wal.append(RecordKind::PageWrite, 1, b"good");
        wal.append(RecordKind::PageWrite, 2, b"also-good");
        wal.append(RecordKind::PageWrite, 3, b"torn");

        // Simulate a crash mid-write: corrupt the last few bytes.
        let mut raw = wal.as_bytes().to_vec();
        let len = raw.len();
        for b in raw.iter_mut().skip(len - 3) {
            *b ^= 0xFF;
        }

        let recovered = Wal::from_bytes(&raw);
        let mut lsns = Vec::new();
        let n = recovered.replay(|r| lsns.push(r.lsn));
        assert_eq!(n, 2, "only the two fully-durable records survive");
        assert_eq!(lsns, alloc::vec![0, 1]);
        assert_eq!(recovered.next_lsn(), 2);
    }

    #[test]
    fn bad_checksum_is_detected() {
        let rec = WalRecord {
            lsn: 0,
            kind: RecordKind::PageWrite,
            block_id: 1,
            payload: alloc::vec![9, 9, 9],
        };
        let mut buf = alloc::vec![0u8; rec.encoded_len()];
        rec.encode(&mut buf).unwrap();
        // Flip a payload byte without fixing the crc.
        buf[HEADER_LEN + 4 + 1] ^= 0x01;
        assert_eq!(WalRecord::decode(&buf), Err(WalError::BadChecksum));
    }

    #[test]
    fn empty_log_replays_nothing() {
        let wal = Wal::new();
        let n = wal.replay(|_| panic!("no records expected"));
        assert_eq!(n, 0);
        assert_eq!(wal.next_lsn(), 0);
    }
}
