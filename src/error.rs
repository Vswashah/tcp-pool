use thiserror::Error;

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),

    #[error("acquire timed out")]
    Timeout,

    /// The pool was dropped while a caller was waiting on the semaphore.
    #[error("pool closed")]
    Closed,
}
