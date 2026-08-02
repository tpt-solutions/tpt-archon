//! Connection handling for PostgreSQL wire protocol.
//!
//! Manages individual client connections, handles the startup sequence,
//! authentication, and message processing loop.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use bytes::{BufMut, BytesMut};
use tracing::{debug, error, info, warn};

use crate::codec::{
    read_message, write_message, write_cstring, PROTOCOL_VERSION, TransactionStatus,
};
use crate::{build_error_response, PgWireError};
use crate::session::Session;
use crate::Result;

/// Handle a single client connection
pub fn handle_connection(
    mut stream: TcpStream,
    database: Arc<Mutex<Session>>,
) -> Result<()> {
    let peer_addr = stream.peer_addr()?;
    info!("New connection from {}", peer_addr);
    
    // Read startup message or SSL request
    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf)?;
    
    // Check if it's an SSL request (protocol version 0x00000000 with special value)
    let msg_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let protocol_version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    
    if protocol_version == 80877103 { // SSL request code
        info!("SSL request from {}", peer_addr);
        // We don't support SSL - send 'N' (refuse)
        stream.write_all(b"N")?;
        stream.flush()?;
        
        // Read the actual startup message after SSL refusal
        stream.read_exact(&mut buf)?;
        let msg_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let protocol_version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        
        if protocol_version != PROTOCOL_VERSION {
            return Err(PgWireError::Protocol(format!(
                "Unsupported protocol version: {:08x}",
                protocol_version
            )));
        }
        
        handle_startup(&mut stream, msg_len, &database)?;
    } else if protocol_version == PROTOCOL_VERSION {
        handle_startup(&mut stream, msg_len, &database)?;
    } else if protocol_version == 0x00000000 {
        // Cancel request - not implemented
        return Err(PgWireError::Protocol("Cancel requests not supported".to_string()));
    } else {
        return Err(PgWireError::Protocol(format!(
            "Unsupported protocol version: {:08x}",
            protocol_version
        )));
    }
    
    // Authentication (trust for now)
    send_authentication_ok(&mut stream)?;
    
    // Send parameter status messages
    send_parameter_status(&mut stream)?;
    
    // Send backend key data
    send_backend_key_data(&mut stream)?;
    
    // Send ReadyForQuery (idle)
    send_ready_for_query(&mut stream, TransactionStatus::Idle)?;
    
    // Main message loop
    let mut read_buf = BytesMut::with_capacity(8192);
    loop {
        // Read more data if needed
        if read_buf.len() < 5 {
            let mut temp = vec![0u8; 8192];
            match stream.read(&mut temp) {
                Ok(0) => {
                    debug!("Connection closed by client {}", peer_addr);
                    break;
                }
                Ok(n) => {
                    read_buf.put_slice(&temp[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        
        // Try to parse a message
        match read_message(&mut read_buf) {
            Ok(Some((msg_type, payload))) => {
                if let Err(e) = process_message(
                    &mut stream,
                    msg_type,
                    &payload,
                    &database,
                ) {
                    error!("Error processing message: {}", e);
                    // Send error response
                    let err_bytes = build_error_response(&e);
                    stream.write_all(&err_bytes)?;
                    stream.flush()?;
                    
                    // If we're in a transaction, mark it as failed
                    if let Ok(mut session) = database.lock() {
                        session.mark_transaction_failed();
                    }
                    
                    // Send ReadyForQuery with appropriate status
                    let status = if let Ok(session) = database.lock() {
                        session.transaction_state.to_transaction_status()
                    } else {
                        TransactionStatus::Idle
                    };
                    send_ready_for_query(&mut stream, status)?;
                }
            }
            Ok(None) => {
                // Need more data
                continue;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }
    
    Ok(())
}

fn handle_startup(
    stream: &mut TcpStream,
    msg_len: usize,
    database: &Arc<Mutex<Session>>,
) -> Result<()> {
    // Read the rest of the startup message
    let mut payload = vec![0u8; msg_len - 8];
    stream.read_exact(&mut payload)?;
    
    // Parse startup message parameters
    let mut cursor = std::io::Cursor::new(&payload);
    let mut params = Vec::new();
    
    loop {
        let key = read_cstring_vec(&mut cursor)?;
        if key.is_empty() {
            break;
        }
        let value = read_cstring_vec(&mut cursor)?;
        params.push((key, value));
    }
    
    // Store relevant parameters in session
    if let Ok(mut session) = database.lock() {
        for (key, value) in params {
            match key.as_str() {
                "user" => {
                    session.set_parameter("user".to_string(), value);
                }
                "database" => {
                    session.set_parameter("database".to_string(), value);
                }
                "application_name" => {
                    session.set_parameter("application_name".to_string(), value);
                }
                _ => {
                    // Store other parameters too
                    session.set_parameter(key, value);
                }
            }
        }
    }
    
    Ok(())
}

fn send_authentication_ok(stream: &mut TcpStream) -> Result<()> {
    let mut buf = BytesMut::new();
    // AuthenticationOk message: type 'R', length 8 (4 bytes len + 4 bytes auth type 0)
    write_message(&mut buf, b'R', &[0, 0, 0, 0]);
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn send_parameter_status(stream: &mut TcpStream) -> Result<()> {
    let params = [
        ("server_version", "16.0"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("application_name", ""),
        ("DateStyle", "ISO, MDY"),
        ("TimeZone", "UTC"),
        ("standard_conforming_strings", "on"),
        ("search_path", "\"$user\", public"),
        ("default_transaction_isolation", "read committed"),
        ("transaction_isolation", "read committed"),
    ];
    
    for (name, value) in params {
        let mut buf = BytesMut::new();
        let mut payload = BytesMut::new();
        write_cstring(&mut payload, name);
        write_cstring(&mut payload, value);
        write_message(&mut buf, b'S', &payload);
        stream.write_all(&buf)?;
    }
    stream.flush()?;
    Ok(())
}

fn send_backend_key_data(stream: &mut TcpStream) -> Result<()> {
    let mut buf = BytesMut::new();
    let mut payload = BytesMut::new();
    // Process ID (4 bytes) - use a fixed value for now
    payload.put_u32(12345);
    // Secret key (4 bytes) - use a fixed value for now
    payload.put_u32(54321);
    write_message(&mut buf, b'K', &payload);
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn send_ready_for_query(stream: &mut TcpStream, status: TransactionStatus) -> Result<()> {
    let mut buf = BytesMut::new();
    let mut payload = BytesMut::new();
    payload.put_u8(status as u8);
    write_message(&mut buf, b'Z', &payload);
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn process_message(
    stream: &mut TcpStream,
    msg_type: u8,
    payload: &[u8],
    database: &Arc<Mutex<Session>>,
) -> Result<()> {
    match msg_type {
        b'Q' => {
            // Simple query
            let payload_vec = payload.to_vec();
            let query = String::from_utf8(payload_vec[..payload_vec.len()-1].to_vec())?; // Remove null terminator
            debug!("Simple query: {}", query);
            handle_simple_query(stream, &query, database)?;
        }
        b'X' => {
            // Terminate
            debug!("Client requested termination");
            return Err(PgWireError::Protocol("Terminate".to_string()));
        }
        b'P' => {
            // Parse (extended query)
            handle_parse(stream, payload, database)?;
        }
        b'B' => {
            // Bind (extended query)
            handle_bind(stream, payload, database)?;
        }
        b'E' => {
            // Execute (extended query)
            handle_execute(stream, payload, database)?;
        }
        b'S' => {
            // Sync (extended query)
            handle_sync(stream, database)?;
        }
        b'H' => {
            // Flush
            stream.flush()?;
        }
        b'C' => {
            // Close
            handle_close(stream, payload, database)?;
        }
        b'D' => {
            // Describe
            handle_describe(stream, payload, database)?;
        }
        _ => {
            warn!("Unhandled message type: {}", msg_type as char);
        }
    }
    Ok(())
}

fn handle_simple_query(
    stream: &mut TcpStream,
    query: &str,
    database: &Arc<Mutex<Session>>,
) -> Result<()> {
    use crate::compat::process_sql;
    use tpt_archon_relational::parser::Statement;
    
    let stmts = process_sql(query)?;
    
    let mut last_result = tpt_archon_relational::executor::ResultSet::default();
    let mut last_tag = tpt_archon_relational::executor::CommandTag::Empty;
    
    for stmt in stmts {
        let (result, tag) = {
            let mut session = database.lock().map_err(|_| PgWireError::Protocol("Lock poisoned".to_string()))?;
            session.database.execute_with_stats(&stmt, &[])?
        };
        last_result = result;
        last_tag = tag;
    }
    
    // Send results
    match last_tag {
        tpt_archon_relational::executor::CommandTag::Select(_) => {
            send_query_results(stream, &last_result)?;
        }
        tpt_archon_relational::executor::CommandTag::Insert(n) => {
            send_command_complete(stream, &format!("INSERT 0 {}", n))?;
        }
        tpt_archon_relational::executor::CommandTag::Update(n) => {
            send_command_complete(stream, &format!("UPDATE {}", n))?;
        }
        tpt_archon_relational::executor::CommandTag::Delete(n) => {
            send_command_complete(stream, &format!("DELETE {}", n))?;
        }
        tpt_archon_relational::executor::CommandTag::CreateTable => {
            send_command_complete(stream, "CREATE TABLE")?;
        }
        tpt_archon_relational::executor::CommandTag::CreateView => {
            send_command_complete(stream, "CREATE VIEW")?;
        }
        tpt_archon_relational::executor::CommandTag::DropView => {
            send_command_complete(stream, "DROP VIEW")?;
        }
        tpt_archon_relational::executor::CommandTag::AlterTable => {
            send_command_complete(stream, "ALTER TABLE")?;
        }
        tpt_archon_relational::executor::CommandTag::Begin => {
            send_command_complete(stream, "BEGIN")?;
        }
        tpt_archon_relational::executor::CommandTag::Commit => {
            send_command_complete(stream, "COMMIT")?;
        }
        tpt_archon_relational::executor::CommandTag::Rollback => {
            send_command_complete(stream, "ROLLBACK")?;
        }
        tpt_archon_relational::executor::CommandTag::Set => {
            send_command_complete(stream, "SET")?;
        }
        tpt_archon_relational::executor::CommandTag::Reset => {
            send_command_complete(stream, "RESET")?;
        }
        tpt_archon_relational::executor::CommandTag::Empty => {
            send_empty_query_response(stream)?;
        }
    }
    
    Ok(())
}

fn send_query_results(stream: &mut TcpStream, result: &tpt_archon_relational::executor::ResultSet) -> Result<()> {
    use crate::codec::{write_message, write_cstring, write_lstring};
    use bytes::BytesMut;
    
    // RowDescription
    let mut buf = BytesMut::new();
    let mut payload = BytesMut::new();
    payload.put_i16(result.columns.len() as i16);
    
    for col_name in &result.columns {
        write_cstring(&mut payload, col_name);
        // Table OID (0 = none)
        payload.put_i32(0);
        // Column attribute number (0 = none)
        payload.put_i16(0);
        // Type OID (25 = TEXT for now)
        payload.put_i32(25);
        // Type size (-1 = variable)
        payload.put_i16(-1);
        // Type modifier (-1 = none)
        payload.put_i32(-1);
        // Format (0 = text, 1 = binary)
        payload.put_i16(0);
    }
    write_message(&mut buf, b'T', &payload);
    stream.write_all(&buf)?;
    
    // DataRow messages
    for row in &result.rows {
        let mut buf = BytesMut::new();
        let mut payload = BytesMut::new();
        payload.put_i16(row.len() as i16);
        
        for val in row {
            match val {
                tpt_archon_relational::executor::Value::Null => {
                    // NULL value: length -1, no data
                    payload.put_i32(-1);
                }
                tpt_archon_relational::executor::Value::Int(i) => {
                    let s = i.to_string();
                    payload.put_i32(s.len() as i32);
                    payload.put_slice(s.as_bytes());
                }
                tpt_archon_relational::executor::Value::Text(s) => {
                    payload.put_i32(s.len() as i32);
                    payload.put_slice(s.as_bytes());
                }
                tpt_archon_relational::executor::Value::Float(f) => {
                    let s = f.to_string();
                    payload.put_i32(s.len() as i32);
                    payload.put_slice(s.as_bytes());
                }
                tpt_archon_relational::executor::Value::Vector(v) => {
                    let s = format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
                    payload.put_i32(s.len() as i32);
                    payload.put_slice(s.as_bytes());
                }
            }
        }
        write_message(&mut buf, b'D', &payload);
        stream.write_all(&buf)?;
    }
    
    // CommandComplete
    send_command_complete(stream, &format!("SELECT {}", result.rows.len()))?;
    
    stream.flush()?;
    Ok(())
}

fn send_command_complete(stream: &mut TcpStream, tag: &str) -> Result<()> {
    let mut buf = BytesMut::new();
    let mut payload = BytesMut::new();
    write_cstring(&mut payload, tag);
    write_message(&mut buf, b'C', &payload);
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn send_empty_query_response(stream: &mut TcpStream) -> Result<()> {
    let mut buf = BytesMut::new();
    write_message(&mut buf, b'I', &[]);
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

// Extended query protocol handlers (simplified - zero-parameter only)
fn handle_parse(
    stream: &mut TcpStream,
    _payload: &[u8],
    _database: &Arc<Mutex<Session>>,
) -> Result<()> {
    // Parse message: portal name (optional), statement name (optional), query string, parameter types
    // For now, just acknowledge
    let mut buf = BytesMut::new();
    write_message(&mut buf, b'1', &[]); // ParseComplete
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn handle_bind(
    stream: &mut TcpStream,
    _payload: &[u8],
    _database: &Arc<Mutex<Session>>,
) -> Result<()> {
    let mut buf = BytesMut::new();
    write_message(&mut buf, b'2', &[]); // BindComplete
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn handle_execute(
    stream: &mut TcpStream,
    _payload: &[u8],
    _database: &Arc<Mutex<Session>>,
) -> Result<()> {
    // For now, we don't have prepared statement storage
    // Send empty result
    send_empty_query_response(stream)?;
    Ok(())
}

fn handle_sync(
    stream: &mut TcpStream,
    _database: &Arc<Mutex<Session>>,
) -> Result<()> {
    let mut buf = BytesMut::new();
    write_message(&mut buf, b'S', &[]); // Sync is frontend only, we send ReadyForQuery
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

fn handle_close(
    _stream: &mut TcpStream,
    _payload: &[u8],
    _database: &Arc<Mutex<Session>>,
) -> Result<()> {
    // CloseComplete
    Ok(())
}

fn handle_describe(
    _stream: &mut TcpStream,
    _payload: &[u8],
    _database: &Arc<Mutex<Session>>,
) -> Result<()> {
    // NoData or ParameterDescription/RowDescription
    Ok(())
}

fn read_cstring(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<String> {
    use bytes::Buf;
    let mut bytes = Vec::new();
    let slice = cursor.get_ref();
    let mut pos = cursor.position() as usize;
    while pos < slice.len() {
        let b = slice[pos];
        pos += 1;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    cursor.set_position(pos as u64);
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_cstring_vec(cursor: &mut std::io::Cursor<&Vec<u8>>) -> io::Result<String> {
    use bytes::Buf;
    let mut bytes = Vec::new();
    let slice = cursor.get_ref();
    let mut pos = cursor.position() as usize;
    while pos < slice.len() {
        let b = slice[pos];
        pos += 1;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    cursor.set_position(pos as u64);
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
