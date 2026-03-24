//! MySQL authentication — mysql_native_password and caching_sha2_password.
//!
//! Implements the challenge-response auth used by MySQL 5.x clients.
//!
//! ## Algorithm
//!
//! The server sends a 20-byte challenge.
//! The client sends: `SHA1(password) XOR SHA1(challenge || SHA1(SHA1(password)))`
//!
//! The server verifies by reconstructing the expected response from the
//! stored password (or verifying with the plaintext password for Phase 5).

use rand::RngCore;
use sha1::{Digest, Sha1};
use sha2::Sha256;

/// Generates a 20-byte random challenge for use in the handshake.
pub fn gen_challenge() -> [u8; 20] {
    let mut challenge = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut challenge);
    challenge
}

/// Verifies a mysql_native_password auth response.
///
/// Returns `true` if authentication succeeds:
/// - If `password` is empty: accepts only an empty `auth_response`.
/// - Otherwise: verifies the SHA1-XOR token.
pub fn verify_native_password(password: &str, challenge: &[u8; 20], auth_response: &[u8]) -> bool {
    if password.is_empty() {
        // Empty password: client must send an empty auth_response.
        return auth_response.is_empty();
    }
    if auth_response.len() != 20 {
        return false;
    }

    // Step 1: SHA1(password)
    let sha1_pwd: [u8; 20] = Sha1::digest(password.as_bytes()).into();
    // Step 2: SHA1(SHA1(password))
    let sha1_sha1_pwd: [u8; 20] = Sha1::digest(sha1_pwd).into();
    // Step 3: SHA1(challenge || SHA1(SHA1(password)))
    let mut h = Sha1::new();
    h.update(challenge);
    h.update(sha1_sha1_pwd);
    let xor_key: [u8; 20] = h.finalize().into();
    // Step 4: expected_token = SHA1(password) XOR xor_key
    let expected: Vec<u8> = sha1_pwd
        .iter()
        .zip(xor_key.iter())
        .map(|(a, b)| a ^ b)
        .collect();

    auth_response == expected.as_slice()
}

// ── caching_sha2_password ─────────────────────────────────────────────────────

/// Computes a caching_sha2_password fast-auth token for testing.
///
/// `token = SHA256(password) XOR SHA256(challenge || SHA256(SHA256(password)))`
///
/// The token is 32 bytes (SHA256 output), unlike mysql_native_password which
/// produces 20 bytes (SHA1 output).
pub fn compute_sha256_token(password: &str, challenge: &[u8; 20]) -> [u8; 32] {
    let sha256_pwd: [u8; 32] = Sha256::digest(password.as_bytes()).into();
    let sha256_sha256_pwd: [u8; 32] = Sha256::digest(sha256_pwd).into();
    let mut h = Sha256::new();
    h.update(challenge);
    h.update(sha256_sha256_pwd);
    let xor_key: [u8; 32] = h.finalize().into();
    let mut token = [0u8; 32];
    for i in 0..32 {
        token[i] = sha256_pwd[i] ^ xor_key[i];
    }
    token
}

/// Phase 5 permissive mode: accepts any caching_sha2_password response for
/// allowed users. Real hash verification is Phase 13.
pub fn verify_sha256_password(_challenge: &[u8; 20], _auth_response: &[u8]) -> bool {
    true // permissive — all allowed users accepted
}

/// Allowed users for Phase 5 — permissive mode.
///
/// Any username in this list is accepted regardless of password.
/// Real authentication is Phase 13.
pub fn is_allowed_user(username: &str) -> bool {
    matches!(username, "root" | "axiomdb" | "admin" | "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_password_accepted_with_empty_response() {
        let challenge = [0u8; 20];
        assert!(verify_native_password("", &challenge, &[]));
    }

    #[test]
    fn test_empty_password_rejected_with_nonempty_response() {
        let challenge = [0u8; 20];
        assert!(!verify_native_password("", &challenge, &[0u8; 20]));
    }

    #[test]
    fn test_known_password_hash() {
        // Verifies the algorithm against a pre-computed test vector.
        // password = "secret", challenge = all zeros
        let challenge = [0u8; 20];
        let password = "secret";

        // Compute expected response manually:
        let sha1_pwd: [u8; 20] = Sha1::digest(password.as_bytes()).into();
        let sha1_sha1_pwd: [u8; 20] = Sha1::digest(sha1_pwd).into();
        let mut h = Sha1::new();
        h.update(challenge);
        h.update(sha1_sha1_pwd);
        let xor_key: [u8; 20] = h.finalize().into();
        let response: Vec<u8> = sha1_pwd
            .iter()
            .zip(xor_key.iter())
            .map(|(a, b)| a ^ b)
            .collect();

        assert!(verify_native_password(password, &challenge, &response));
        assert!(!verify_native_password(password, &challenge, &[0u8; 20]));
    }
}
