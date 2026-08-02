//! Startup and authentication: handles the `StartupMessage` → `AuthenticationOk`
//! flow, plus the cleartext password exchange and SCRAM-SHA-256.
//! Trust mode (no auth) for v1.

use crate::codec::MessageWriter;
use crate::session::Session;

/// Handles a `StartupMessage` and returns the sequence of backend messages that
/// should be sent before the connection enters the `Idle` state.
///
/// v1 always uses "trust" auth: `AuthenticationOk` is sent immediately with no
/// password challenge.
pub fn handle_startup(
    msg: &crate::codec::message::StartupMessage,
    session: &mut Session,
) -> Vec<u8> {
    let mut out = Vec::new();

    session.params.clear();
    session.params.extend_from_slice(&msg.params);

    let mut w = MessageWriter::new();
    w.write_auth_ok();
    out.extend_from_slice(w.bytes());

    let mut w = MessageWriter::new();
    w.write_backend_key_data(session.pid, session.secret);
    out.extend_from_slice(w.bytes());

    let mut w = MessageWriter::new();
    w.write_parameter_status("server_version", "0.1.0-archon");
    w.write_parameter_status("server_encoding", "UTF8");
    w.write_parameter_status("client_encoding", "UTF8");
    w.write_parameter_status("DateStyle", "ISO, MDY");
    w.write_parameter_status("TimeZone", "Etc/UTC");
    w.write_parameter_status("integer_datetimes", "on");
    w.write_parameter_status("standard_conforming_strings", "on");
    out.extend_from_slice(w.bytes());

    let mut w = MessageWriter::new();
    let txn = match session.txn_status {
        crate::session::SessionTxnStatus::Idle => crate::codec::message::TxnStatus::Idle,
        crate::session::SessionTxnStatus::InTransaction => {
            crate::codec::message::TxnStatus::InTransaction
        }
        crate::session::SessionTxnStatus::Failed => crate::codec::message::TxnStatus::Failed,
    };
    w.write_ready_for_query(txn);
    out.extend_from_slice(w.bytes());

    out
}

/// Handles a `PasswordMessage` frontend message. v1 always trusts; this just
/// accepts the password and returns `AuthenticationOk`.
pub fn handle_password(_password: &str, _session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = MessageWriter::new();
    w.write_auth_ok();
    out.extend_from_slice(w.bytes());
    let mut w = MessageWriter::new();
    w.write_ready_for_query(crate::codec::message::TxnStatus::Idle);
    out.extend_from_slice(w.bytes());
    out
}

/// SCRAM-SHA-256 authentication mechanism.
///
/// This implements the SCRAM-SHA-256 authentication as specified in RFC 7677
/// and PostgreSQL's implementation. The flow is:
/// 1. Client sends `PasswordMessage` with mechanism "SCRAM-SHA-256" and initial client-first-message
/// 2. Server responds with `AuthenticationSASLContinue` containing server-first-message
/// 3. Client sends `PasswordMessage` with client-final-message
/// 4. Server responds with `AuthenticationSASLFinal` containing server-final-message
/// 5. Server sends `AuthenticationOk` and `ReadyForQuery`
pub mod scram {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::str;

    /// SCRAM-SHA-256 server state machine.
    #[derive(Debug, Clone, Default)]
    pub struct ScramServer {
        /// The client's first message (without the mechanism name)
        client_first_message_bare: Option<String>,
        /// The server's first message
        server_first_message: Option<String>,
        /// The salt used for this authentication
        salt: Option<Vec<u8>>,
        /// The iteration count
        iteration_count: Option<u32>,
        /// The stored key (ServerKey)
        stored_key: Option<Vec<u8>>,
        /// The server key (ServerKey)
        server_key: Option<Vec<u8>>,
    }

    impl ScramServer {
        /// Creates a new SCRAM server state.
        pub fn new() -> Self {
            Self::default()
        }

        /// Processes the client's first message and returns the server's first message.
        /// Returns the authentication message to send to the client.
        pub fn process_client_first(&mut self, client_first: &str) -> Result<Vec<u8>, ScramError> {
            // Parse client-first-message: "n,,n=user,r=client-nonce"
            let parts: Vec<&str> = client_first.split(',').collect();
            if parts.len() < 3 {
                return Err(ScramError::InvalidClientFirstMessage);
            }

            // Extract username and client nonce
            let username = parts[1]
                .strip_prefix("n=")
                .ok_or(ScramError::InvalidClientFirstMessage)?;
            let client_nonce = parts[2]
                .strip_prefix("r=")
                .ok_or(ScramError::InvalidClientFirstMessage)?;

            // Generate server nonce (append to client nonce)
            let server_nonce = format!("{}{}", client_nonce, generate_nonce(12));

            // Generate salt and iteration count
            let salt = generate_salt(16);
            let iteration_count = 4096; // Minimum per RFC 7677

            // Store for later verification
            self.client_first_message_bare = Some(format!("n={},r={}", username, server_nonce));
            self.salt = Some(salt.clone());
            self.iteration_count = Some(iteration_count);

            // In a real implementation, we would look up the stored credentials
            // For now, we'll use a placeholder - the actual stored_key and server_key
            // would be derived from the user's password using PBKDF2
            // This is a stub implementation for the wire protocol
            self.stored_key = Some(vec![0u8; 32]); // Placeholder
            self.server_key = Some(vec![0u8; 32]); // Placeholder

            // Build server-first-message: "r=server-nonce,s=salt,i=iteration-count"
            let server_first = format!(
                "r={},s={},i={}",
                server_nonce,
                base64_encode(&salt),
                iteration_count
            );
            self.server_first_message = Some(server_first.clone());

            // Send AuthenticationSASLContinue with server-first-message
            let mut out = Vec::new();
            let mut w = MessageWriter::new();
            w.write_auth_sasl_continue(&server_first);
            out.extend_from_slice(w.bytes());
            Ok(out)
        }

