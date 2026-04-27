use bytesize::ByteSize;
use serde::{Deserialize, Deserializer, de};

use crate::{Result, at, err};

/// Accepts an integer byte count or a string with SI/IEC suffix (e.g. "24 MiB")
/// and returns a `usize`.
pub fn deserialize_bytesize_usize<'de, D>(de: D) -> Result<usize, D::Error>
where
	D: Deserializer<'de>,
{
	ByteSize::deserialize(de)
		.map(at!(0))
		.map(usize::try_from)?
		.map_err(de::Error::custom)
}

/// Accepts an integer byte count or a string with SI/IEC suffix (e.g. "32 MiB")
/// and returns a `u64`.
pub fn deserialize_bytesize_u64<'de, D>(de: D) -> Result<u64, D::Error>
where
	D: Deserializer<'de>,
{
	ByteSize::deserialize(de).map(at!(0))
}

/// Parse a human-writable size string w/ si-unit suffix into integer
#[inline]
pub fn from_str(str: &str) -> Result<usize> {
	let bytes: ByteSize = str
		.parse()
		.map_err(|e| err!(Arithmetic("Failed to parse byte size: {e}")))?;

	let bytes: usize = bytes
		.as_u64()
		.try_into()
		.map_err(|e| err!(Arithmetic("Failed to convert u64 to usize: {e}")))?;

	Ok(bytes)
}

/// Output a human-readable size string w/ iec-unit suffix
#[inline]
#[must_use]
pub fn pretty(bytes: usize) -> String {
	let bytes: u64 = bytes
		.try_into()
		.expect("failed to convert usize to u64");

	ByteSize::b(bytes).display().iec().to_string()
}

#[inline]
#[must_use]
pub fn increment(old: Option<&[u8]>) -> [u8; 8] {
	old.map_or(0_u64, |bytes| u64_from_bytes(bytes).unwrap_or(0))
		.wrapping_add(1)
		.to_be_bytes()
}

/// Parses 8 big-endian bytes into an u64; panic on invalid argument
#[inline]
#[must_use]
pub fn u64_from_u8(bytes: &[u8]) -> u64 {
	u64_from_bytes(bytes).expect("must slice at least 8 bytes")
}

/// Parses the big-endian bytes into an u64.
#[inline]
pub fn u64_from_bytes(bytes: &[u8]) -> Result<u64> { Ok(u64::from_be_bytes(bytes.try_into()?)) }
