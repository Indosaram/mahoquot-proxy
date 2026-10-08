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

    /// The remote catalog has never been published (404 on the pinned URLs).
    ///
    /// This is the normal state of an offline-first deployment that ships only
    /// the embedded catalog: it is not a failure, so it must not mark the active
    /// catalog stale or raise an operator-facing error.
    #[error("remote catalog is not published")]
    RemoteUnpublished,

    #[error("invalid state: {0}")]
    InvalidState(String),
}
