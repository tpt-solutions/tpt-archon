//! Task implementation for PostgreSQL wire protocol connections.
//!
//! This implements the `tpt_archon_kernel::scheduler::Task` trait so that
//! each PostgreSQL connection can be run as a scheduled task, making the
//! "one Task per connection" claim in spec.txt literally true.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use tpt_archon_kernel::scheduler::{Poll, Task};

use crate::codec::{
    read_message, write_cstring, write_message, TransactionStatus, PROTOCOL_VERSION,
};
use crate::error::{build_error_response, PgWireError, Result};
use crate::session::Session;

/// A task that handles a single PostgreSQL connection.
///
/// This wraps the connection handling logic in a way that can be
/// cooperatively scheduled by the kernel's scheduler.
pub struct PgConnectionTask {
    stream: Option<TcpStream>,
    database: Arc<Mutex<Session>>,
    #[allow(dead_code)]
    peer_addr: std::net::SocketAddr,
    state: ConnectionState,
    read_buf: BytesMut,
    write_buf: BytesMut,
    startup_complete: bool,
    auth_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    /// Waiting for startup message
    Startup,
    /// Processing authentication
    Authenticating,
    /// Ready for queries
    Ready,
    /// Processing a message
    Processing,
    /// Connection closed
    Closed,
    /// Error state
    Error,
}

impl PgConnectionTask {
    /// Creates a new connection task from an accepted TCP stream.
    pub fn new(stream: TcpStream, database: Arc<Mutex<Session>>) -> Result<Self> {
        let peer_addr = stream
            .peer_addr()
            .unwrap_or(std::net::SocketAddr::from(([0u8; 4], 0)));
        stream.set_nonblocking(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;

        Ok(Self {
            stream: Some(stream),
            database,
            peer_addr,
            state: ConnectionState::Startup,
            read_buf: BytesMut::with_capacity(8192),
            write_buf: BytesMut::with_capacity(8192),
            startup_complete: false,
            auth_complete: false,
        })
    }

    /// Processes the startup message and authentication.
    fn process_startup(&mut self) -> Result<()> {
        // We need to take the stream out temporarily to avoid borrow issues
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Stream not available"))?;

        // Read startup message or SSL request
        let mut buf = [0u8; 8];
        match stream.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.stream = Some(stream);
                return Ok(());
            }
            Err(e) => {
                self.stream = Some(stream);
                return Err(e.into());
            }
        }

        let msg_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let protocol_version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        if protocol_version == 80877103 {
            // SSL request code
            // We don't support SSL - send 'N' (refuse)
            stream.write_all(b"N")?;
            stream.flush()?;

            // Read the actual startup message after SSL refusal
            stream.read_exact(&mut buf)?;
            let msg_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            let protocol_version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

            if protocol_version != PROTOCOL_VERSION {
                self.stream = Some(stream);
                return Err(PgWireError::Protocol(format!(
                    "Unsupported protocol version: {:08x}",
                    protocol_version
                )));
            }

            self.handle_startup(&mut stream, msg_len)?;
        } else if protocol_version == PROTOCOL_VERSION {
            self.handle_startup(&mut stream, msg_len)?;
        } else if protocol_version == 0x00000000 {
            // Cancel request - not implemented
            self.stream = Some(stream);
            return Err(PgWireError::Protocol(
                "Cancel requests not supported".to_string(),
            ));
        } else {
            self.stream = Some(stream);
            return Err(PgWireError::Protocol(format!(
                "Unsupported protocol version: {:08x}",
                protocol_version
            )));
        }

