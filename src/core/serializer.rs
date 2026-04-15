use serde::Serialize;
use serde_json::Value;
use sha3::{Digest, Keccak256};

use super::types::{CoreError, SerializationError};

/// A serializer that produces deterministic, canonical JSON for Ethereum-related data.
///
/// Canonical JSON ensures that the same data structure always results in the exact same
/// byte sequence, which is essential for signature verification.
pub struct CanonicalSerializer;

impl CanonicalSerializer {
    /// Serializes an object to a canonical JSON byte sequence.
    ///
    /// Rejects floating point numbers as they are non-deterministic across platforms.
    pub fn serialize<T: Serialize>(obj: &T) -> Result<Vec<u8>, CoreError> {
        let value = serde_json::to_value(obj)
            .map_err(|_| CoreError::InvalidSerialization(SerializationError::ConvertToValue))?;
        let canonical = Self::canonicalize(value)?;
        serde_json::to_vec(&canonical)
            .map_err(|_| CoreError::InvalidSerialization(SerializationError::SerializeCanonical))
    }

    /// Computes the Keccak256 hash of the canonical JSON representation of an object.
    pub fn hash<T: Serialize>(obj: &T) -> Result<Vec<u8>, CoreError> {
        let bytes = Self::serialize(obj)?;
        Ok(Keccak256::digest(bytes).to_vec())
    }

    /// Verifies that serialization is deterministic by running it multiple times.
    pub fn verify_determinism<T: Serialize>(obj: &T, iterations: usize) -> Result<bool, CoreError> {
        let mut first = None;
        for _ in 0..iterations {
            let serialized = Self::serialize(obj)?;
            match &first {
                Some(existing) if existing != &serialized => return Ok(false),
                None => first = Some(serialized),
                _ => {}
            }
        }

        Ok(true)
    }

    fn canonicalize(mut value: Value) -> Result<Value, CoreError> {
        match value {
            Value::Object(ref mut map) => {
                for (_, v) in map.iter_mut() {
                    let inner = std::mem::replace(v, Value::Null);
                    *v = Self::canonicalize(inner)?;
                }
                Ok(value)
            }
            Value::Array(ref mut values) => {
                for v in values.iter_mut() {
                    let inner = std::mem::replace(v, Value::Null);
                    *v = Self::canonicalize(inner)?;
                }
                Ok(value)
            }
            Value::Number(number) => {
                if number.as_i64().is_none() && number.as_u64().is_none() {
                    return Err(CoreError::InvalidSerialization(
                        SerializationError::FloatingPointUnsupported,
                    ));
                }
                Ok(Value::Number(number))
            }
            other => Ok(other),
        }
    }
}
