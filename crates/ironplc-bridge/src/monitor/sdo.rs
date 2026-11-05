//! Shared server/edge read-only upload semantics.
use crate::ProgramHandle;
use iocore::sdo::{SdoAddress, SdoError};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadQuery {
    pub device: String,
    pub slave: u16,
    /// Decimal, or hexadecimal with an explicit 0x prefix.
    pub index: String,
    pub subindex: u8,
}

impl UploadQuery {
    pub fn address(&self) -> Result<SdoAddress, SdoError> {
        if self.device.is_empty() {
            return Err(SdoError::Invalid("empty device name".into()));
        }
        let index = if let Some(hex) = self.index.strip_prefix("0x") {
            u16::from_str_radix(hex, 16)
        } else {
            self.index.parse::<u16>()
        }
        .map_err(|_| {
            SdoError::Invalid("index must be a u16 (decimal or 0x-prefixed hex)".into())
        })?;
        Ok(SdoAddress {
            slave: self.slave,
            index,
            subindex: self.subindex,
        })
    }
}

pub fn error_response(error: SdoError) -> (u16, serde_json::Value) {
    (
        error.status_code(),
        serde_json::json!({"error": error.to_string(), "sdo_error": error}),
    )
}

pub async fn upload(handle: &ProgramHandle, query: UploadQuery) -> (u16, serde_json::Value) {
    let address = match query.address() {
        Ok(a) => a,
        Err(e) => return error_response(e),
    };
    match handle.sdo_upload(&query.device, address).await {
        Ok(value) => (
            200,
            serde_json::json!({"device": query.device, "upload": value}),
        ),
        Err(e) => error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_radix_and_bounds_are_explicit() {
        let mut q = UploadQuery {
            device: "bus".into(),
            slave: 1,
            index: "0x2008".into(),
            subindex: 255,
        };
        assert_eq!(q.address().unwrap().index, 8200);
        q.index = "2008".into();
        assert_eq!(q.address().unwrap().index, 2008);
        for bad in ["65536", "0x10000", "-1", "200g", "", "0x"] {
            q.index = bad.into();
            assert_eq!(q.address().unwrap_err().status_code(), 400);
        }
    }
    #[test]
    fn errors_preserve_machine_readable_classification() {
        let (status, body) = error_response(SdoError::Busy);
        assert_eq!(status, 409);
        assert_eq!(body["sdo_error"]["kind"], "busy");
        assert_eq!(error_response(SdoError::Timeout).0, 504);
        assert!(serde_json::from_value::<UploadQuery>(serde_json::json!({"device":"bus", "slave":1, "index":"0x2008", "subindex":1, "value":42})).is_err());
    }
}
