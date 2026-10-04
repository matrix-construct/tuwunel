use jwt::{TokenData, Validation, dangerous::insecure_decode, decode};
use ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use tuwunel_core::{Err, Result, at, config::JwtConfig, debug, err, jwt, utils::BoolExt, warn};
use tuwunel_service::Services;

#[derive(Debug, Deserialize)]
struct Claim {
	/// Subject is the localpart of the User MXID
	sub: String,
}

/// Validates a login token and resolves its local user identity.
///
/// The configured JWT rules validate the token before its subject becomes a user ID.
pub fn validate_user(services: &Services, token: &str) -> Result<OwnedUserId> {
	let config = &services.config.jwt;

	if !config.enable {
		return Err!(Request(Unauthorized("JWT login is not enabled.")));
	}

	let claim = validate(config, token)?;
	let local = claim.sub.to_lowercase();
	let server = &services.server.name;
	let user_id = UserId::parse_with_server_name(local, server).map_err(|e| {
		err!(Request(InvalidUsername("JWT subject is not a valid user MXID: {e}")))
	})?;

	Ok(user_id)
}

fn validate(config: &JwtConfig, token: &str) -> Result<Claim> {
	let token_data = if cfg!(debug_assertions) && !config.validate_signature {
		warn!("JWT signature validation is disabled!");
		insecure_decode(token)
	} else {
		let verifier = config.decoding_key()?;
		let validator = init_validator(config)?;

		decode(token, &verifier, &validator)
	};

	token_data
		.map(|decoded: TokenData<Claim>| (decoded.header, decoded.claims))
		.inspect(|(head, claim)| debug!(?head, ?claim, "JWT token decoded"))
		.map_err(|e| err!(Request(Forbidden("Invalid JWT token: {e}"))))
		.map(at!(1))
}

fn init_validator(config: &JwtConfig) -> Result<Validation> {
	let has_audience = config.audience.is_empty().is_false();
	let has_issuer = config.issuer.is_empty().is_false();
	let required = [
		Some("sub"),
		config.require_exp.then_some("exp"),
		config.require_nbf.then_some("nbf"),
		has_audience.then_some("aud"),
		has_issuer.then_some("iss"),
	];

	let validator = Validation {
		required_spec_claims: required
			.into_iter()
			.flatten()
			.map(ToOwned::to_owned)
			.collect(),
		validate_exp: config.validate_exp,
		validate_nbf: config.validate_nbf,
		aud: has_audience.then(|| config.audience.iter().cloned().collect()),
		iss: has_issuer.then(|| config.issuer.iter().cloned().collect()),
		..Validation::new(config.algorithm()?)
	};

	debug!(?validator, "JWT configured");

	Ok(validator)
}
