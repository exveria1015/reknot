//! Shared wire protocol source; no Rust object is passed through the native ABI.
use reknot::{InitPayload, ServiceError, ServiceKey, ServiceRequirement};
pub const PRIMARY: ServiceKey = ServiceKey::new(0x434f_4e54, 1);
pub const COUNTER: ServiceKey = ServiceKey::new(0x434f_554e, 1);
pub const FORMAT: ServiceKey = ServiceKey::new(0x464d_5420, 1);
pub const REQUIREMENTS: &[ServiceRequirement] = &[
    ServiceRequirement::new(1, COUNTER),
    ServiceRequirement::new(2, FORMAT),
];
pub struct Config {
    pub step: u32,
}
impl InitPayload for Config {
    const SCHEMA: ServiceKey = ServiceKey::new(0x4346_4720, 1);
    fn encode(&self) -> Result<Vec<u8>, ServiceError> {
        let mut bytes = b"CFG1".to_vec();
        bytes.extend_from_slice(&self.step.to_le_bytes());
        Ok(bytes)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ServiceError> {
        if bytes.len() != 8 || &bytes[..4] != b"CFG1" {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let step = u32::from_le_bytes(
            bytes[4..]
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        if !(1..=100).contains(&step) {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(Self { step })
    }
}
