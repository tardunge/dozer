use std::borrow::Cow;

use dozer_types::{
    node::{NodeHandle, OpIdentifier},
    types::{IndexDefinition, Record, Schema},
};

use crate::errors::StorageError;

pub enum Encoded<'a> {
    U8([u8; 1]),
    U8x4([u8; 4]),
    U8x8([u8; 8]),
    U8x16([u8; 16]),
    Vec(Vec<u8>),
    Borrowed(&'a [u8]),
}

impl<'a> AsRef<[u8]> for Encoded<'a> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::U8(v) => v.as_slice(),
            Self::U8x4(v) => v.as_slice(),
            Self::U8x8(v) => v.as_slice(),
            Self::U8x16(v) => v.as_slice(),
            Self::Vec(v) => v.as_slice(),
            Self::Borrowed(v) => v,
        }
    }
}

pub trait Encode {
    fn encode(&self) -> Result<Encoded, StorageError>;
}

pub trait Decode: ToOwned {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LmdbValType {
    U32,
    #[cfg(target_pointer_width = "64")]
    U64,
    FixedSizeOtherThanU32OrUsize,
    VariableSize,
}

/// A trait for types that can be used as keys in LMDB.
///
/// # Safety
///
/// This trait is `unsafe` because `TYPE` must match the implementation of `encode`.
///
/// # Note
///
/// The implementation for `u32` and `u64` has a caveat: The values are encoded in big-endian but compared in native-endian.
pub unsafe trait LmdbKey: Encode {
    const TYPE: LmdbValType;
}

pub trait LmdbValue: Encode + Decode {}

impl<T: Encode + Decode + ?Sized> LmdbValue for T {}

pub trait LmdbDupValue: LmdbKey + LmdbValue {}

impl<T: LmdbKey + LmdbValue + ?Sized> LmdbDupValue for T {}

impl Encode for u8 {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::U8([*self]))
    }
}

impl Decode for u8 {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Owned(bytes[0]))
    }
}

unsafe impl LmdbKey for u8 {
    const TYPE: LmdbValType = LmdbValType::FixedSizeOtherThanU32OrUsize;
}

impl Encode for u32 {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::U8x4(self.to_be_bytes()))
    }
}

impl Decode for u32 {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Owned(u32::from_be_bytes(bytes.try_into().unwrap())))
    }
}

unsafe impl LmdbKey for u32 {
    const TYPE: LmdbValType = LmdbValType::U32;
}

impl Encode for u64 {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::U8x8(self.to_be_bytes()))
    }
}

impl Decode for u64 {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Owned(u64::from_be_bytes(bytes.try_into().unwrap())))
    }
}

unsafe impl LmdbKey for u64 {
    #[cfg(target_pointer_width = "64")]
    const TYPE: LmdbValType = LmdbValType::U64;
    #[cfg(not(target_pointer_width = "64"))]
    const TYPE: LmdbValType = LmdbValType::FixedSizeOtherThanU32OrUsize;
}

impl Encode for [u8] {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::Borrowed(self))
    }
}

impl Decode for [u8] {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Borrowed(bytes))
    }
}

unsafe impl LmdbKey for [u8] {
    const TYPE: LmdbValType = LmdbValType::VariableSize;
}

impl Encode for str {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::Borrowed(self.as_bytes()))
    }
}

impl Decode for str {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Borrowed(std::str::from_utf8(bytes).unwrap()))
    }
}

unsafe impl LmdbKey for str {
    const TYPE: LmdbValType = LmdbValType::VariableSize;
}

impl Encode for Record {
    fn encode(&self) -> Result<Encoded, StorageError> {
        dozer_types::bincode::serialize(self)
            .map(Encoded::Vec)
            .map_err(|e| StorageError::SerializationError {
                typ: "Record",
                reason: Box::new(e),
            })
    }
}

impl Decode for Record {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        dozer_types::bincode::deserialize(bytes)
            .map(Cow::Owned)
            .map_err(|e| StorageError::DeserializationError {
                typ: "Record",
                reason: Box::new(e),
            })
    }
}

unsafe impl LmdbKey for Record {
    const TYPE: LmdbValType = LmdbValType::VariableSize;
}

impl Encode for (Schema, Vec<IndexDefinition>) {
    fn encode(&self) -> Result<Encoded, StorageError> {
        dozer_types::bincode::serialize(self)
            .map(Encoded::Vec)
            .map_err(|e| StorageError::SerializationError {
                typ: "(Schema, Vec<IndexDefinition>)",
                reason: Box::new(e),
            })
    }
}

impl Decode for (Schema, Vec<IndexDefinition>) {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        dozer_types::bincode::deserialize(bytes)
            .map(Cow::Owned)
            .map_err(|e| StorageError::DeserializationError {
                typ: "(Schema, Vec<IndexDefinition>)",
                reason: Box::new(e),
            })
    }
}

impl Encode for NodeHandle {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::Vec(self.to_bytes()))
    }
}

impl Decode for NodeHandle {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Owned(NodeHandle::from_bytes(bytes)))
    }
}

unsafe impl LmdbKey for NodeHandle {
    const TYPE: LmdbValType = LmdbValType::VariableSize;
}

impl Encode for OpIdentifier {
    fn encode(&self) -> Result<Encoded, StorageError> {
        Ok(Encoded::U8x16(self.to_bytes()))
    }
}

impl Decode for OpIdentifier {
    fn decode(bytes: &[u8]) -> Result<Cow<Self>, StorageError> {
        Ok(Cow::Owned(OpIdentifier::from_bytes(
            bytes.try_into().unwrap(),
        )))
    }
}

unsafe impl LmdbKey for OpIdentifier {
    const TYPE: LmdbValType = LmdbValType::FixedSizeOtherThanU32OrUsize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lmdb_key_types() {
        assert_eq!(u8::TYPE, LmdbValType::FixedSizeOtherThanU32OrUsize);
        assert_eq!(u32::TYPE, LmdbValType::U32);
        assert_eq!(u64::TYPE, LmdbValType::U64);
        assert_eq!(<[u8]>::TYPE, LmdbValType::VariableSize);
    }
}
