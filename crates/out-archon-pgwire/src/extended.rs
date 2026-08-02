//! Extended query protocol stubs (B4): Parse/Bind/Execute/Describe/Sync.
//!
//! v1 only parses these tags and returns `ParseComplete`/`BindComplete`; typed
//! parameter binding is deferred because the relational crate has no parameter
//! API yet. A real implementation should not invent that API inside the wire
//! crate (see TODO.md Phase 8 Track B B4).

use crate::codec::MessageWriter;
use crate::session::Session;

pub fn handle_parse(_session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_parse_complete();
    out.extend_from_slice(w.bytes());
    out
}

pub fn handle_bind(_session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_bind_complete();
    out.extend_from_slice(w.bytes());
    out
}

pub fn handle_describe(_session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_no_data();
    out.extend_from_slice(w.bytes());
    out
}

pub fn handle_execute(_session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_portal_suspended();
    out.extend_from_slice(w.bytes());
    out
}

pub fn handle_sync(_session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_ready_for_query(crate::codec::message::TxnStatus::Idle);
    out.extend_from_slice(w.bytes());
    out
}

pub fn handle_close(_session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_close_complete();
    out.extend_from_slice(w.bytes());
    out
}
