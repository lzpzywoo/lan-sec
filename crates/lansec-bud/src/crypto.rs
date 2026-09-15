use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

const INFO: &[u8] = b"lansec-bud-v1";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("aes-gcm decrypt failed")]
    Decrypt,
    #[error("aes-gcm encrypt failed")]
    Encrypt,
}

pub struct Handshake {
    secret: StaticSecret,
    pub public: PublicKey,
    pub nonce: [u8; 16],
}

impl Handshake {
    pub fn new() -> Self {
        let mut rng = rand::rngs::OsRng;
        let mut nonce = [0u8; 16];
        rng.fill_bytes(&mut nonce);
        let secret = StaticSecret::random_from_rng(rng);
        let public = PublicKey::from(&secret);
        Self {
            secret,
            public,
            nonce,
        }
    }

    pub fn derive(&self, peer_public: &PublicKey, peer_nonce: &[u8; 16], pin: &str) -> SessionKeys {
        let shared = self.secret.diffie_hellman(peer_public);
        let mut salt = [0u8; 32];
        salt[..16].copy_from_slice(&self.nonce);
        salt[16..].copy_from_slice(peer_nonce);
        // Order salt so both peers get the same key regardless of who started.
        if self.nonce < *peer_nonce {
            salt[..16].copy_from_slice(&self.nonce);
            salt[16..].copy_from_slice(peer_nonce);
        } else {
            salt[..16].copy_from_slice(peer_nonce);
            salt[16..].copy_from_slice(&self.nonce);
        }
        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
        let mut okm = [0u8; 32];
        let pin_hash = Sha256::digest(pin.as_bytes());
        let mut info = Vec::from(INFO);
        info.extend_from_slice(&pin_hash);
        hk.expand(&info, &mut okm).expect("hkdf");
        SessionKeys::new(okm)
    }
}

#[derive(Clone)]
pub struct SessionKeys {
    cipher: Aes256Gcm,
}

impl SessionKeys {
    fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new_from_slice(&key).expect("aes key"),
        }
    }

    pub fn seal(&self, nonce: u64, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let n = seq_nonce(nonce);
        self.cipher
            .encrypt(Nonce::from_slice(&n), Payload { msg: plaintext, aad })
            .map_err(|_| CryptoError::Encrypt)
    }

    pub fn open(&self, nonce: u64, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let n = seq_nonce(nonce);
        self.cipher
            .decrypt(Nonce::from_slice(&n), Payload { msg: ciphertext, aad })
            .map_err(|_| CryptoError::Decrypt)
    }
}

fn seq_nonce(seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&seq.to_le_bytes());
    n
}

#[allow(dead_code)]
pub fn pin_hash(pin: &str) -> [u8; 32] {
    Sha256::digest(pin.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_and_roundtrip() {
        let a = Handshake::new();
        let b = Handshake::new();
        let ka = a.derive(&b.public, &b.nonce, "1234");
        let kb = b.derive(&a.public, &a.nonce, "1234");
        let ct = ka.seal(7, b"hdr", b"hello").unwrap();
        let pt = kb.open(7, b"hdr", &ct).unwrap();
        assert_eq!(pt, b"hello");
    }
}
