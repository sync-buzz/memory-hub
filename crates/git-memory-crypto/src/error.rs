use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encryption failed: {0}")]
    Encrypt(String),

    #[error("decryption failed: {0}")]
    Decrypt(String),

    #[error("key operation failed: {0}")]
    Key(String),

    #[error("IO error: {0}")]
    Io(String),
}

impl From<std::io::Error> for CryptoError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<age::EncryptError> for CryptoError {
    fn from(e: age::EncryptError) -> Self {
        Self::Encrypt(e.to_string())
    }
}

impl From<age::DecryptError> for CryptoError {
    fn from(e: age::DecryptError) -> Self {
        Self::Decrypt(e.to_string())
    }
}
