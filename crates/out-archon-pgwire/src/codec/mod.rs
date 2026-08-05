//! PostgreSQL wire-protocol message framing: read and write the length-prefixed
//! message format (type byte + 4-byte big-endian length + payload).

pub mod message;

use crate::codec::message::{BackendTag, FrontendMessage, StartupMessage, TxnStatus};
use bytes::{Buf, BufMut, BytesMut};
use tpt_archon_relational::executor::Value;

/// Protocol version number (3.0 = 196608)
pub const PROTOCOL_VERSION: u32 = 196608;

/// Transaction status for ReadyForQuery messages.
pub type TransactionStatus = TxnStatus;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("incomplete message: need {needed} more bytes, got {got}")]
    Incomplete { needed: usize, got: usize },
    #[error("invalid message length {0}")]
    InvalidLength(usize),
    #[error("unexpected frontend tag: {0:#x}")]
    UnexpectedTag(u8),
    #[error("parse error: {0}")]
    Parse(String),
}

pub type CodecResult<T> = Result<T, CodecError>;

/// Reader state machine: buffers bytes until a full message is available.
#[derive(Debug, Clone, Default)]
pub struct MessageReader {
    buf: Vec<u8>,
    state: ReaderState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ReaderState {
    #[default]
    Tag,
    Length,
    Body {
        len: usize,
        tag: u8,
        body_pos: usize,
    },
}

impl ReaderState {
    fn default_for_body(len: usize, tag: u8) -> Self {
        Self::Body {
            len,
            tag,
            body_pos: 0,
        }
    }
}

impl MessageReader {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            state: ReaderState::Tag,
        }
    }

    /// Feed bytes into the reader. Returns any fully parsed messages.
    pub fn feed(&mut self, bytes: &[u8]) -> CodecResult<Vec<FrontendMessage>> {
        self.buf.extend_from_slice(bytes);
        let mut msgs = Vec::new();
        loop {
            match self.state {
                ReaderState::Tag => {
                    if self.buf.is_empty() {
                        break;
                    }
                    self.buf.remove(0);
                    self.state = ReaderState::Length;
                }
                ReaderState::Length => {
                    if self.buf.len() < 4 {
                        break;
                    }
                    let len = ((self.buf[0] as usize) << 24)
                        | ((self.buf[1] as usize) << 16)
                        | ((self.buf[2] as usize) << 8)
                        | (self.buf[3] as usize);
                    self.buf.drain(0..4);
                    if len < 4 {
                        return Err(CodecError::InvalidLength(len));
                    }
                    let payload_len = len - 4;
                    let tag = match self.state {
                        ReaderState::Body { tag, .. } => tag,
                        _ => 0,
                    };
                    self.state = ReaderState::default_for_body(payload_len, tag);
                }
                ReaderState::Body { len, tag, body_pos } => {
                    let available = self.buf.len().min(len - body_pos);
                    if available == 0 {
                        break;
                    }
                    let new_pos = body_pos + available;
                    if new_pos < len {
                        self.state = ReaderState::Body {
                            len,
                            tag,
                            body_pos: new_pos,
                        };
                        break;
                    }
                    let body: Vec<u8> = self.buf.drain(0..len).collect();
                    let msg = parse_frontend_message(tag, &body)?;
                    msgs.push(msg);
                    self.state = ReaderState::Tag;
                }
            }
        }
        Ok(msgs)
    }
}

fn parse_frontend_message(tag: u8, body: &[u8]) -> CodecResult<FrontendMessage> {
    match tag {
        0x00 => parse_startup(body),
        b'Q' => Ok(FrontendMessage::Query(string_from_body(body))),
        b'X' => Ok(FrontendMessage::Terminate),
        b'p' => Ok(FrontendMessage::Password(string_from_body(body))),
        b'P' | b'B' | b'D' | b'E' | b'S' | b'C' => Ok(FrontendMessage::Parse),
        _ => Err(CodecError::UnexpectedTag(tag)),
    }
}

fn parse_startup(body: &[u8]) -> CodecResult<FrontendMessage> {
    let s = string_from_body(body);
    let mut params = Vec::new();
    for part in s.split('\0') {
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('\0') {
            params.push((k.to_string(), v.to_string()));
        }
    }
    Ok(FrontendMessage::Startup(StartupMessage {
        protocol_major: 3,
        protocol_minor: 0,
        params,
    }))
}

fn string_from_body(body: &[u8]) -> String {
    core::str::from_utf8(body)
        .unwrap_or("")
        .trim_end_matches('\0')
        .to_string()
}

/// Encodes a backend message into `type_byte + length + payload` bytes.
#[derive(Debug, Clone, Default)]
pub struct MessageWriter {
    out: Vec<u8>,
}

