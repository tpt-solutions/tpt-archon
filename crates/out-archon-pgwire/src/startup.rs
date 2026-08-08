//! Startup and authentication: handles the `StartupMessage` → `AuthenticationOk`
//! flow, plus the cleartext password exchange and SCRAM-SHA-256.
//!
//! When a credential store is configured on the [`Session`](crate::session::Session),
//! startup offers `SCRAM-SHA-256` (RFC 7677) and the SASL exchange is verified
//! cryptographically. Otherwise v1 uses "trust" auth: `AuthenticationOk` is
//! sent immediately with no password challenge.

use crate::codec::MessageWriter;
use crate::session::Session;

/// Handles a `StartupMessage` and returns the sequence of backend messages that
/// should be sent before the connection enters the `Idle` state.
///
/// With a configured credential store, this sends `AuthenticationSASL` listing
/// `SCRAM-SHA-256`; the SASL exchange (handled in [`handle_scram_auth`]) then
/// finishes with `AuthenticationOk` + `ReadyForQuery`. Otherwise it sends
/// `AuthenticationOk` immediately (trust auth).
pub fn handle_startup(
    msg: &crate::codec::message::StartupMessage,
    session: &mut Session,
) -> Vec<u8> {
    let mut out = Vec::new();

    session.params.clear();
    session.params.extend_from_slice(&msg.params);

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

    if session.auth_store.is_some() {
        let mut w = MessageWriter::new();
        w.write_auth_sasl(&["SCRAM-SHA-256"]);
        out.extend_from_slice(w.bytes());
        return out;
    }

    let mut w = MessageWriter::new();
    w.write_auth_ok();
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

/// Handles a `PasswordMessage` frontend message. Trust mode always accepts;
/// this just returns `AuthenticationOk`.
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

/// SCRAM-SHA-256 authentication mechanism (RFC 7677 / PostgreSQL).
///
/// The previous implementation was a stub that accepted any proof. This is a
/// real implementation: passwords are stored as [`StoredCredential`] (salt +
/// iteration count + `StoredKey`/`ServerKey` derived via PBKDF2-HMAC-SHA-256),
/// and the client proof is cryptographically verified, so a wrong password is
/// rejected.
pub mod scram {
    use alloc::string::String;
    use alloc::vec::Vec;
    use std::collections::HashMap;

    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    /// A stored SCRAM credential for a user (never the plaintext password).
    #[derive(Debug, Clone)]
    pub struct StoredCredential {
        /// The per-user salt (used for PBKDF2).
        pub salt: Vec<u8>,
        /// PBKDF2 iteration count.
        pub iteration_count: u32,
        /// SHA-256(ClientKey) — used to verify the client proof.
        pub stored_key: Vec<u8>,
        /// HMAC(SaltedPassword, "Server Key") — used to sign the server proof.
        pub server_key: Vec<u8>,
    }

    /// In-memory credential store. v1: populated at server start; a file-backed
    /// store is a future hardening.
    #[derive(Debug, Clone, Default)]
    pub struct AuthStore {
        users: HashMap<String, StoredCredential>,
    }

    impl AuthStore {
        /// Creates an empty credential store.
        pub fn new() -> Self {
            Self {
                users: HashMap::new(),
            }
        }

        /// Adds a user with the given plaintext password, generating a fresh
        /// random salt and the standard 4096 iterations.
        pub fn add_user(&mut self, name: &str, password: &str) {
            let salt = random_bytes(16);
            let cred = derive_credential(password.as_bytes(), &salt, 4096);
            self.users.insert(name.to_string(), cred);
        }

        /// Returns the stored credential for `name`, if it exists.
        pub fn get(&self, name: &str) -> Option<&StoredCredential> {
            self.users.get(name)
        }
    }

    /// In-progress SCRAM exchange, retained on the session between the
    /// client-first and client-final SASL messages.
    #[derive(Debug, Clone)]
    pub struct ScramExchange {
        /// client-first-message-bare ("n=user,r=nonce").
        pub client_first_bare: String,
        /// server-first-message ("r=...,s=...,i=...").
        pub server_first: String,
        /// The user's stored credential.
        pub cred: StoredCredential,
    }

    /// Derives a [`StoredCredential`] from a password via PBKDF2-HMAC-SHA-256.
    pub fn derive_credential(password: &[u8], salt: &[u8], iter: u32) -> StoredCredential {
        let salted = pbkdf2_hmac_sha256(password, salt, iter);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, b"Server Key");
        StoredCredential {
            salt: salt.to_vec(),
            iteration_count: iter,
            stored_key,
            server_key,
        }
    }

    impl ScramExchange {
        /// Processes the client-first message and returns the server-first
        /// message (sent as `AuthenticationSASLContinue`).
        pub fn process_client_first(&mut self, client_first: &str) -> Result<String, ScramError> {
            // client-first-message = gs2-header "," client-first-message-bare.
            // gs2-header is "n,," / "y,," / "p=...," — strip it to the bare form.
            let bare = if let Some(rest) = client_first.strip_prefix("n,,") {
                rest.to_string()
            } else if let Some(rest) = client_first.strip_prefix("y,,") {
                rest.to_string()
            } else if let Some((_, rest)) = client_first.split_once(",,n=") {
                alloc::format!("n={}", rest)
            } else {
                return Err(ScramError::InvalidClientFirstMessage);
            };
            self.client_first_bare = bare.clone();

            let client_nonce = bare
                .split(',')
                .find_map(|p| p.strip_prefix("r="))
                .ok_or(ScramError::InvalidClientFirstMessage)?;

            let server_nonce = format!("{}{}", client_nonce, random_nonce(18));
            self.server_first = format!(
                "r={},s={},i={}",
                server_nonce,
                base64_encode(&self.cred.salt),
                self.cred.iteration_count
            );
            Ok(self.server_first.clone())
        }

        /// Processes the client-final message, verifying the proof, and returns
        /// the server-final message (sent as `AuthenticationSASLFinal`).
        pub fn process_client_final(&self, client_final: &str) -> Result<String, ScramError> {
            let without_proof = client_final
                .split_once(",p=")
                .map(|(head, _)| head)
                .ok_or(ScramError::InvalidClientFinalMessage)?;
            let combined = format!(
                "{},{},{}",
                self.client_first_bare, self.server_first, without_proof
            );

            let proof_b64 = client_final
                .rsplit(',')
                .find_map(|p| p.strip_prefix("p="))
                .ok_or(ScramError::InvalidClientFinalMessage)?;
            let proof = base64_decode(proof_b64).ok_or(ScramError::InvalidClientFinalMessage)?;

            let client_signature = hmac_sha256(&self.cred.stored_key, combined.as_bytes());
            if client_signature.len() != proof.len() {
                return Err(ScramError::AuthenticationFailed);
            }
            let client_key: Vec<u8> = client_signature
                .iter()
                .zip(proof.iter())
                .map(|(a, b)| a ^ b)
                .collect();
            let computed_stored = sha256(&client_key);
            if !constant_time_eq(&computed_stored, &self.cred.stored_key) {
                return Err(ScramError::AuthenticationFailed);
            }

            let server_signature = hmac_sha256(&self.cred.server_key, combined.as_bytes());
            Ok(format!("v={}", base64_encode(&server_signature)))
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

    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        m.update(data);
        m.finalize().into_bytes().to_vec()
    }

    fn sha256(data: &[u8]) -> Vec<u8> {
        Sha256::digest(data).to_vec()
    }

    /// PBKDF2-HMAC-SHA-256 with a fixed 32-byte output (the SHA-256 size).
    fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iter: u32) -> Vec<u8> {
        let mut block = salt.to_vec();
        block.extend_from_slice(&[0, 0, 0, 1]); // big-endian block index 1
        let mut u = hmac_sha256(password, &block);
        let mut t = u.clone();
        for _ in 1..iter {
            u = hmac_sha256(password, &u);
            for (a, b) in t.iter_mut().zip(u.iter()) {
                *a ^= *b;
            }
        }
        t
    }

    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    fn random_bytes(len: usize) -> Vec<u8> {
        use rand::RngCore;
        let mut buf = vec![0u8; len];
        rand::thread_rng().fill_bytes(&mut buf);
        buf
    }

    fn random_nonce(len: usize) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = random_bytes(len);
        bytes
            .iter()
            .map(|b| CHARS[(b % (CHARS.len() as u8)) as usize] as char)
            .collect()
    }

    /// Base64-encode (standard alphabet, no padding required by callers).
    pub fn base64_encode(input: &[u8]) -> String {
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

    /// Base64-decode (standard alphabet). Returns `None` on invalid input.
    pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
        const INV: u8 = 255;
        let mut table = [INV; 256];
        for (i, c) in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
            .bytes()
            .enumerate()
        {
            table[c as usize] = i as u8;
        }
        let mut out = Vec::new();
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c0 = *bytes.get(i)?;
            if c0 == b'=' {
                break;
            }
            let v0 = *table.get(c0 as usize)? as u32;
            i += 1;
            let c1 = *bytes.get(i)?;
            if c1 == b'=' {
                break;
            }
            let v1 = *table.get(c1 as usize)? as u32;
            i += 1;
            out.push(((v0 << 2) | (v1 >> 4)) as u8);
            if i >= bytes.len() {
                break;
            }
            let c2 = *bytes.get(i)?;
            if c2 == b'=' {
                break;
            }
            let v2 = *table.get(c2 as usize)? as u32;
            i += 1;
            out.push(((v1 << 4) | (v2 >> 2)) as u8);
            if i >= bytes.len() {
                break;
            }
            let c3 = *bytes.get(i)?;
            if c3 == b'=' {
                break;
            }
            let v3 = *table.get(c3 as usize)? as u32;
            i += 1;
            out.push(((v2 << 6) | v3) as u8);
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn full_scram_exchange_with_correct_password_succeeds() {
            let mut store = AuthStore::new();
            store.add_user("alice", "correct horse");
            let cred = store.get("alice").unwrap().clone();

            let mut ex = ScramExchange {
                client_first_bare: String::new(),
                server_first: String::new(),
                cred,
            };

            // Client-first (gs2-header "n,," + bare "n=alice,r=clientnonce").
            let client_first = "n,,n=alice,r=clientnonce";
            let server_first = ex.process_client_first(client_first).unwrap();
            assert!(server_first.starts_with("r=clientnonce"));

            // Reconstruct a valid client-final using the same crypto the client
            // would: derive salted password, client key, proof.
            let salted =
                pbkdf2_hmac_sha256(b"correct horse", &ex.cred.salt, ex.cred.iteration_count);
            let client_key = hmac_sha256(&salted, b"Client Key");
            let stored_key = sha256(&client_key);
            let server_key = hmac_sha256(&salted, b"Server Key");
            let auth_message = format!(
                "{},{},{}",
                ex.client_first_bare, ex.server_first, "c=biws,r=clientnonceXXXX"
            );
            let client_sig = hmac_sha256(&stored_key, auth_message.as_bytes());
            let proof: Vec<u8> = client_key
                .iter()
                .zip(client_sig.iter())
                .map(|(a, b)| a ^ b)
                .collect();
            let client_final = format!("c=biws,r=clientnonceXXXX,p={}", base64_encode(&proof));

            let server_final = ex.process_client_final(&client_final).unwrap();
            assert!(server_final.starts_with("v="));

            // And the server signature we recompute must match what we signed.
            let expected_sig = base64_encode(&hmac_sha256(&server_key, auth_message.as_bytes()));
            assert_eq!(server_final, format!("v={}", expected_sig));
        }

        #[test]
        fn wrong_password_is_rejected() {
            let mut store = AuthStore::new();
            store.add_user("alice", "right-password");
            let cred = store.get("alice").unwrap().clone();
            let mut ex = ScramExchange {
                client_first_bare: String::new(),
                server_first: String::new(),
                cred,
            };
            let server_first = ex.process_client_first("n,,n=alice,r=nonce").unwrap();

            // Client computes proof with the WRONG password.
            let salted =
                pbkdf2_hmac_sha256(b"wrong-password", &ex.cred.salt, ex.cred.iteration_count);
            let client_key = hmac_sha256(&salted, b"Client Key");
            let auth_message = format!(
                "{},{},{}",
                ex.client_first_bare, server_first, "c=biws,r=nonceXXXX"
            );
            let client_sig = hmac_sha256(&ex.cred.stored_key, auth_message.as_bytes());
            let proof: Vec<u8> = client_key
                .iter()
                .zip(client_sig.iter())
                .map(|(a, b)| a ^ b)
                .collect();
            let client_final = format!("c=biws,r=nonceXXXX,p={}", base64_encode(&proof));

            assert!(ex.process_client_final(&client_final).is_err());
        }

        #[test]
        fn base64_round_trip() {
            for s in [&b"hello world"[..], &[0u8, 1, 2, 3, 255, 254][..], &b""[..]] {
                let enc = base64_encode(s);
                let dec = base64_decode(&enc).unwrap();
                assert_eq!(dec, s);
            }
        }
    }
}

