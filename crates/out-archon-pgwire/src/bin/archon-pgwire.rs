//! Binary entry point for the Archon PostgreSQL wire protocol server.
//!
//! Usage:
//!   cargo run --bin archon-pgwire [-- --host HOST --port PORT]
//!   cargo run --bin archon-pgwire --features tls -- \
//!       --host HOST --port PORT --tls-cert cert.pem --tls-key key.pem

use std::sync::{Arc, Mutex};

use out_archon_pgwire::serve;
use tpt_archon_relational::database::Database;

#[cfg(feature = "tls")]
use out_archon_pgwire::serve_tls;
#[cfg(feature = "tls")]
use out_archon_pgwire::tls::TlsConfig;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut host = "127.0.0.1".to_string();
    let mut port = "5432".to_string();
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                if i < args.len() {
                    host = args[i].clone();
                }
            }
            "--port" => {
                i += 1;
                if i < args.len() {
                    port = args[i].clone();
                }
            }
            "--tls-cert" => {
                i += 1;
                if i < args.len() {
                    tls_cert = Some(args[i].clone());
                }
            }
            "--tls-key" => {
                i += 1;
                if i < args.len() {
                    tls_key = Some(args[i].clone());
                }
            }
            other => {
                eprintln!("ignoring unknown argument: {}", other);
            }
        }
        i += 1;
    }

    let addr = format!("{}:{}", host, port);
    let db = Arc::new(Mutex::new(Database::empty()));

    println!(
        "Starting Archon PostgreSQL wire protocol server on {}",
        addr
    );

    #[cfg(feature = "tls")]
    {
        if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
            let cfg =
                TlsConfig::from_pem_files(std::path::Path::new(&cert), std::path::Path::new(&key))?;
            let server_config = std::sync::Arc::new(cfg.into_server_config()?);
            println!("TLS enabled (cert={}, key={})", cert, key);
            return serve_tls(&addr, db, server_config).map_err(Into::into);
        }
    }

    #[cfg(not(feature = "tls"))]
    if tls_cert.is_some() || tls_key.is_some() {
        eprintln!(
            "warning: TLS flags ignored; rebuild with --features tls to enable encrypted transport"
        );
    }

    serve(&addr, db)?;

    Ok(())
}
