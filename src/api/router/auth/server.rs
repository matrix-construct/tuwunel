use axum::RequestPartsExt;
use axum_extra::{TypedHeader, headers::Authorization, typed_header::TypedHeaderRejectionReason};
use http::uri::PathAndQuery;
use ruma::{
	CanonicalJsonName, CanonicalJsonObject, CanonicalJsonValue,
	api::federation::authentication::XMatrix,
};
use tuwunel_core::{Err, Result, debug_error, err, warn};
use tuwunel_service::{
	Services,
	server_keys::{PubKeyMap, PubKeys},
};

use super::{Auth, Request};

pub(super) async fn auth_server(
	services: &Services,
	request: &mut Request,
	body: Option<&CanonicalJsonValue>,
) -> Result<Auth> {
	type Member = (CanonicalJsonName, CanonicalJsonValue);
	type Object = CanonicalJsonObject;
	type Value = CanonicalJsonValue;

	let x_matrix = parse_x_matrix(request).await?;
	auth_server_checks(services, &x_matrix)?;

	let destination = services.globals.server_name();
	let origin = &x_matrix.origin;
	let signature_uri = request
		.parts
		.uri
		.path_and_query()
		.map(PathAndQuery::as_str)
		.unwrap_or("/");

	let signature: [Member; 1] =
		[(x_matrix.key.as_str().into(), Value::String(x_matrix.sig.to_string()))];

	let signatures: [Member; 1] = [(origin.as_str().into(), Value::Object(signature.into()))];

	let authorization: Object = if let Some(body) = body.cloned() {
		let authorization: [Member; 6] = [
			("content".into(), body),
			("destination".into(), Value::String(destination.into())),
			("method".into(), Value::String(request.parts.method.as_str().into())),
			("origin".into(), Value::String(origin.as_str().into())),
			("signatures".into(), Value::Object(signatures.into())),
			("uri".into(), Value::String(signature_uri.into())),
		];

		authorization.into()
	} else {
		let authorization: [Member; 5] = [
			("destination".into(), Value::String(destination.into())),
			("method".into(), Value::String(request.parts.method.as_str().into())),
			("origin".into(), Value::String(origin.as_str().into())),
			("signatures".into(), Value::Object(signatures.into())),
			("uri".into(), Value::String(signature_uri.into())),
		];

		authorization.into()
	};

	let key = services
		.server_keys
		.get_verify_key(origin, &x_matrix.key)
		.await
		.map_err(|e| {
			err!(Request(Forbidden(debug_warn!("Failed to fetch signing keys: {e}"))))
		})?;

	let keys: PubKeys = [(x_matrix.key.as_str().into(), key.key)].into();
	let keys: PubKeyMap = [(origin.as_str().into(), keys)].into();
	if let Err(e) = ruma::signatures::verify_json(&keys, &authorization) {
		debug_error!("Failed to verify federation request from {origin}: {e}");
		if request.parts.uri.to_string().contains('@') {
			warn!(
				"Request uri contained '@' character. Make sure your reverse proxy gives \
				 tuwunel the raw uri (apache: use nocanon)"
			);
		}

		return Err!(Request(Forbidden("Failed to verify X-Matrix signatures.")));
	}

	Ok(Auth {
		origin: origin.to_owned().into(),
		..Auth::default()
	})
}

fn auth_server_checks(services: &Services, x_matrix: &XMatrix) -> Result {
	if !services.server.config.allow_federation {
		return Err!(Config("allow_federation", "Federation is disabled."));
	}

	let destination = services.globals.server_name();
	if x_matrix.destination.as_deref() != Some(destination) {
		return Err!(Request(Forbidden("Invalid destination.")));
	}

	let origin = &x_matrix.origin;
	if services
		.config
		.is_forbidden_remote_server_name(origin)
	{
		return Err!(Request(Forbidden(debug_warn!(
			"Federation requests from {origin} denied."
		))));
	}

	Ok(())
}

async fn parse_x_matrix(request: &mut Request) -> Result<XMatrix> {
	let TypedHeader(Authorization(x_matrix)) = request
		.parts
		.extract::<TypedHeader<Authorization<XMatrix>>>()
		.await
		.map_err(|e| {
			let msg = match e.reason() {
				| TypedHeaderRejectionReason::Missing => "Missing Authorization header.",
				| TypedHeaderRejectionReason::Error(_) => "Invalid X-Matrix signatures.",
				| _ => "Unknown header-related error",
			};

			err!(Request(Forbidden(debug_warn!("{msg}: {e}"))))
		})?;

	Ok(x_matrix)
}
