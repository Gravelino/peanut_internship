use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use sha3::{Digest, Keccak256};

use super::types::CoreError;

pub struct CanonicalSerializer;

impl CanonicalSerializer {
    pub fn serialize<T: Serialize>(obj: &T) -> Result<Vec<u8>, CoreError> {
        let value = serde_json::to_value(obj).map_err(|error| CoreError::InvalidSerialization(error.to_string()))?;
        let canonical = Self::canonicalize(value)?;
        serde_json::to_vec(&canonical).map_err(|error| CoreError::InvalidSerialization(error.to_string()))
    }

    pub fn hash<T: Serialize>(obj: &T) -> Result<Vec<u8>, CoreError> {
        let bytes = Self::serialize(obj)?;
        Ok(Keccak256::digest(bytes).to_vec())
    }

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

    fn canonicalize(value: Value) -> Result<Value, CoreError> {
        match value {
            Value::Object(map) => {
                let mut ordered = BTreeMap::new();
                for (key, value) in map {
                    ordered.insert(key, Self::canonicalize(value)?);
                }

                Ok(Value::Object(ordered.into_iter().collect()))
            }
            Value::Array(values) => {
                let mut canonical = Vec::with_capacity(values.len());
                for value in values {
                    canonical.push(Self::canonicalize(value)?);
                }
                Ok(Value::Array(canonical))
            }
            Value::Number(number) => {
                if number.as_i64().is_none() && number.as_u64().is_none() {
                    return Err(CoreError::InvalidSerialization("floating point numbers are not supported in canonical serialization".to_string()));
                }
                Ok(Value::Number(number))
            }
            other => Ok(other),
        }
    }
}
