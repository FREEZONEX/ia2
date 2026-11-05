//! Read-only, scalar CoE upload contract. No download capability is exposed.
use async_trait::async_trait;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SdoAddress {
    pub slave: u16,
    pub index: u16,
    pub subindex: u8,
}

#[derive(Debug, Serialize)]
pub struct SdoValue {
    pub address: SdoAddress,
    /// Exact little-endian payload; authoritative even in JSON clients with
    /// limited integer precision. Width and signedness are not guessed.
    pub bytes: Vec<u8>,
    pub unsigned: u64,
    pub signed: i64,
    pub vendor_id: u32,
    pub product_id: u32,
    pub revision: u32,
    pub serial: u32,
    pub sampled_at_unix_ms: u64,
}

#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SdoError {
    #[error("invalid SDO request: {0}")]
    Invalid(String),
    #[error("SDO target not found: {0}")]
    NotFound(String),
    #[error("device does not support live SDO upload")]
    Unsupported,
    #[error("SDO upload already in progress")]
    Busy,
    #[error("SDO service unavailable: {0}")]
    Unavailable(String),
    #[error("SDO upload timed out")]
    Timeout,
    #[error("SDO protocol error: {0}")]
    Protocol(String),
}

impl SdoError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound(_) => 404,
            Self::Unsupported | Self::Busy => 409,
            Self::Unavailable(_) => 503,
            Self::Timeout => 504,
            Self::Protocol(_) => 502,
        }
    }
}

#[async_trait]
pub trait SdoUpload: Send + Sync {
    async fn upload(&self, address: SdoAddress) -> Result<SdoValue, SdoError>;
}
