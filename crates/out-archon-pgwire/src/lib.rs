//! `out-archon-pgwire` — PostgreSQL wire-protocol server for `tpt-archon`.
//!
//! Non-published workspace member depending only on `tpt-archon-relational`.
//! Implements a thread-per-connection `TcpListener` server with the simple
//! query protocol (`Q`) plus stubs for the extended query protocol (`Parse`/
//! `Bind`/`Execute`/`Describe`/`Sync`).
//!
//! ## Design note (ADR 0004)
//!
//! Concurrency is capped at a mutex regardless because `Database` is not
//! `Sync + Send` in the current single-threaded arena model. Adding `tokio` or
//! another async runtime buys nothing for connection scaling until the
//! storage layer becomes `Sync + Send`. This should be revisited on measured
//! evidence, not taste.

extern crate alloc;

pub mod codec;
pub mod compat;
pub mod error;
pub mod extended;
pub mod session;
pub mod simple_query;
pub mod sqlstate;
pub mod startup;
pub mod task;

#[cfg(feature = "std")]
use std::io::{Read, Write};
#[cfg(feature = "std")]
use std::net::{Shutdown, TcpListener, TcpStream};
#[cfg(feature = "std")]
use std::sync::{Arc, Mutex};

use tpt_archon_relational::database::Database;

#[cfg(feature = "std")]
use crate::codec::MessageReader;
#[cfg(feature = "std")]
use crate::session::Session;

/// Spawns a thread-per-connection PostgreSQL wire-protocol server.
///
/// Listens on `addr` and accepts connections until the listener is closed.
/// Each connection is handled in its own OS thread sharing a `Database` via
/// `Arc<Mutex<_>>`. The server exits when the listener is closed (e.g. Ctrl-C).
#[cfg(feature = "std")]
pub fn serve(addr: &str, db: Arc<Mutex<Database>>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(false)?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let db = Arc::clone(&db);
                std::thread::spawn(move || handle_connection(stream, db));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(feature = "std")]
fn handle_connection(mut stream: TcpStream, db: Arc<Mutex<Database>>) {
    let mut reader = MessageReader::new();
    let mut session = Session::new();
    let mut buf = [0u8; 8192];

    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));

    let startup_resp = crate::startup::handle_startup(
        &crate::codec::message::StartupMessage {
            protocol_major: 3,
            protocol_minor: 0,
            params: Vec::new(),
        },
        &mut session,
    );
    let _ = stream.write_all(&startup_resp);

    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let bytes = &buf[..n];
        let msgs = match reader.feed(bytes) {
            Ok(msgs) => msgs,
            Err(_) => break,
        };
        if msgs.is_empty() {
            continue;
        }
        let mut response: Vec<u8> = Vec::new();
        for msg in msgs {
            match msg {
                crate::codec::message::FrontendMessage::Query(sql) => {
                    let mut db = db.lock().unwrap();
                    let frames =
                        crate::simple_query::handle_simple_query(&sql, &mut db, &mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Terminate => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return;
                }
                crate::codec::message::FrontendMessage::Parse => {
                    let frames = crate::extended::handle_parse(&mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Bind => {
                    let frames = crate::extended::handle_bind(&mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Describe => {
                    let frames = crate::extended::handle_describe(&mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Execute => {
                    let frames = crate::extended::handle_execute(&mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Sync => {
                    let frames = crate::extended::handle_sync(&mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Close => {
                    let frames = crate::extended::handle_close(&mut session);
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Password(password) => {
                    // Check if this is a SCRAM authentication message
                    // SCRAM messages start with the mechanism name followed by the client data
                    if password.starts_with("SCRAM-SHA-256") {
                        // Extract the mechanism and client data
                        let parts: Vec<&str> = password.splitn(2, ' ').collect();
                        if parts.len() == 2 {
                            let frames =
                                crate::startup::handle_scram_auth(parts[0], parts[1], &mut session);
                            response.extend_from_slice(&frames);
                        } else {
                            // Malformed SCRAM message, fall back to trust auth
                            let frames = crate::startup::handle_password("", &mut session);
                            response.extend_from_slice(&frames);
                        }
                    } else {
                        // Cleartext password or other mechanism - use trust auth for v1
                        let frames = crate::startup::handle_password(&password, &mut session);
                        response.extend_from_slice(&frames);
                    }
                }
                crate::codec::message::FrontendMessage::SslRequest => {
                    response.extend_from_slice(b"N");
                }
                crate::codec::message::FrontendMessage::Startup(_) => {
                    let mut w = crate::codec::MessageWriter::new();
                    w.write_error_response(
                        "unexpected startup in middle of connection",
                        Some(*b"58000"),
                    );
                    response.extend_from_slice(w.bytes());
                }
            }
        }
        if !response.is_empty() {
            let _ = stream.write_all(&response);
        }
    }
}

#[cfg(not(feature = "std"))]
pub fn serve(_addr: &str, _db: Arc<Mutex<Database>>) {}

/// Spawns a scheduler-based PostgreSQL wire-protocol server.
///
/// This uses the `tpt_archon_kernel::scheduler::Scheduler` to run each
/// connection as a cooperative task, making the "one Task per connection"
/// claim in spec.txt literally true.
///
/// The server runs in a single thread, polling the scheduler in a loop.
/// Each connection is a `PgConnectionTask` that yields control back to the
/// scheduler when waiting for I/O.
#[cfg(feature = "std")]
pub fn serve_scheduled(addr: &str, db: Arc<Mutex<Database>>) -> std::io::Result<()> {
    use tpt_archon_kernel::scheduler::Scheduler;

    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;

    let mut scheduler = Scheduler::new();

    loop {
        // Accept new connections
        match listener.accept() {
            Ok((stream, _addr)) => {
                let session = Arc::new(Mutex::new(Session::with_database(Arc::clone(&db))));
                match crate::task::PgConnectionTask::new(stream, session) {
                    Ok(task) => {
                        scheduler.spawn(Box::new(task));
                    }
                    Err(e) => {
                        eprintln!("Failed to create connection task: {}", e);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No new connections, continue to scheduler tick
            }
            Err(e) => return Err(e),
        }

        // Run one scheduler tick
        scheduler.tick();

        // Small sleep to prevent busy-waiting when no tasks are ready
        if scheduler.task_count() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

#[cfg(not(feature = "std"))]
pub fn serve_scheduled(_addr: &str, _db: Arc<Mutex<Database>>) {}
