use crate::{Error, Result};

pub trait Encode {
    /// Appends the wire representation.
    ///
    /// # Errors
    /// Returns an error if a field exceeds its wire limits. Output may be
    /// partially written on error and should be discarded.
    fn encode(&self, output: &mut Vec<u8>) -> Result<()>;
}

pub trait Decode: Sized {
    /// Reads one value, advancing the input slice.
    ///
    /// # Errors
    /// Returns an error for incomplete or invalid input. The slice may have
    /// advanced on error; retry incomplete frames at the framing layer.
    fn decode(input: &mut &[u8]) -> Result<Self>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarInt(pub i32);

impl Encode for VarInt {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        let mut value = u32::from_ne_bytes(self.0.to_ne_bytes());
        loop {
            let byte = u8::try_from(value & 0x7f).expect("seven bits fit in a byte");
            value >>= 7;
            output.push(if value == 0 { byte } else { byte | 0x80 });
            if value == 0 {
                return Ok(());
            }
        }
    }
}

impl Decode for VarInt {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        let mut value = 0_u32;
        for index in 0..5 {
            let (&byte, rest) = input.split_first().ok_or(Error::Incomplete)?;
            *input = rest;
            if index == 4 && byte & 0xf0 != 0 {
                return Err(Error::InvalidVarInt);
            }
            value |= u32::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                return Ok(Self(i32::from_ne_bytes(value.to_ne_bytes())));
            }
        }
        Err(Error::InvalidVarInt)
    }
}

macro_rules! fixed_integer {
    ($($ty:ty),+ $(,)?) => {$(
        impl Encode for $ty {
            fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
                output.extend_from_slice(&self.to_be_bytes());
                Ok(())
            }
        }
        impl Decode for $ty {
            fn decode(input: &mut &[u8]) -> Result<Self> {
                let (bytes, rest) = input.split_at_checked(size_of::<Self>()).ok_or(Error::Incomplete)?;
                let value = Self::from_be_bytes(bytes.try_into().expect("fixed integer size"));
                *input = rest;
                Ok(value)
            }
        }
    )+};
}

fixed_integer!(u8, i8, u16, i32, u32, i64, f32, f64);

/// A UUID in network byte order, without a string or length prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uuid(pub [u8; 16]);

impl Encode for Uuid {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        output.extend_from_slice(&self.0);
        Ok(())
    }
}

impl Decode for Uuid {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        let (bytes, rest) = input.split_at_checked(16).ok_or(Error::Incomplete)?;
        *input = rest;
        Ok(Self(bytes.try_into().expect("UUID size")))
    }
}

impl Encode for bool {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        u8::from(*self).encode(output)
    }
}

impl Decode for bool {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        match u8::decode(input)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidBoolean),
        }
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        self.is_some().encode(output)?;
        if let Some(value) = self {
            value.encode(output)?;
        }
        Ok(())
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        if bool::decode(input)? { Ok(Some(T::decode(input)?)) } else { Ok(None) }
    }
}

/// UTF-8 on the wire with a `VarInt` byte length and a UTF-16 length limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McString<const N: usize>(String);

impl<const N: usize> McString<N> {
    /// # Errors
    /// Returns an error if the string exceeds N UTF-16 code units.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() > N.saturating_mul(3) || value.encode_utf16().count() > N {
            return Err(Error::StringTooLong);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> Encode for McString<N> {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        let length = i32::try_from(self.0.len()).map_err(|_| Error::StringTooLong)?;
        VarInt(length).encode(output)?;
        output.extend_from_slice(self.0.as_bytes());
        Ok(())
    }
}

impl<const N: usize> Decode for McString<N> {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        let length = usize::try_from(VarInt::decode(input)?.0).map_err(|_| Error::StringTooLong)?;
        if length > N.saturating_mul(3) {
            return Err(Error::StringTooLong);
        }
        let (bytes, rest) = input.split_at_checked(length).ok_or(Error::Incomplete)?;
        let value = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
        if value.encode_utf16().count() > N {
            return Err(Error::StringTooLong);
        }
        *input = rest;
        Ok(Self(value.to_owned()))
    }
}
