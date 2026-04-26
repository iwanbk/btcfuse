use thiserror::Error;

#[derive(Debug, Error)]
pub enum BtcFuseError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid format: {0}")]
    InvalidFormat(String),

    #[error("rate limit exceeded")]
    RateLimit,

    #[error("rpc error: {0}")]
    Rpc(String),

    #[error("cache error: {0}")]
    Cache(String),
}

impl From<BtcFuseError> for libc::c_int {
    fn from(e: BtcFuseError) -> Self {
        match e {
            BtcFuseError::NotFound(_) => libc::ENOENT,
            BtcFuseError::InvalidFormat(_) => libc::EINVAL,
            BtcFuseError::RateLimit => libc::EAGAIN,
            BtcFuseError::Rpc(_) | BtcFuseError::Cache(_) => libc::EIO,
        }
    }
}