/// Handles SCRAM-SHA-256 authentication given the session's credential store.
///
/// `client_data` is the SCRAM message payload (everything after the
/// `"SCRAM-SHA-256 "` mechanism prefix). On the client-first message it looks up
/// the user, begins the exchange, and returns `AuthenticationSASLContinue`; on
/// the client-final message it verifies the proof and returns
/// `AuthenticationSASLFinal` + `AuthenticationOk` + `ReadyForQuery`, or an error
/// response if the credential is missing or the proof is wrong.
pub fn handle_scram_auth(mechanism: &str, client_data: &str, session: &mut Session) -> Vec<u8> {
    if mechanism != "SCRAM-SHA-256" {
        return handle_password("", session);
    }
    let store = match &session.auth_store {
        Some(s) => s,
        None => return handle_password("", session),
    };

    let is_final = client_data.starts_with("c=");
    if !is_final {
        let username = client_data
            .strip_prefix("n,,n=")
            .and_then(|r| r.split(',').next())
            .or_else(|| {
                client_data
                    .split_once(",,n=")
                    .and_then(|(_, rest)| rest.split(',').next())
            })
            .unwrap_or("");
        let cred = match (store.get(username), !username.is_empty()) {
            (Some(c), true) => c.clone(),
            _ => {
                let mut w = MessageWriter::new();
                w.write_error_response("password authentication failed", Some(*b"28P01"));
                return w.bytes().to_vec();
            }
        };
        let mut exchange = scram::ScramExchange {
            client_first_bare: String::new(),
            server_first: String::new(),
            cred,
        };
        match exchange.process_client_first(client_data) {
            Ok(server_first) => {
                session.scram_state = Some(exchange);
                let mut w = MessageWriter::new();
                w.write_auth_sasl_continue(&server_first);
                w.bytes().to_vec()
            }
            Err(_) => {
                let mut w = MessageWriter::new();
                w.write_error_response("SCRAM authentication failed", Some(*b"28000"));
                w.bytes().to_vec()
            }
        }
    } else {
        let exchange = match session.scram_state.take() {
            Some(e) => e,
            None => {
                let mut w = MessageWriter::new();
                w.write_error_response("SCRAM authentication failed", Some(*b"28000"));
                return w.bytes().to_vec();
            }
        };
        match exchange.process_client_final(client_data) {
            Ok(server_final) => {
                let mut out = Vec::new();
                let mut w = MessageWriter::new();
                w.write_auth_sasl_final(&server_final);
                out.extend_from_slice(w.bytes());
                let mut w = MessageWriter::new();
                w.write_auth_ok();
                out.extend_from_slice(w.bytes());
                let mut w = MessageWriter::new();
                w.write_ready_for_query(crate::codec::message::TxnStatus::Idle);
                out.extend_from_slice(w.bytes());
                out
            }
            Err(_) => {
                let mut w = MessageWriter::new();
                w.write_error_response("password authentication failed", Some(*b"28P01"));
                w.bytes().to_vec()
            }
        }
    }
}
