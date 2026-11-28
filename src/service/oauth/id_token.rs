//! Checks on the ID token a provider returns with its access token.

#[cfg(test)]
mod tests;

use std::{slice::from_ref, time::Duration};

use serde::Deserialize;
use tuwunel_core::{jwt::dangerous::insecure_decode_claims, utils::BoolExt};

use super::providers::trim_issuer;

/// What an acceptable token says, from the provider's configuration and the
/// session of the sign-in that asked for it.
pub(super) struct Expected<'a> {
	/// The provider's issuer, trimmed as [`trim_issuer`] does.
	pub(super) issuer: Option<&'a str>,

	/// Our client id at the provider.
	pub(super) client_id: &'a str,

	/// The nonce sent with the authorization request, when one was.
	pub(super) nonce: Option<&'a str>,

	/// The current time since the Unix epoch.
	pub(super) now: Duration,
}

/// The claims of an ID token that are checked.
#[derive(Deserialize)]
struct Claims {
	iss: Option<String>,
	sub: Option<String>,
	aud: Option<Audience>,
	azp: Option<String>,
	exp: Option<f64>,
	iat: Option<f64>,
	nonce: Option<String>,
}

/// The `aud` claim, which names one audience or lists several.
#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
	One(String),
	Many(Vec<String>),
}

/// How long past its expiry a token is still accepted, for clock skew.
const EXPIRY_LEEWAY: Duration = Duration::from_mins(2);

/// Check a provider's ID token against what this sign-in expects of it.
///
/// These are the claim checks OpenID Connect Core 3.1.3.7 requires of an
/// authorization code client, in its order, and the presence of `sub` and
/// `iat`, which Core 2 requires of every ID token. A token that also names
/// another audience passes only when `azp` names this client. The signature is
/// not among the checks: Core 3.1.3.7 lets a client handed the token by the
/// token endpoint itself rely on the endpoint's TLS instead. A refusal names
/// the first claim that failed, or `format` for a token that does not decode.
pub(super) fn check_claims(token: &str, expected: &Expected<'_>) -> Result<(), &'static str> {
	let claims = decode(token).ok_or("format")?;
	let issued = claims
		.iss
		.as_deref()
		.map(trim_issuer)
		.is_some_and(|issuer| Some(issuer) == expected.issuer);

	let identified = claims.sub.is_some();
	let audiences = claims
		.aud
		.as_ref()
		.map_or(&[][..], Audience::as_slice);

	let ours = |audience: &String| audience == expected.client_id;
	let addressed = audiences.iter().any(ours);
	let sole = audiences.iter().all(ours);
	let authorized = claims
		.azp
		.as_deref()
		.map_or(sole, |party| party == expected.client_id);

	let unexpired = claims
		.exp
		.and_then(|expiry| Duration::try_from_secs_f64(expiry).ok())
		.is_some_and(|expiry| expected.now < expiry.saturating_add(EXPIRY_LEEWAY));

	let dated = claims.iat.is_some();
	let answered = expected
		.nonce
		.is_none_or(|nonce| claims.nonce.as_deref() == Some(nonce));

	[
		("iss", issued),
		("sub", identified),
		("aud", addressed),
		("azp", authorized),
		("exp", unexpired),
		("iat", dated),
		("nonce", answered),
	]
	.into_iter()
	.find_map(|(claim, accepted)| accepted.is_false().then_some(claim))
	.map_or(Ok(()), Err)
}

impl Audience {
	fn as_slice(&self) -> &[String] {
		match self {
			| Self::One(audience) => from_ref(audience),
			| Self::Many(audiences) => audiences,
		}
	}
}

/// Read the claims of a compact JWS, whose three segments are its header,
/// claims and signature.
fn decode(token: &str) -> Option<Claims> {
	token
		.split('.')
		.count()
		.eq(&3)
		.then(|| insecure_decode_claims(token).ok())
		.flatten()
}
