//! The row TLV (tag-length-value) codec, MVCC write-buffer tagging, and
//! literal-to-value conversion — the encode/decode primitives every DML path
//! builds on.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::executor::Value;
use crate::parser::Literal;

use super::schema::{ColumnType, DbError, Schema};

/// MVCC-buffered-write status tag: the row bytes that follow are live.
const MVCC_LIVE: u8 = 0;
/// MVCC-buffered-write status tag: the row was deleted within the transaction.
pub(super) const MVCC_TOMBSTONE: u8 = 1;

/// Wraps encoded row bytes with the MVCC live-row status tag.
pub(super) fn mvcc_wrap_row(values: &[Value]) -> Vec<u8> {
    let mut out = vec![MVCC_LIVE];
    out.extend_from_slice(&encode_row(values));
    out
}

/// The MVCC tombstone marker for a row deleted within a transaction.
pub(super) fn mvcc_wrap_tombstone() -> Vec<u8> {
    vec![MVCC_TOMBSTONE]
}

pub(super) fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u16).to_le_bytes());
    for v in values {
        match v {
            Value::Int(i) => {
                out.push(0);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Text(t) => {
                out.push(1);
                out.extend_from_slice(&(t.len() as u32).to_le_bytes());
                out.extend_from_slice(t.as_bytes());
            }
            Value::Vector(vec) => {
                out.push(2);
                out.extend_from_slice(&(vec.len() as u32).to_le_bytes());
                for f in vec {
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
            Value::Float(f) => {
                out.push(4);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Null => {
                out.push(3);
            }
        }
    }
    out
}

pub(super) fn try_decode_row(bytes: &[u8]) -> Result<Vec<Value>, DbError> {
    if bytes.len() < 2 {
        return Err(DbError::CorruptRow(0));
    }
    let mut pos = 0usize;
    let n = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
    pos += 2;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        if pos >= bytes.len() {
            return Err(DbError::CorruptRow(0));
        }
        let tag = bytes[pos];
        pos += 1;
        match tag {
            0 => {
                if pos + 8 > bytes.len() {
                    return Err(DbError::CorruptRow(0));
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&bytes[pos..pos + 8]);
                pos += 8;
                row.push(Value::Int(i64::from_le_bytes(b)));
            }
            1 => {
                if pos + 4 > bytes.len() {
                    return Err(DbError::CorruptRow(0));
                }
                let len = u32::from_le_bytes([
                    bytes[pos],
                    bytes[pos + 1],
                    bytes[pos + 2],
                    bytes[pos + 3],
                ]) as usize;
                pos += 4;
                if pos + len > bytes.len() {
                    return Err(DbError::CorruptRow(0));
                }
                let s = String::from_utf8_lossy(&bytes[pos..pos + len]).into_owned();
                pos += len;
                row.push(Value::Text(s));
            }
            2 => {
                if pos + 4 > bytes.len() {
                    return Err(DbError::CorruptRow(0));
                }
                let len = u32::from_le_bytes([
                    bytes[pos],
                    bytes[pos + 1],
                    bytes[pos + 2],
                    bytes[pos + 3],
                ]) as usize;
                pos += 4;
                let vec_bytes = len * 4;
                if pos + vec_bytes > bytes.len() {
                    return Err(DbError::CorruptRow(0));
                }
                let mut vec = Vec::with_capacity(len);
                for _ in 0..len {
                    let mut b = [0u8; 4];
                    b.copy_from_slice(&bytes[pos..pos + 4]);
                    pos += 4;
                    vec.push(f32::from_le_bytes(b));
                }
                row.push(Value::Vector(vec));
            }
            3 => row.push(Value::Null),
            4 => {
                if pos + 4 > bytes.len() {
                    return Err(DbError::CorruptRow(0));
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&bytes[pos..pos + 4]);
                pos += 4;
                row.push(Value::Float(f32::from_le_bytes(b)));
            }
            5 => row.push(Value::Null),
            _ => row.push(Value::Int(0)),
        }
    }
    Ok(row)
}

pub(super) fn decode_row_validated(
    id: u64,
    bytes: &[u8],
    col_count: usize,
) -> Result<Vec<Value>, DbError> {
    let row = try_decode_row(bytes)?;
    if row.len() != col_count {
        return Err(DbError::CorruptRow(id));
    }
    Ok(row)
}

pub(super) fn literal_to_value(
    schema: &Schema,
    slot: usize,
    lit: &Literal,
) -> Result<Value, DbError> {
    let expected = &schema.types[slot];
    match (expected, lit) {
        (_, Literal::Null) => Ok(Value::Null),
        (ColumnType::Int, Literal::Int(i)) => Ok(Value::Int(*i)),
        (ColumnType::Float, Literal::Float(f)) => Ok(Value::Float(*f)),
        (ColumnType::Float, Literal::Int(i)) => Ok(Value::Float(*i as f32)),
        (ColumnType::Double, Literal::Float(f)) => Ok(Value::Float(*f)),
        (ColumnType::Double, Literal::Int(i)) => Ok(Value::Float(*i as f32)),
        (ColumnType::Text, Literal::Text(t)) => Ok(Value::Text(t.clone())),
        (ColumnType::Varchar(_), Literal::Text(t)) => Ok(Value::Text(t.clone())),
        (ColumnType::Vector, Literal::Vector(v)) => Ok(Value::Vector(v.clone())),
        (ColumnType::Boolean, Literal::Int(0)) => Ok(Value::Int(0)),
        (ColumnType::Boolean, Literal::Int(1)) => Ok(Value::Int(1)),
        (ColumnType::Numeric, Literal::Int(i)) => Ok(Value::Int(*i)),
        (ColumnType::Date, Literal::Int(i)) => Ok(Value::Int(*i)),
        (ColumnType::Timestamp, Literal::Int(i)) => Ok(Value::Int(*i)),
        _ => Err(DbError::ColumnTypeMismatch(schema.columns[slot].clone())),
    }
}
