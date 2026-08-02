//! Binary entry point for the Archon PostgreSQL wire protocol server.
//!
//! Usage: cargo run --bin archon-pgwire [-- --host HOST --port PORT]

use std::sync::{Arc, Mutex};

use out_archon_pgwire::serve;
use tpt_archon_relational::database::Database;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "5432".to_string());
    let addr = format!("{}:{}", host, port);

    let db = Arc::new(Mutex::new(Database::empty()));

    println!(
        "Starting Archon PostgreSQL wire protocol server on {}",
        addr
    );

    serve(&addr, db)?;

    Ok(())
}
