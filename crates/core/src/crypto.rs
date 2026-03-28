use anyhow::{bail, Result};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use std::io::{Read, Write};

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAX_ENCRYPTED_SIZE: usize = 64 * 1024 * 1024;

/// Generates a random 32-byte key and returns it as a hex string.
pub fn generate_key_hex() -> String {
    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    hex::encode(&key)
}

/// Parses a hex-encoded 32-byte key.
pub fn parse_key_hex(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str).map_err(|e| anyhow::anyhow!("invalid hex key: {e}"))?;
    if bytes.len() != 32 {
        bail!("key must be 32 bytes (64 hex chars), got {} bytes", bytes.len());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn make_nonce(session_id: [u8; 4], counter: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..4].copy_from_slice(&session_id);
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

fn random_session_id() -> [u8; 4] {
    use rand::RngCore;
    let mut id = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

/// Encrypted writer: wraps any Write with ChaCha20-Poly1305.
/// Wire format per message: [4-byte total_len][12-byte nonce][ciphertext + 16-byte tag]
/// Each session gets a random 4-byte prefix to prevent nonce reuse across sessions.
pub struct EncryptedWriter<W: Write> {
    inner: W,
    cipher: ChaCha20Poly1305,
    counter: u64,
    session_id: [u8; 4],
}

impl<W: Write> EncryptedWriter<W> {
    pub fn new(writer: W, key: &[u8; 32]) -> Self {
        Self {
            inner: writer,
            cipher: ChaCha20Poly1305::new(key.into()),
            counter: 0,
            session_id: random_session_id(),
        }
    }

    pub fn write_encrypted(&mut self, plaintext: &[u8]) -> Result<()> {
        self.counter += 1;
        let nonce_bytes = make_nonce(self.session_id, self.counter);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| anyhow::anyhow!("encrypt failed: {e}"))?;

        let total_len = (NONCE_LEN + ciphertext.len()) as u32;
        self.inner.write_all(&total_len.to_be_bytes())?;
        self.inner.write_all(&nonce_bytes)?;
        self.inner.write_all(&ciphertext)?;
        self.inner.flush()?;
        Ok(())
    }
}

/// Encrypted reader: wraps any Read with ChaCha20-Poly1305.
/// Nonce is read from the wire (sent by writer). AEAD tag provides authentication.
pub struct EncryptedReader<R: Read> {
    inner: R,
    cipher: ChaCha20Poly1305,
}

impl<R: Read> EncryptedReader<R> {
    pub fn new(reader: R, key: &[u8; 32]) -> Self {
        Self {
            inner: reader,
            cipher: ChaCha20Poly1305::new(key.into()),
        }
    }

    pub fn read_decrypted(&mut self) -> Result<Vec<u8>> {
        let mut len_buf = [0u8; 4];
        self.inner.read_exact(&mut len_buf)?;
        let total_len = u32::from_be_bytes(len_buf) as usize;

        if total_len < NONCE_LEN + TAG_LEN {
            bail!("encrypted message too short ({total_len} bytes)");
        }
        if total_len > MAX_ENCRYPTED_SIZE {
            bail!("encrypted message too large ({total_len} bytes)");
        }

        let mut nonce_bytes = [0u8; NONCE_LEN];
        self.inner.read_exact(&mut nonce_bytes)?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ct_len = total_len - NONCE_LEN;
        let mut ciphertext = vec![0u8; ct_len];
        self.inner.read_exact(&mut ciphertext)?;

        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|_| anyhow::anyhow!("decryption failed"))?;

        Ok(plaintext)
    }
}

// We need hex encoding - use a minimal inline impl to avoid adding a dep
mod hex {
    pub fn encode(data: &[u8]) -> String {
        data.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        if !s.len().is_multiple_of(2) {
            return Err("odd length hex string".into());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"hello, phantom remote desktop!";

        let mut buf = Vec::new();
        {
            let mut writer = EncryptedWriter::new(&mut buf, &key);
            writer.write_encrypted(plaintext).unwrap();
            writer.write_encrypted(b"second message").unwrap();
        }

        let mut reader = EncryptedReader::new(Cursor::new(&buf), &key);
        let msg1 = reader.read_decrypted().unwrap();
        assert_eq!(msg1, plaintext);
        let msg2 = reader.read_decrypted().unwrap();
        assert_eq!(msg2, b"second message");
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = [1u8; 32];
        let key2 = [2u8; 32];

        let mut buf = Vec::new();
        {
            let mut writer = EncryptedWriter::new(&mut buf, &key1);
            writer.write_encrypted(b"secret").unwrap();
        }

        let mut reader = EncryptedReader::new(Cursor::new(&buf), &key2);
        let result = reader.read_decrypted();
        assert!(result.is_err());
    }

    #[test]
    fn key_hex_roundtrip() {
        let hex = generate_key_hex();
        assert_eq!(hex.len(), 64);
        let key = parse_key_hex(&hex).unwrap();
        assert_eq!(hex, super::hex::encode(&key));
    }
}
