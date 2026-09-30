//! Turns the JWT key settings into signing and verifying keys and an algorithm.
//!
//! Every use of the key, format and algorithm settings goes through these
//! conversions. The startup and reload check parses the format alone, so an
//! unknown format is refused before the first login while a key that does not
//! decode fails at login.

use super::JwtConfig;
use crate::{
	Err, Error, Result, err, implement,
	jwt::{Algorithm, DecodingKey, EncodingKey, errors::Error as JwtError},
};

#[derive(Clone, Copy)]
pub(super) enum KeyFormat {
	Hmac,
	B64Hmac,
	Ecdsa,
	Eddsa,
}

/// Spellings of the `format` directive, matched without regard to case.
///
/// B64HMAC is the documented spelling; HMACB64 was the only one accepted before
/// it and is kept for existing configurations.
const KEY_FORMATS: [(&str, KeyFormat); 5] = [
	("HMAC", KeyFormat::Hmac),
	("B64HMAC", KeyFormat::B64Hmac),
	("HMACB64", KeyFormat::B64Hmac),
	("ECDSA", KeyFormat::Ecdsa),
	("EDDSA", KeyFormat::Eddsa),
];

/// Decodes the configured key for verifying tokens.
///
/// Fails when the format is not one of the supported spellings or when the key
/// does not parse in that format.
#[implement(JwtConfig)]
pub fn decoding_key(&self) -> Result<DecodingKey> {
	let key = self.key.as_str();

	match self.key_format()? {
		| KeyFormat::Hmac => Ok(DecodingKey::from_secret(key.as_bytes())),
		| KeyFormat::B64Hmac =>
			DecodingKey::from_base64_secret(key).map_err(invalid_key("base64")),
		| KeyFormat::Ecdsa =>
			DecodingKey::from_ec_pem(key.as_bytes()).map_err(invalid_key("ECDSA PEM")),
		| KeyFormat::Eddsa =>
			DecodingKey::from_ed_pem(key.as_bytes()).map_err(invalid_key("EDDSA PEM")),
	}
}

/// Decodes the configured key for signing tokens.
///
/// Only the shared-secret formats can sign. An ECDSA or EDDSA key is the
/// issuer's public key, which verifies but never signs.
#[implement(JwtConfig)]
pub fn encoding_key(&self) -> Result<EncodingKey> {
	let key = self.key.as_str();
	let format = self.format.as_str();

	match self.key_format()? {
		| KeyFormat::Hmac => Ok(EncodingKey::from_secret(key.as_bytes())),
		| KeyFormat::B64Hmac =>
			EncodingKey::from_base64_secret(key).map_err(invalid_key("base64")),
		| KeyFormat::Ecdsa | KeyFormat::Eddsa => Err!(Config(
			"jwt.format",
			"An {format} key is a public key; signing needs the HMAC or B64HMAC format."
		)),
	}
}

/// Parses the configured signature algorithm.
///
/// Fails with a `jwt.algorithm` configuration error naming the value when the
/// algorithm name is not recognized.
#[implement(JwtConfig)]
pub fn algorithm(&self) -> Result<Algorithm> {
	let algorithm = self.algorithm.as_str();

	algorithm.parse().map_err(|e| {
		err!(Config("jwt.algorithm", "JWT algorithm {algorithm:?} is not recognized: {e}"))
	})
}

#[implement(JwtConfig)]
pub(super) fn key_format(&self) -> Result<KeyFormat> {
	let format = self.format.as_str();

	KEY_FORMATS
		.iter()
		.find(|(name, _)| name.eq_ignore_ascii_case(format))
		.map(|&(_, kind)| kind)
		.ok_or_else(|| {
			err!(Config(
				"jwt.format",
				"Key format {format:?} is not supported; use HMAC, B64HMAC, ECDSA or EDDSA."
			))
		})
}

fn invalid_key(encoding: &'static str) -> impl FnOnce(JwtError) -> Error {
	move |e| err!(Config("jwt.key", "JWT key is not valid {encoding}: {e}"))
}
