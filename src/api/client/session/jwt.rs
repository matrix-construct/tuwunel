use std::str::FromStr;

use jwt::{Algorithm, DecodingKey, Validation, decode};
use ruma::{
	OwnedUserId, UserId,
	api::client::session::login::v3::{Request, Token},
};
use serde::Deserialize;
use tuwunel_core::{Err, Result, at, config::JwtConfig, debug, err, jwt, warn};
use tuwunel_service::Services;

use crate::Ruma;

#[derive(Debug, Deserialize)]
struct Claim {
	/// Subject is the localpart of the User MXID
	sub: String,
}

pub(super) async fn handle_login(
	services: &Services,
	_body: &Ruma<Request>,
	info: &Token,
) -> Result<OwnedUserId> {
	let user_id = validate_user(services, &info.token)?;
	if !services.users.exists(&user_id).await {
		let config = &services.config.jwt;
		if !config.register_user {
			return Err!(Request(NotFound("User {user_id} is not registered on this server.")));
		}

		services
			.users
			.create(&user_id, Some("*"), Some("jwt"))
			.await?;
	}

	Ok(user_id)
}

pub(crate) fn validate_user(services: &Services, token: &str) -> Result<OwnedUserId> {
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
	let verifier = init_verifier(config)?;
	let validator = init_validator(config)?;
	decode::<Claim>(token, &verifier, &validator)
		.map(|decoded| (decoded.header, decoded.claims))
		.inspect(|(head, claim)| debug!(?head, ?claim, "JWT token decoded"))
		.map_err(|e| err!(Request(Forbidden("Invalid JWT token: {e}"))))
		.map(at!(1))
}

fn init_verifier(config: &JwtConfig) -> Result<DecodingKey> {
	let key = &config.key;
	let format = config.format.to_uppercase();

	Ok(match format.as_str() {
		| "HMAC" => DecodingKey::from_secret(key.as_bytes()),

		| "HMACB64" => DecodingKey::from_base64_secret(key.as_str())
			.map_err(|e| err!(Config("jwt.key", "JWT key is not valid base64: {e}")))?,

		| "ECDSA" => DecodingKey::from_ec_pem(key.as_bytes())
			.map_err(|e| err!(Config("jwt.key", "JWT key is not valid ECDSA PEM: {e}")))?,

		| "EDDSA" => DecodingKey::from_ed_pem(key.as_bytes())
			.map_err(|e| err!(Config("jwt.key", "JWT key is not valid EDDSA PEM: {e}")))?,

		| _ => return Err!(Config("jwt.format", "Key format {format:?} is not supported.")),
	})
}

fn init_validator(config: &JwtConfig) -> Result<Validation> {
	let alg = config.algorithm.as_str();
	let alg = Algorithm::from_str(alg).map_err(|e| {
		err!(Config("jwt.algorithm", "JWT algorithm is not recognized or configured {e}"))
	})?;

	let mut validator = Validation::new(alg);
	let mut required_spec_claims: Vec<_> = ["sub"].into();

	validator.validate_exp = config.validate_exp;
	if config.require_exp {
		required_spec_claims.push("exp");
	}

	validator.validate_nbf = config.validate_nbf;
	if config.require_nbf {
		required_spec_claims.push("nbf");
	}

	if !config.audience.is_empty() {
		required_spec_claims.push("aud");
		validator.set_audience(&config.audience);
	}

	if !config.issuer.is_empty() {
		required_spec_claims.push("iss");
		validator.set_issuer(&config.issuer);
	}

	#[expect(deprecated)]
	if cfg!(debug_assertions) && !config.validate_signature {
		warn!("JWT signature validation is disabled!");
		validator.insecure_disable_signature_validation();
	}

	validator.set_required_spec_claims(&required_spec_claims);
	debug!(?validator, "JWT configured");

	Ok(validator)
}
