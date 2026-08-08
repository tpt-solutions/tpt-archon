//! PostgreSQL wire protocol server.
//!
//! Listens for incoming connections and spawns a thread per connection
//! to handle the PostgreSQL wire protocol.

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use tracing::{error, info, warn};

use crate::session::Session;
use crate::{handle_connection, PgWireError, Result};

/// Server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Host to bind to
    pub host: String,
    /// Port to listen on
    pub port: u16,
    /// Maximum number of concurrent connections
    pub max_connections: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5432,
            max_connections: 100,
        }
    }
}

/// Main server struct
pub struct Server {
    config: ServerConfig,
    database: Arc<Mutex<Session>>,
    listener: Option<TcpListener>,
    /// Shared, lock-free metrics for all connections handled by this server.
    metrics: Arc<crate::metrics::Metrics>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new(ServerConfig::default(), Session::new())
    }
}

impl Server {
    /// Create a new server with the given configuration and database
    pub fn new(config: ServerConfig, database: Session) -> Self {
        Self {
            config,
            database: Arc::new(Mutex::new(database)),
            listener: None,
            metrics: Arc::new(crate::metrics::Metrics::new()),
        }
    }
    
    /// Start the server and listen for connections
    /// This blocks until the server is shut down
    pub fn run(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = TcpListener::bind(&addr)?;
        info!("PostgreSQL wire protocol server listening on {}", addr);
        
        self.listener = Some(listener.try_clone()?);
        
        let mut handles = Vec::new();
        
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    self.metrics.record_connection_accepted();
                    let database = Arc::clone(&self.database);
                    let metrics = Arc::clone(&self.metrics);
                    let handle = thread::spawn(move || {
                        if let Err(e) = handle_connection(stream, database, metrics) {
                            // Check if it's a clean termination
                            if !matches!(e, PgWireError::Protocol(ref s) if s == "Terminate") {
                                error!("Connection error: {}", e);
                            }
                        }
                    });
                    handles.push(handle);
                    
                    // Limit concurrent connections
                    if handles.len() >= self.config.max_connections {
                        // Wait for one to finish
                        if let Some(handle) = handles.pop() {
                            let _ = handle.join();
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to accept connection: {}", e);
                }
            }
        }
        
        // Wait for all remaining handles
        for handle in handles {
            let _ = handle.join();
        }
        
        Ok(())
    }
    
    /// Get the local address the server is bound to
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener
            .as_ref()
            .ok_or_else(|| PgWireError::Protocol("Server not running".to_string()))?
            .local_addr()
            .map_err(Into::into)
    }

    /// Returns the shared metrics handle for this server. Read
    /// [`Metrics::snapshot`](crate::metrics::Metrics::snapshot) to observe
    /// connection/query/error throughput at runtime.
    pub fn metrics(&self) -> Arc<crate::metrics::Metrics> {
        Arc::clone(&self.metrics)
    }
    
    /// Shutdown the server
    pub fn shutdown(&mut self) {
        self.listener = None;
    }
}

/// Run a simple server with default configuration (blocking)
pub fn run_server(config: ServerConfig, database: Session) -> Result<()> {
    let mut server = Server::new(config, database);
    server.run()
}

/// Run a simple server with default configuration and empty database (blocking)
pub fn run_default_server() -> Result<()> {
    let mut server = Server::default();
    server.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::thread;
    
    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 5432);
        assert_eq!(config.max_connections, 100);
    }
}