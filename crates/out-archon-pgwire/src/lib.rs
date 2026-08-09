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

pub mod backup;
pub mod catalog;
pub mod codec;
pub mod compat;
pub mod error;
pub mod extended;
pub mod metrics;
pub mod session;
pub mod simple_query;
pub mod sqlstate;
pub mod startup;
pub mod task;
#[cfg(feature = "tls")]
pub mod tls;

#[cfg(all(feature = "std", feature = "tls"))]
use std::io::{Read, Write};
#[cfg(feature = "std")]
use std::net::{TcpListener, TcpStream};
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
    let metrics = Arc::new(crate::metrics::Metrics::new());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                metrics.record_connection_accepted();
                let db = Arc::clone(&db);
                let metrics = Arc::clone(&metrics);
                std::thread::spawn(move || handle_connection(stream, db, metrics));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Code sent by a PostgreSQL client in the pre-startup `SSLRequest` message.
#[cfg(all(feature = "std", feature = "tls"))]
const SSL_REQUEST_CODE: u32 = 80_877_103; // 0x04D2_162F

/// Spawns a thread-per-connection PostgreSQL wire-protocol server over TLS.
///
/// Listens on `addr`, performs PostgreSQL's pre-startup `SSLRequest`
/// negotiation (responds `S` and upgrades to TLS when the client asks, `N`
/// and serves plaintext otherwise), then runs the same protocol loop as
/// [`serve`]. Hand a [`TlsConfig`](crate::tls::TlsConfig) loaded from PEM
/// files (see the `tls` module). Requires the `tls` feature.
#[cfg(all(feature = "std", feature = "tls"))]
pub fn serve_tls(
    addr: &str,
    db: Arc<Mutex<Database>>,
    tls: Arc<rustls::ServerConfig>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(false)?;
    let metrics = Arc::new(crate::metrics::Metrics::new());
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        let db = Arc::clone(&db);
        let metrics = Arc::clone(&metrics);
        let tls = Arc::clone(&tls);
        std::thread::spawn(move || {
            metrics.record_connection_accepted();
            // Detect a pre-startup SSLRequest without consuming the stream so
            // the eventual plaintext path still sees the real startup bytes.
            let mut peek = [0u8; 8];
            let mut filled = 0;
            while filled < 8 {
                match stream.peek(&mut peek[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => return,
                }
            }
            let wants_ssl = filled == 8
                && u32::from_be_bytes(peek[0..4].try_into().unwrap()) == 8
                && u32::from_be_bytes(peek[4..8].try_into().unwrap()) == SSL_REQUEST_CODE;
            if wants_ssl {
                let mut _consumed = [0u8; 8];
                if stream.read_exact(&mut _consumed).is_err() {
                    return;
                }
                if stream.write_all(b"S").is_err() {
                    return;
                }
                match crate::tls::accept(stream, tls) {
                    Ok(tls_stream) => run_protocol(tls_stream, db, metrics),
                    Err(e) => eprintln!("TLS handshake failed: {}", e),
                }
            } else {
                if stream.write_all(b"N").is_err() {
                    return;
                }
                run_protocol(stream, db, metrics);
            }
        });
    }
    Ok(())
}

#[cfg(feature = "std")]
fn handle_connection(
    stream: TcpStream,
    db: Arc<Mutex<Database>>,
    metrics: Arc<crate::metrics::Metrics>,
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    run_protocol(stream, db, metrics);
}

#[cfg(feature = "std")]
fn run_protocol<S: std::io::Read + std::io::Write>(
    mut stream: S,
    db: Arc<Mutex<Database>>,
    metrics: Arc<crate::metrics::Metrics>,
) {
    let mut reader = MessageReader::new();
    let mut session = Session::new();
    session.metrics = metrics.clone();
    let mut buf = [0u8; 8192];

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
                    session.metrics.record_statement();
                    response.extend_from_slice(&frames);
                }
                crate::codec::message::FrontendMessage::Terminate => {
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
            metrics.record_bytes_sent(response.len() as u64);
        }
    }
    metrics.record_connection_closed();
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
