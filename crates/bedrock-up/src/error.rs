use std::path::PathBuf;

/// Errors from the check/download/apply pipeline.
///
/// A supervisor running unattended needs to distinguish "transient, retry
/// later" from "broken, stop trying and shout" — a bare `String` can't
/// express that, so callers should match on [`UpdateError::is_retryable`].
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The update API responded, but not in the shape this tool understands
    /// — the API changed and needs a human, not a retry.
    #[error("unexpected response from the update API: {0}")]
    UpstreamFormat(String),

    #[error("failed to download update from {url}: {source}")]
    Download {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to read update archive: {0}")]
    Archive(#[from] zip::result::ZipError),

    #[error("I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{} is in use and could not be replaced", path.display())]
    InUse { path: PathBuf },

    /// The update landed on disk; only the cache bookkeeping failed. Callers
    /// must not treat this as a failed update and retry the whole thing.
    #[error("update applied but failed to write the cache at {}: {source}", path.display())]
    CacheWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A `StagedUpdate::load` found a staging directory that is missing or
    /// cannot be parsed as one this tool wrote — not retryable, since a
    /// broken stage needs a fresh `download`, not another attempt to read it.
    #[error("staged update at {} is invalid: {reason}", path.display())]
    Staging { path: PathBuf, reason: String },
}

impl UpdateError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, UpdateError::Network(_) | UpdateError::Download { .. })
    }
}
