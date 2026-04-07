use ring::{
	rand::SystemRandom,
	signature::{self, EcdsaKeyPair},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, at, err, info, utils};
use tuwunel_database::{Cbor, Deserialized};

use super::Data;

#[derive(Deserialize, Serialize)]
pub(super) struct SigningKey {
	pub(super) key_id: String,
	pub(super) key_der: Vec<u8>,
}

const SIGNING_KEY_DB_KEY: &str = "oidc_signing_key";

pub(super) fn init_signing_key(db: &Data) -> Result<SigningKey> {
	if let Ok(signing_key_data) = db
		.oidc_signingkey
		.get_blocking(SIGNING_KEY_DB_KEY)
		.and_then(|val| val.deserialized::<Cbor<SigningKey>>())
		.map(at!(0))
	{
		info!(
			key_id = ?signing_key_data.key_id,
			"Loaded existing OIDC signing key",
		);

		return Ok(signing_key_data);
	}

	let signing_key_data = generate_signing_key()?;

	db.oidc_signingkey
		.raw_put(SIGNING_KEY_DB_KEY, Cbor(&signing_key_data));

	info!(
		key_id = ?signing_key_data.key_id,
		"Generated new OIDC signing key",
	);

	Ok(signing_key_data)
}

fn generate_signing_key() -> Result<SigningKey> {
	let rng = SystemRandom::new();
	let alg = &signature::ECDSA_P256_SHA256_FIXED_SIGNING;
	let key_id = utils::random_string(16);
	let pkcs8 = EcdsaKeyPair::generate_pkcs8(alg, &rng)
		.map_err(|e| err!(error!("Failed to generate ECDSA key: {e}")))?;

	Ok(SigningKey { key_der: pkcs8.as_ref().to_vec(), key_id })
}