        self.stream = Some(stream);
        self.startup_complete = true;
        self.state = ConnectionState::Authenticating;
        Ok(())
    }

    fn handle_startup(&mut self, stream: &mut TcpStream, msg_len: usize) -> Result<()> {
        // Read the rest of the startup message
        let mut payload = vec![0u8; msg_len - 8];
        stream.read_exact(&mut payload)?;

        // Parse startup message parameters
        let payload_slice: &[u8] = &payload;
        let mut cursor = std::io::Cursor::new(payload_slice);
        let mut params = Vec::new();

        loop {
            let key = read_cstring(&mut cursor)?;
            if key.is_empty() {
                break;
            }
            let value = read_cstring(&mut cursor)?;
            params.push((key, value));
        }

        // Store relevant parameters in session
        if let Ok(mut session) = self.database.lock() {
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
                        session.set_parameter(key, value);
                    }
                }
            }
        }

        Ok(())
    }

    /// Sends authentication OK and parameter status messages.
    fn send_auth_and_params(&mut self) -> Result<()> {
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Stream not available"))?;

        // Send AuthenticationOk
        self.write_buf.clear();
        write_message(&mut self.write_buf, b'R', &[0, 0, 0, 0]);
        stream.write_all(&self.write_buf)?;

        // Send parameter status messages
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
            self.write_buf.clear();
            let mut payload = BytesMut::new();
            write_cstring(&mut payload, name);
            write_cstring(&mut payload, value);
            write_message(&mut self.write_buf, b'S', &payload);
            stream.write_all(&self.write_buf)?;
        }

        // Send backend key data
        self.write_buf.clear();
        let mut payload = BytesMut::new();
        payload.put_u32(12345); // Process ID
        payload.put_u32(54321); // Secret key
        write_message(&mut self.write_buf, b'K', &payload);
        stream.write_all(&self.write_buf)?;

        // Send ReadyForQuery (idle)
        self.send_ready_for_query(&mut stream, TransactionStatus::Idle)?;

        stream.flush()?;
        self.auth_complete = true;
        self.state = ConnectionState::Ready;
        self.stream = Some(stream);
        Ok(())
    }

    /// Main message processing loop - processes one message per poll.
    fn process_one_message(&mut self) -> Result<bool> {
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Stream not available"))?;

        // Read more data if needed
        if self.read_buf.len() < 5 {
            let mut temp = vec![0u8; 8192];
            match stream.read(&mut temp) {
                Ok(0) => {
                    // Connection closed by client
                    self.state = ConnectionState::Closed;
                    self.stream = Some(stream);
                    return Ok(false);
                }
                Ok(n) => {
                    self.read_buf.put_slice(&temp[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.stream = Some(stream);
                    return Ok(true); // Need more data, but connection still alive
                }
                Err(e) => {
                    self.stream = Some(stream);
                    return Err(e.into());
                }
            }
        }

        // Try to parse a message
        match read_message(&mut self.read_buf) {
            Ok(Some((msg_type, payload))) => {
                self.state = ConnectionState::Processing;
                let result = self.process_message(&mut stream, msg_type, &payload);
                if let Err(e) = result {
                    // Send error response
                    let err_bytes = build_error_response(&e);
                    stream.write_all(&err_bytes)?;

                    // If we're in a transaction, mark it as failed
                    if let Ok(mut session) = self.database.lock() {
                        session.mark_transaction_failed();
                    }

                    // Send ReadyForQuery with appropriate status
                    let status = if let Ok(session) = self.database.lock() {
                        session.txn_status.to_transaction_status()
                    } else {
                        TransactionStatus::Idle
                    };
                    self.send_ready_for_query(&mut stream, status)?;
                }
                self.state = ConnectionState::Ready;
                self.stream = Some(stream);
                Ok(true)
            }
            Ok(None) => {
                // Need more data
                self.stream = Some(stream);
                Ok(true)
            }
            Err(e) => {
                self.stream = Some(stream);
                Err(e.into())
            }
        }
    }

    fn process_message(
        &mut self,
        stream: &mut TcpStream,
        msg_type: u8,
        payload: &[u8],
    ) -> Result<()> {
        match msg_type {
            b'Q' => {
                // Simple query
                let payload_vec = payload.to_vec();
                let query = String::from_utf8(payload_vec[..payload_vec.len() - 1].to_vec())?; // Remove null terminator
                self.handle_simple_query(stream, &query)?;
            }
            b'X' => {
                // Terminate
                self.state = ConnectionState::Closed;
                return Err(PgWireError::Protocol("Terminate".to_string()));
            }
            b'P' => {
                // Parse (extended query)
                self.handle_parse(stream, payload)?;
            }
            b'B' => {
                // Bind (extended query)
                self.handle_bind(stream, payload)?;
            }
            b'E' => {
                // Execute (extended query)
                self.handle_execute(stream, payload)?;
            }
            b'S' => {
                // Sync (extended query)
                self.handle_sync(stream)?;
            }
            b'H' => {
                // Flush
                stream.flush()?;
            }
            b'C' => {
                // Close
                self.handle_close(stream, payload)?;
            }
            b'D' => {
                // Describe
                self.handle_describe(stream, payload)?;
            }
            b'p' => {
                // Password message (for SCRAM auth)
                let password = String::from_utf8(payload[..payload.len() - 1].to_vec())?;
                self.handle_password(stream, &password)?;
            }
            _ => {
                // Unhandled message type - ignore for now
            }
        }
        Ok(())
    }

    fn handle_simple_query(&mut self, stream: &mut TcpStream, query: &str) -> Result<()> {
        use crate::compat::process_sql;

        let stmts = process_sql(query)?;

        let mut last_result = tpt_archon_relational::executor::ResultSet::default();
        let mut last_tag = tpt_archon_relational::executor::CommandTag::Empty;

        for stmt in stmts {
            let (result, tag) = {
                let session_guard = self
                    .database
                    .lock()
                    .map_err(|_| PgWireError::Protocol("Lock poisoned".to_string()))?;
                let mut db_guard = session_guard
                    .database
                    .lock()
                    .map_err(|_| PgWireError::Protocol("Lock poisoned".to_string()))?;
                db_guard.execute_with_stats(&stmt, &[])?
            };
            last_result = result;
            last_tag = tag;
        }

        // Send results
        match last_tag {
            tpt_archon_relational::executor::CommandTag::Select(_) => {
                self.send_query_results(stream, &last_result)?;
            }
            tpt_archon_relational::executor::CommandTag::Insert(n) => {
                self.send_command_complete(stream, &format!("INSERT 0 {}", n))?;
            }
            tpt_archon_relational::executor::CommandTag::Update(n) => {
                self.send_command_complete(stream, &format!("UPDATE {}", n))?;
            }
            tpt_archon_relational::executor::CommandTag::Delete(n) => {
                self.send_command_complete(stream, &format!("DELETE {}", n))?;
            }
            tpt_archon_relational::executor::CommandTag::CreateTable => {
                self.send_command_complete(stream, "CREATE TABLE")?;
            }
            tpt_archon_relational::executor::CommandTag::CreateView => {
                self.send_command_complete(stream, "CREATE VIEW")?;
            }
            tpt_archon_relational::executor::CommandTag::DropView => {
                self.send_command_complete(stream, "DROP VIEW")?;
            }
            tpt_archon_relational::executor::CommandTag::AlterTable => {
                self.send_command_complete(stream, "ALTER TABLE")?;
            }
            tpt_archon_relational::executor::CommandTag::Begin => {
                self.send_command_complete(stream, "BEGIN")?;
            }
            tpt_archon_relational::executor::CommandTag::Commit => {
                self.send_command_complete(stream, "COMMIT")?;
            }
            tpt_archon_relational::executor::CommandTag::Rollback => {
                self.send_command_complete(stream, "ROLLBACK")?;
            }
            tpt_archon_relational::executor::CommandTag::Set => {
                self.send_command_complete(stream, "SET")?;
            }
            tpt_archon_relational::executor::CommandTag::Reset => {
                self.send_command_complete(stream, "RESET")?;
            }
            tpt_archon_relational::executor::CommandTag::Empty => {
                self.send_empty_query_response(stream)?;
            }
        }

        Ok(())
    }

    fn send_query_results(
        &mut self,
        stream: &mut TcpStream,
        result: &tpt_archon_relational::executor::ResultSet,
    ) -> Result<()> {
        use crate::codec::{write_cstring, write_message};

        // RowDescription
        self.write_buf.clear();
        let mut payload = BytesMut::new();
        payload.put_i16(result.columns.len() as i16);

        for col_name in &result.columns {
            write_cstring(&mut payload, col_name);
            payload.put_i32(0); // Table OID
            payload.put_i16(0); // Column attribute number
            payload.put_i32(25); // Type OID (TEXT)
            payload.put_i16(-1); // Type size
            payload.put_i32(-1); // Type modifier
            payload.put_i16(0); // Format (text)
        }
        write_message(&mut self.write_buf, b'T', &payload);
        stream.write_all(&self.write_buf)?;

        // DataRow messages
        for row in &result.rows {
            self.write_buf.clear();
            let mut payload = BytesMut::new();
            payload.put_i16(row.len() as i16);

            for val in row {
                match val {
                    tpt_archon_relational::executor::Value::Null => {
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
                        let s = format!(
                            "[{}]",
                            v.iter()
                                .map(|x| x.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        );
                        payload.put_i32(s.len() as i32);
                        payload.put_slice(s.as_bytes());
                    }
                    tpt_archon_relational::executor::Value::Bool(b) => {
                        // Postgres's text bool format is "t"/"f".
                        let s = if *b { "t" } else { "f" };
                        payload.put_i32(s.len() as i32);
                        payload.put_slice(s.as_bytes());
                    }
                }
            }
            write_message(&mut self.write_buf, b'D', &payload);
            stream.write_all(&self.write_buf)?;
        }

        // CommandComplete
        self.send_command_complete(stream, &format!("SELECT {}", result.rows.len()))?;

        stream.flush()?;
        Ok(())
    }

    fn send_command_complete(&mut self, stream: &mut TcpStream, tag: &str) -> Result<()> {
        self.write_buf.clear();
        let mut payload = BytesMut::new();
        write_cstring(&mut payload, tag);
        write_message(&mut self.write_buf, b'C', &payload);
        stream.write_all(&self.write_buf)?;
        stream.flush()?;
        Ok(())
    }

    fn send_empty_query_response(&mut self, stream: &mut TcpStream) -> Result<()> {
        self.write_buf.clear();
        write_message(&mut self.write_buf, b'I', &[]);
        stream.write_all(&self.write_buf)?;
        stream.flush()?;
        Ok(())
    }

    fn send_ready_for_query(
        &mut self,
        stream: &mut TcpStream,
        status: TransactionStatus,
    ) -> Result<()> {
        self.write_buf.clear();
        let mut payload = BytesMut::new();
        payload.put_u8(status as u8);
        write_message(&mut self.write_buf, b'Z', &payload);
        stream.write_all(&self.write_buf)?;
        stream.flush()?;
        Ok(())
    }

    // Extended query protocol handlers (simplified - zero-parameter only)
    fn handle_parse(&mut self, stream: &mut TcpStream, _payload: &[u8]) -> Result<()> {
        self.write_buf.clear();
        write_message(&mut self.write_buf, b'1', &[]); // ParseComplete
        stream.write_all(&self.write_buf)?;
        stream.flush()?;
        Ok(())
    }

    fn handle_bind(&mut self, stream: &mut TcpStream, _payload: &[u8]) -> Result<()> {
        self.write_buf.clear();
        write_message(&mut self.write_buf, b'2', &[]); // BindComplete
        stream.write_all(&self.write_buf)?;
        stream.flush()?;
        Ok(())
    }

    fn handle_execute(&mut self, stream: &mut TcpStream, _payload: &[u8]) -> Result<()> {
        self.send_empty_query_response(stream)?;
        Ok(())
    }

    fn handle_sync(&mut self, stream: &mut TcpStream) -> Result<()> {
        // Sync is frontend only, we send ReadyForQuery
        let status = if let Ok(session) = self.database.lock() {
            session.txn_status.to_transaction_status()
        } else {
            TransactionStatus::Idle
        };
        self.send_ready_for_query(stream, status)?;
        Ok(())
    }

    fn handle_close(&mut self, _stream: &mut TcpStream, _payload: &[u8]) -> Result<()> {
        // CloseComplete - just acknowledge
        Ok(())
    }

    fn handle_describe(&mut self, _stream: &mut TcpStream, _payload: &[u8]) -> Result<()> {
        // NoData or ParameterDescription/RowDescription
        Ok(())
    }

    fn handle_password(&mut self, stream: &mut TcpStream, password: &str) -> Result<()> {
        // Check if this is a SCRAM authentication message
        if password.starts_with("SCRAM-SHA-256") {
            let parts: Vec<&str> = password.splitn(2, ' ').collect();
            if parts.len() == 2 {
                let frames = crate::startup::handle_scram_auth(
                    parts[0],
                    parts[1],
                    &mut self.database.lock().unwrap(),
                );
                stream.write_all(&frames)?;
                stream.flush()?;
            } else {
                // Malformed SCRAM message, fall back to trust auth
                let frames =
                    crate::startup::handle_password("", &mut self.database.lock().unwrap());
                stream.write_all(&frames)?;
                stream.flush()?;
            }
        } else {
            // Cleartext password or other mechanism - use trust auth for v1
            let frames =
                crate::startup::handle_password(password, &mut self.database.lock().unwrap());
            stream.write_all(&frames)?;
            stream.flush()?;
        }
        Ok(())
    }
}

impl Task for PgConnectionTask {
    fn poll(&mut self) -> Poll {
        // If connection is closed or in error state, we're done
        if self.state == ConnectionState::Closed || self.state == ConnectionState::Error {
            return Poll::Ready;
        }

        // Process startup if not complete
        if !self.startup_complete {
            if let Err(_e) = self.process_startup() {
                self.state = ConnectionState::Error;
                return Poll::Ready;
            }
            // If startup is still in progress (WouldBlock), yield
            if !self.startup_complete {
                return Poll::Pending;
            }
        }

        // Process authentication if not complete
        if !self.auth_complete {
            if let Err(_e) = self.send_auth_and_params() {
                self.state = ConnectionState::Error;
                return Poll::Ready;
            }
            if !self.auth_complete {
                return Poll::Pending;
            }
        }

        // Process one message per poll (cooperative scheduling)
        match self.process_one_message() {
            Ok(true) => Poll::Pending, // Message processed, more may be available
            Ok(false) => Poll::Ready,  // Connection closed
            Err(_) => {
                self.state = ConnectionState::Error;
                Poll::Ready
            }
        }
    }
}

fn read_cstring(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<String> {
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
