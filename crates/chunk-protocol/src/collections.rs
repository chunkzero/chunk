use crate::{Decode, Encode, Error, Result, VarInt};

/// A `VarInt`-prefixed sequence with a maximum element count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedArray<T, const N: usize>(Vec<T>);

impl<T, const N: usize> BoundedArray<T, N> {
    /// # Errors
    /// Rejects more than N elements.
    pub fn new(values: Vec<T>) -> Result<Self> {
        if values.len() > N {
            return Err(Error::CollectionTooLong);
        }
        Ok(Self(values))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T: Encode, const N: usize> Encode for BoundedArray<T, N> {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        let length = i32::try_from(self.0.len()).map_err(|_| Error::CollectionTooLong)?;
        VarInt(length).encode(output)?;
        for value in &self.0 {
            value.encode(output)?;
        }
        Ok(())
    }
}

impl<T: Decode, const N: usize> Decode for BoundedArray<T, N> {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        let length = usize::try_from(VarInt::decode(input)?.0).map_err(|_| Error::CollectionTooLong)?;
        if length > N {
            return Err(Error::CollectionTooLong);
        }
        // Allocate only as elements are successfully decoded, never from an untrusted count.
        let mut values = Vec::new();
        for _ in 0..length {
            values.push(T::decode(input)?);
        }
        Ok(Self(values))
    }
}

/// A `VarInt`-prefixed byte array, bounded before allocating.
pub type ByteArray<const N: usize> = BoundedArray<u8, N>;

/// Bytes occupying the remainder of a packet; no length prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemainingBytes<const N: usize>(Vec<u8>);

impl<const N: usize> RemainingBytes<N> {
    /// # Errors
    /// Rejects more than N bytes.
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > N {
            return Err(Error::CollectionTooLong);
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> Encode for RemainingBytes<N> {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        output.extend_from_slice(&self.0);
        Ok(())
    }
}

impl<const N: usize> Decode for RemainingBytes<N> {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        if input.len() > N {
            return Err(Error::CollectionTooLong);
        }
        let value = Self(input.to_vec());
        *input = &[];
        Ok(value)
    }
}