impl MessageWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.out
    }

    pub fn bytes(&self) -> &[u8] {
        &self.out
    }

    pub fn write_auth_ok(&mut self) {
        self.start(BackendTag::AUTH.0);
        self.write_i32(8);
        self.out.extend_from_slice(&0u32.to_be_bytes());
        self.finish();
    }

    pub fn write_auth_plain(&mut self) {
        self.start(BackendTag::AUTH.0);
        self.write_i32(8);
        self.out.extend_from_slice(&3u32.to_be_bytes());
        self.finish();
    }

    /// Writes an AuthenticationSASL message (mechanism list).
    pub fn write_auth_sasl(&mut self, mechanisms: &[&str]) {
        self.start(BackendTag::AUTH.0);
        // Auth type 10 = SASL
        self.write_i32(4 + 4 + mechanisms.iter().map(|m| m.len() + 1).sum::<usize>() as i32 + 1);
        self.out.extend_from_slice(&10u32.to_be_bytes()); // SASL auth type
        for mech in mechanisms {
            self.out.extend_from_slice(mech.as_bytes());
            self.out.push(0);
        }
        self.out.push(0); // Null terminator for mechanism list
        self.finish();
    }

    /// Writes an AuthenticationSASLContinue message.
    pub fn write_auth_sasl_continue(&mut self, data: &str) {
        self.start(BackendTag::AUTH.0);
        // Auth type 11 = SASL Continue
        let data_bytes = data.as_bytes();
        self.write_i32(4 + 4 + data_bytes.len() as i32);
        self.out.extend_from_slice(&11u32.to_be_bytes()); // SASL Continue auth type
        self.out.extend_from_slice(data_bytes);
        self.finish();
    }

    /// Writes an AuthenticationSASLFinal message.
    pub fn write_auth_sasl_final(&mut self, data: &str) {
        self.start(BackendTag::AUTH.0);
        // Auth type 12 = SASL Final
        let data_bytes = data.as_bytes();
        self.write_i32(4 + 4 + data_bytes.len() as i32);
        self.out.extend_from_slice(&12u32.to_be_bytes()); // SASL Final auth type
        self.out.extend_from_slice(data_bytes);
        self.finish();
    }

    pub fn write_backend_key_data(&mut self, pid: i32, secret: i32) {
        self.start(BackendTag::BACKEND_KEY.0);
        self.write_i32(12);
        self.out.extend_from_slice(&pid.to_be_bytes());
        self.out.extend_from_slice(&secret.to_be_bytes());
        self.finish();
    }

    pub fn write_parameter_status(&mut self, name: &str, value: &str) {
        self.start(BackendTag::PARAM_STATUS.0);
        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();
        let total: i32 = 4 + (name_bytes.len() as i32 + 1) + (value_bytes.len() as i32 + 1);
        self.write_i32(total);
        self.out.extend_from_slice(name_bytes);
        self.out.push(0);
        self.out.extend_from_slice(value_bytes);
        self.out.push(0);
        self.finish();
    }

    pub fn write_ready_for_query(&mut self, txn_status: message::TxnStatus) {
        self.start(BackendTag::READY_FOR_QUERY.0);
        self.write_i32(5);
        self.out.push(txn_status.as_byte());
        self.finish();
    }

    pub fn write_row_description(&mut self, cols: &[message::ColumnDesc]) {
        self.start(BackendTag::ROW_DESCRIPTION.0);
        self.write_i16(cols.len() as i16);
        for col in cols {
            self.write_cstring(col.name.as_bytes());
            self.write_i32(col.table_oid);
            self.write_i16(col.column_attr);
            self.write_i32(col.type_oid);
            self.write_i16(col.type_size);
            self.write_i32(col.type_mod);
            self.write_i16(col.format);
        }
        self.finish();
    }

    pub fn write_data_row(&mut self, cols: &[message::ColumnDesc], row: &[Value]) {
        self.start(BackendTag::DATA_ROW.0);
        self.write_i16(cols.len() as i16);
        for (col, val) in cols.iter().zip(row.iter()) {
            let bytes = encode_value(val, col.format == 1);
            self.write_i32(bytes.len() as i32);
            self.out.extend_from_slice(&bytes);
        }
        self.finish();
    }

    pub fn write_command_complete(&mut self, tag: &str, rows: u64) {
        self.start(BackendTag::COMMAND_COMPLETE.0);
        let payload = if rows == 0 {
            alloc::format!("{tag} 0")
        } else {
            alloc::format!("{tag} {rows}")
        };
        let bytes = payload.as_bytes();
        let total: i32 = 4 + (bytes.len() as i32 + 1);
        self.out.extend_from_slice(&total.to_be_bytes());
        self.out.extend_from_slice(bytes);
        self.out.push(0);
    }

    pub fn write_error_response(&mut self, message: &str, sqlstate: Option<[u8; 5]>) {
        self.start(BackendTag::ERROR_RESPONSE.0);
        write_field(&mut self.out, b'M', message.as_bytes());
        write_field(&mut self.out, b'S', b"ERROR");
        if let Some(ss) = sqlstate {
            write_field(&mut self.out, b'C', &ss);
        }
        self.out.push(0);
        self.finish();
    }

    pub fn write_parse_complete(&mut self) {
        self.start(BackendTag::PARSE_COMPLETE.0);
        self.write_i32(4);
        self.finish();
    }

    pub fn write_bind_complete(&mut self) {
        self.start(BackendTag::BIND_COMPLETE.0);
        self.write_i32(4);
        self.finish();
    }

    pub fn write_close_complete(&mut self) {
        self.start(BackendTag::CLOSE_COMPLETE.0);
        self.write_i32(4);
        self.finish();
    }

    pub fn write_no_data(&mut self) {
        self.start(BackendTag::NO_DATA.0);
        self.write_i32(4);
        self.finish();
    }

    pub fn write_portal_suspended(&mut self) {
        self.start(BackendTag::PORTAL_SUSPEND.0);
        self.write_i32(4);
        self.finish();
    }

    fn start(&mut self, tag: u8) {
        self.out.push(tag);
        self.out.extend_from_slice(&[0, 0, 0, 0]);
    }

    fn finish(&mut self) {
        let len = self.out.len() as i32;
        let len_bytes = len.to_be_bytes();
        let pos = self.out.len() - 4;
        self.out[pos..].copy_from_slice(&len_bytes);
    }

    fn write_i32(&mut self, v: i32) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn write_i16(&mut self, v: i16) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn write_cstring(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
        self.out.push(0);
    }
}

