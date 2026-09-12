use std::fmt::Display;

use argon2::{
	Algorithm, Argon2, Error as Argon2Error, Params, PasswordHasher, PasswordVerifier, Version,
	password_hash::phc::Salt,
};
use rand::random;
use smallstr::SmallString;

use crate::{Error, Result, err, format_small_string, implement};

/// A PHC-formatted Argon2id password hash.
///
/// The inline budget fits the 97 bytes a hash takes at any OWASP recommended
/// cost, so a hash spills to the heap only past an unusually large `m_cost`.
pub type PhcString = SmallString<[u8; 112]>;

/// Argon2id cost parameters for hashing a new password.
///
/// Every hash records the values it was produced with, so a change takes
/// effect on new hashes only. Each hashing operation holds `m_cost` KiB for
/// its duration, which dominates the server's cost when several run at once.
#[derive(Clone, Copy, Debug)]
pub struct Cost {
	/// Size of the working buffer, in 1 KiB blocks.
	pub m_cost: u32,

	/// Number of passes made over the working buffer.
	pub t_cost: u32,

	/// Number of lanes the working buffer is divided into.
	pub p_cost: u32,
}

/// Hashes a plaintext password with Argon2id at the given cost.
///
/// A fresh random salt is generated for every call. The result is a
/// PHC-formatted string containing the salt and parameters needed for
/// verification.
pub fn password(password: &str, cost: Cost) -> Result<PhcString> {
	let salt: [u8; Salt::RECOMMENDED_LENGTH] = random();

	hasher(cost)
		.map_err(map_err)?
		.hash_password_with_salt(password.as_bytes(), &salt)
		.map(|hash| format_small_string!("{hash}"))
		.map_err(map_err)
}

/// Verifies a plaintext password against an encoded Argon2 password hash.
///
/// Cost parameters come from the encoded hash, so a hash written under any
/// cost still verifies. Malformed hashes and password mismatches return an
/// error.
pub fn verify_password(password: &str, password_hash: &str) -> Result {
	Argon2::default()
		.verify_password(password.as_bytes(), password_hash)
		.map_err(map_err)
}

/// Rejects a cost Argon2id cannot accept.
///
/// The parameters are interdependent: `m_cost` has a floor of eight blocks and
/// must be at least eight times `p_cost`, and `t_cost` has a floor of one. The
/// crate's error names the parameter at fault.
#[implement(Cost)]
pub fn check(self) -> Result<(), Argon2Error> { hasher(self).map(|_| ()) }

/// Whether the cost is at least as strong as the weakest OWASP Argon2id
/// recommendation.
///
/// The recommended pairs of memory and passes run from (47104, 1) down to
/// (7168, 5) at a roughly constant product, so a cost qualifies when its
/// memory is at least 7168 blocks and its memory times its passes at least
/// matches that last pair. Lanes are left out, since `p_cost` divides that
/// work rather than adding to it.
#[implement(Cost)]
#[must_use]
pub fn is_recommended(self) -> bool {
	const MIN_M_COST: u64 = 7168;
	const MIN_PRODUCT: u64 = MIN_M_COST * 5;

	let m_cost = u64::from(self.m_cost);
	let product = m_cost.saturating_mul(u64::from(self.t_cost));

	m_cost >= MIN_M_COST && product >= MIN_PRODUCT
}

fn hasher(Cost { m_cost, t_cost, p_cost }: Cost) -> Result<Argon2<'static>, Argon2Error> {
	let out_len: Option<usize> = None;

	Params::new(m_cost, t_cost, p_cost, out_len)
		.map(|params| Argon2::new(Algorithm::Argon2id, Version::default(), params))
}

fn map_err<E: Display>(e: E) -> Error { err!("{e}") }

#[cfg(test)]
mod tests {
	use argon2::Params;

	use super::{Cost, password, verify_password};

	// The OWASP cost the shipped configuration also defaults to.
	const COST: Cost = Cost {
		m_cost: Params::DEFAULT_M_COST,
		t_cost: Params::DEFAULT_T_COST,
		p_cost: Params::DEFAULT_P_COST,
	};

	#[test]
	fn password_hash_and_verify() {
		let preimage = "temp123";
		let digest = password(preimage, COST).expect("digest");

		verify_password(preimage, &digest).expect("verified");
	}

	#[test]
	fn the_owasp_pairs_are_recommended_and_weaker_costs_are_not() {
		let recommended = [(47104, 1), (19456, 2), (12288, 3), (9216, 4), (7168, 5)];
		let weaker = [(19456, 1), (7168, 4), (4096, 9), (64, 1)];
		let cost = |(m_cost, t_cost)| Cost { m_cost, t_cost, p_cost: 1 };

		for pair in recommended {
			assert!(cost(pair).is_recommended(), "{pair:?}");
		}

		for pair in weaker {
			assert!(!cost(pair).is_recommended(), "{pair:?}");
		}
	}

	#[test]
	#[should_panic(expected = "unverified")]
	fn password_hash_and_verify_fail() {
		let preimage = "temp123";
		let fakeimage = "temp321";
		let digest = password(preimage, COST).expect("digest");

		verify_password(fakeimage, &digest).expect("unverified");
	}
}
