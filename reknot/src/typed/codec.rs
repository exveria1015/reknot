use super::Error;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Per-handler byte encoding. The format is part of the checked contract.
/// Metadata and worker checkpoints always use JSON. Neither format shares Rust
/// allocations or borrowed references across a reloadable boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFormat {
    #[default]
    #[serde(rename = "json")]
    Json,
    /// Named MessagePack maps, preserving field names and Serde's human-readable
    /// representation. Enable `typed-binary` in host and logic.
    #[cfg(feature = "typed-binary")]
    #[serde(rename = "messagepack-named-v1")]
    MessagePack,
}
impl WireFormat {
    pub(super) fn is_json(&self) -> bool {
        *self == Self::Json
    }
    pub(super) fn encode<T: Serialize>(self, value: &T, limit: usize) -> Result<Vec<u8>, Error> {
        let mut writer = Bounded {
            bytes: Vec::new(),
            limit,
            full: false,
        };
        let result = match self {
            Self::Json => serde_json::to_writer(&mut writer, value).map_err(|e| e.to_string()),
            #[cfg(feature = "typed-binary")]
            Self::MessagePack => value
                .serialize(
                    &mut rmp_serde::Serializer::new(&mut writer)
                        .with_struct_map()
                        .with_human_readable(),
                )
                .map_err(|e| e.to_string()),
        };
        if writer.full {
            return Err(Error::TooLarge { limit });
        }
        result.map_err(Error::Codec)?;
        Ok(writer.bytes)
    }
    pub(super) fn decode<T: DeserializeOwned>(
        self,
        bytes: &[u8],
        limit: usize,
    ) -> Result<T, Error> {
        if bytes.len() > limit {
            return Err(Error::TooLarge { limit });
        }
        match self {
            Self::Json => serde_json::from_slice(bytes).map_err(|e| Error::Codec(e.to_string())),
            #[cfg(feature = "typed-binary")]
            Self::MessagePack => {
                let mut decoder =
                    rmp_serde::Deserializer::new(std::io::Cursor::new(bytes)).with_human_readable();
                let value =
                    T::deserialize(&mut decoder).map_err(|e| Error::Codec(e.to_string()))?;
                if decoder.get_ref().position() != bytes.len() as u64 {
                    return Err(Error::Codec(
                        "trailing bytes after MessagePack value".into(),
                    ));
                }
                Ok(value)
            }
        }
    }
}

// Bound output while encoding, before allocating an arbitrarily large buffer.
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
    full: bool,
}
impl std::io::Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.full = true;
            return Err(std::io::Error::other("message too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, feature = "typed-binary"))]
mod tests {
    use super::*;

    #[test]
    fn named_fields_survive_declaration_order_changes() {
        #[derive(Serialize)]
        struct Before {
            left: u32,
            right: u32,
        }
        #[derive(Deserialize, Debug, PartialEq)]
        struct After {
            right: u32,
            left: u32,
        }
        let bytes = WireFormat::MessagePack
            .encode(
                &Before {
                    left: 11,
                    right: 29,
                },
                100,
            )
            .unwrap();
        assert_eq!(
            WireFormat::MessagePack
                .decode::<After>(&bytes, 100)
                .unwrap(),
            After {
                right: 29,
                left: 11
            }
        );
    }

    #[test]
    fn malformed_trailing_and_oversized_messages_are_rejected() {
        let format = WireFormat::MessagePack;
        assert!(matches!(
            format.decode::<u64>(&[0xc1], 100),
            Err(Error::Codec(_))
        ));
        assert!(matches!(
            format.decode::<u64>(&[1, 2], 100),
            Err(Error::Codec(_))
        ));
        assert!(matches!(
            format.decode::<String>(&[1, 2], 1),
            Err(Error::TooLarge { limit: 1 })
        ));
        assert!(matches!(
            format.encode(&"long", 2),
            Err(Error::TooLarge { limit: 2 })
        ));
    }
}
