use crate::lrp::LocationReference;

pub mod v3;
pub mod tpeg;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("input too short: need at least {min} bytes, got {got}")]
    TooShort { min: usize, got: usize },
    #[error("invalid magic / version byte: {0:#04x}")]
    InvalidHeader(u8),
    #[error("trailing bytes after valid payload ({0} bytes)")]
    TrailingBytes(usize),
    #[error("base64 decode failed: {0}")]
    Base64(String),
    #[error("hex decode failed: {0}")]
    Hex(String),
    #[error("unexpected subcomponent id: expected {expected:#04x}, got {got:#04x}")]
    InvalidComponent { expected: u8, got: u8 },
    #[error("TPEG location type not supported: {0:#04x}")]
    InvalidLocationType(u8),
    #[error("TPEG length field says {expected} bytes, payload is {got} bytes")]
    LengthMismatch { expected: usize, got: usize },
}

pub trait Decoder {
    fn decode(&self, bytes: &[u8]) -> Result<LocationReference, DecodeError>;
}