fn write_field(out: &mut Vec<u8>, typ: u8, value: &[u8]) {
    out.push(typ);
    out.extend_from_slice(&(value.len() as i32).to_be_bytes());
    out.extend_from_slice(value);
}

fn encode_value(val: &Value, binary: bool) -> Vec<u8> {
    if binary {
        return match val {
            Value::Int(i) => i.to_be_bytes().to_vec(),
            Value::Float(f) => f.to_be_bytes().to_vec(),
            Value::Text(t) => t.as_bytes().to_vec(),
            Value::Vector(v) => {
                let mut b = Vec::with_capacity(v.len() * 4 + 4);
                b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                for f in v {
                    b.extend_from_slice(&f.to_be_bytes());
                }
                b
            }
            // Postgres's binary bool format is a single 0x00/0x01 byte.
            Value::Bool(b) => vec![*b as u8],
            Value::Null => Vec::new(),
        };
    }
    match val {
        Value::Int(i) => alloc::format!("{i}").into_bytes(),
        Value::Float(f) => alloc::format!("{f}").into_bytes(),
        Value::Text(t) => t.clone().into_bytes(),
        Value::Vector(v) => {
            let mut s = String::from("[");
            for (i, f) in v.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&alloc::format!("{f}"));
            }
            s.push(']');
            s.into_bytes()
        }
        // Postgres's text bool format is "t"/"f", not "true"/"false".
        Value::Bool(b) => (if *b { "t" } else { "f" }).as_bytes().to_vec(),
        Value::Null => Vec::new(),
    }
}

/// Reads a single message from the buffer.
/// Returns `Ok(Some((msg_type, payload)))` if a complete message is available,
/// `Ok(None)` if more data is needed, or `Err` on error.
pub fn read_message(buf: &mut BytesMut) -> CodecResult<Option<(u8, Vec<u8>)>> {
    if buf.len() < 5 {
        return Ok(None);
    }

    let msg_type = buf[0];
    let len = ((buf[1] as usize) << 24)
        | ((buf[2] as usize) << 16)
        | ((buf[3] as usize) << 8)
        | (buf[4] as usize);

    if len < 4 {
        return Err(CodecError::InvalidLength(len));
    }

    let total_len = 1 + len; // type byte + length (including itself)
    if buf.len() < total_len {
        return Ok(None);
    }

    let payload = buf[5..total_len].to_vec();
    buf.advance(total_len);

    Ok(Some((msg_type, payload)))
}

/// Writes a message to the buffer.
pub fn write_message(buf: &mut BytesMut, msg_type: u8, payload: &[u8]) {
    let len = 4 + payload.len(); // length includes itself (4 bytes) + payload
    buf.put_u8(msg_type);
    buf.put_u32(len as u32);
    buf.put_slice(payload);
}

/// Writes a C-string (null-terminated) to the buffer.
pub fn write_cstring(buf: &mut BytesMut, s: &str) {
    buf.put_slice(s.as_bytes());
    buf.put_u8(0);
}
