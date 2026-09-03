use std::io;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("registry domain error: {0}")]
    Registry(#[from] mahoquot_registry::RegistryError),

    #[error("verification error: {0}")]
    Verification(#[from] mahoquot_registry::CatalogVerificationError),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("invalid state: {0}")]
    InvalidState(String),
}
