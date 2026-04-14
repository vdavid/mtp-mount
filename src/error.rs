use thiserror::Error;

/// Errors returned by mtp-mount operations.
#[derive(Debug, Error)]

pub enum MountError {
    /// An error from the underlying MTP library.
    #[error("MTP error: {0}")]
    Mtp(#[from] mtp_rs::Error),

    /// An I/O error (filesystem, FUSE, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// No MTP device was found on the system.
    #[error("no MTP device found")]
    #[allow(dead_code)]
    NoDevice,

    /// A catch-all for other error conditions.
    #[error("{0}")]
    Other(String),
}