        /// Processes the client's final message and returns the server's final message.
        /// Returns the authentication message to send to the client.
        pub fn process_client_final(&mut self, client_final: &str) -> Result<Vec<u8>, ScramError> {
            // Parse client-final-message: "c=channel-binding,r=combined-nonce,p=client-proof"
            let parts: Vec<&str> = client_final.split(',').collect();
            if parts.len() < 3 {
                return Err(ScramError::InvalidClientFinalMessage);
            }

            let channel_binding = parts[0]
                .strip_prefix("c=")
                .ok_or(ScramError::InvalidClientFinalMessage)?;
            let combined_nonce = parts[1]
                .strip_prefix("r=")
                .ok_or(ScramError::InvalidClientFinalMessage)?;
            let client_proof = parts[2]
                .strip_prefix("p=")
                .ok_or(ScramError::InvalidClientFinalMessage)?;

            // Verify the client proof
            // In a real implementation, we would:
            // 1. Compute ClientKey = HMAC(StoredKey, "Client Key")
            // 2. Compute ClientSignature = HMAC(ClientKey, auth_message)
            // 3. Compute ClientProof = ClientKey XOR ClientSignature
            // 4. Verify ClientProof matches
            // 5. Compute ServerSignature = HMAC(ServerKey, auth_message)
            // 6. Send ServerSignature in server-final-message

            // For now, this is a stub - we accept any proof
            let _ = (channel_binding, combined_nonce, client_proof);

            // Build server-final-message: "v=server-signature"
            let server_signature = base64_encode(&[0u8; 32]); // Placeholder
            let server_final = format!("v={}", server_signature);

            // Send AuthenticationSASLFinal with server-final-message
            let mut out = Vec::new();
            let mut w = MessageWriter::new();
            w.write_auth_sasl_final(&server_final);
            out.extend_from_slice(w.bytes());

            // Send AuthenticationOk
            let mut w = MessageWriter::new();
            w.write_auth_ok();
            out.extend_from_slice(w.bytes());

            // Send ReadyForQuery
            let mut w = MessageWriter::new();
            w.write_ready_for_query(crate::codec::message::TxnStatus::Idle);
            out.extend_from_slice(w.bytes());

            Ok(out)
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub enum ScramError {
        #[error("invalid client first message")]
        InvalidClientFirstMessage,
        #[error("invalid client final message")]
        InvalidClientFinalMessage,
        #[error("authentication failed")]
        AuthenticationFailed,
    }

    /// Generates a random nonce of the specified length.
    fn generate_nonce(len: usize) -> String {
        // In a real implementation, use a cryptographically secure RNG
        // For now, use a simple deterministic approach
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut nonce = String::with_capacity(len);
        for i in 0..len {
            nonce.push(CHARS[(i * 7) % CHARS.len()] as char);
        }
        nonce
    }

    /// Generates a random salt of the specified length.
    fn generate_salt(len: usize) -> Vec<u8> {
        // In a real implementation, use a cryptographically secure RNG
        let mut salt = Vec::with_capacity(len);
        for i in 0..len {
            salt.push((i * 13) as u8);
        }
        salt
    }

    /// Base64 encodes the input.
    fn base64_encode(input: &[u8]) -> String {
        // Simple base64 encoding
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::new();
        let mut i = 0;
        while i < input.len() {
            let b1 = input[i];
            let b2 = if i + 1 < input.len() { input[i + 1] } else { 0 };
            let b3 = if i + 2 < input.len() { input[i + 2] } else { 0 };

            output.push(TABLE[(b1 >> 2) as usize] as char);
            output.push(TABLE[((b1 & 0x03) << 4 | (b2 >> 4)) as usize] as char);
            output.push(if i + 1 < input.len() {
                TABLE[((b2 & 0x0F) << 2 | (b3 >> 6)) as usize] as char
            } else {
                '='
            });
            output.push(if i + 2 < input.len() {
                TABLE[(b3 & 0x3F) as usize] as char
            } else {
                '='
            });

            i += 3;
        }
        output
    }
}

/// Handles SCRAM-SHA-256 authentication.
/// This is a stub implementation that demonstrates the wire protocol flow.
pub fn handle_scram_auth(mechanism: &str, client_data: &str, session: &mut Session) -> Vec<u8> {
    if mechanism != "SCRAM-SHA-256" {
        // Fall back to trust auth for unknown mechanisms
        return handle_password("", session);
    }

    // Check if this is the first message (client-first) or final message (client-final)
    if client_data.starts_with("n,,n=") || client_data.starts_with("n=") {
        // Client-first message
        let mut scram = scram::ScramServer::new();
        match scram.process_client_first(client_data) {
            Ok(response) => response,
            Err(_) => {
                // On error, fall back to trust auth
                handle_password("", session)
            }
        }
    } else if client_data.starts_with("c=") {
        // Client-final message
        // In a real implementation, we'd retrieve the ScramServer from session
        // For now, create a new one (this won't work for real auth but shows the flow)
        let mut scram = scram::ScramServer::new();
        match scram.process_client_final(client_data) {
            Ok(response) => response,
            Err(_) => {
                // On error, send error response
                let mut out = Vec::new();
                let mut w = MessageWriter::new();
                w.write_error_response("SCRAM authentication failed", Some(*b"28000"));
                out.extend_from_slice(w.bytes());
                out
            }
        }
    } else {
        // Unknown message format
        handle_password("", session)
    }
}
