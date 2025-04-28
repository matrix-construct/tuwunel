use std::sync::OnceLock;

use argon2::{
	Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
	password_hash, password_hash::SaltString,
};

use crate::{Error, Result, err};

const M_COST: u32 = Params::DEFAULT_M_COST; // memory size in 1 KiB blocks
const T_COST: u32 = Params::DEFAULT_T_COST; // nr of iterations
const P_COST: u32 = Params::DEFAULT_P_COST; // parallelism

static ARGON: OnceLock<Argon2<'static>> = OnceLock::new();

fn init_argon() -> Argon2<'static> {
	// 19456 Kib blocks, iterations = 2, parallelism = 1
	// * <https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html#argon2id>
	debug_assert!(M_COST == 19_456, "M_COST default changed");
	debug_assert!(T_COST == 2, "T_COST default changed");
	debug_assert!(P_COST == 1, "P_COST default changed");

	let algorithm = Algorithm::Argon2id;
	let version = Version::default();
	let out_len: Option<usize> = None;
	let params = Params::new(M_COST, T_COST, P_COST, out_len).expect("valid parameters");
	Argon2::new(algorithm, version, params)
}

pub(super) fn password(password: &str) -> Result<String> {
	let salt = SaltString::generate(rand::thread_rng());
	ARGON
		.get_or_init(init_argon)
		.hash_password(password.as_bytes(), &salt)
		.map(|it| it.to_string())
		.map_err(map_err)
}

pub(super) fn verify_password(password: &str, password_hash: &str) -> Result<()> {
	let password_hash = PasswordHash::new(password_hash).map_err(map_err)?;
	ARGON
		.get_or_init(init_argon)
		.verify_password(password.as_bytes(), &password_hash)
		.map_err(map_err)
}

fn map_err(e: password_hash::Error) -> Error {
	err!("{e}")
}

#[cfg(test)]
mod tests {
	#[test]
	fn password_hash_and_verify() {
		use crate::utils::hash;
		let preimage = "temp123";
		let digest = hash::password(preimage).expect("digest");
		hash::verify_password(preimage, &digest).expect("verified");
	}

	#[test]
	#[should_panic(expected = "unverified")]
	fn password_hash_and_verify_fail() {
		use crate::utils::hash;
		let preimage = "temp123";
		let fakeimage = "temp321";
		let digest = hash::password(preimage).expect("digest");
		hash::verify_password(fakeimage, &digest).expect("unverified");
	}
}
