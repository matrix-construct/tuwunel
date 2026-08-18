use axum::{Json, body::Body, extract::State, response::IntoResponse};
use http::{HeaderMap, Response, StatusCode, header::AUTHORIZATION};
use serde_json::json;
use tuwunel_core::{Err, Result, info};
use tuwunel_service::oauth::server::DcrRequest;
use url::{Host, Url};

use super::oauth_error;
use crate::ClientIp;

/// RFC 7591 §3.2.2 client-registration error response.
#[derive(Debug)]
enum DcrError {
	Metadata(&'static str),
	RedirectUri(&'static str),
}

impl IntoResponse for DcrError {
	fn into_response(self) -> Response<Body> {
		let (error, description) = match self {
			| Self::Metadata(description) => ("invalid_client_metadata", description),
			| Self::RedirectUri(description) => ("invalid_redirect_uri", description),
		};

		oauth_error(StatusCode::BAD_REQUEST, error, description)
	}
}

pub(crate) async fn registration_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	headers: HeaderMap,
	Json(body): Json<DcrRequest>,
) -> Result<Response<Body>> {
	let oidc = services.oauth.get_server()?;
	services.oauth.check_rate_limit(client)?;
	let config = &services.config;

	// Initial access token (RFC 7591): gate registration when one is configured.
	let required_token = config.oidc_registration_access_token.as_str();
	if !required_token.is_empty() {
		let presented = headers
			.get(AUTHORIZATION)
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.strip_prefix("Bearer "));

		if presented != Some(required_token) {
			return Err!(Request(Forbidden(
				"A valid initial access token is required; this server has \
				 oidc_registration_access_token set"
			)));
		}
	}

	let require_client_uri = config.oidc_registration_require_client_uri;
	if let Err(error) = validate_client_metadata(&body, require_client_uri) {
		return Ok(error.into_response());
	}

	// Redirect-host allowlist (RFC 7591): every redirect_uri host must be listed.
	let allowed = &config.oidc_registration_allowed_redirect_hosts;
	if !allowed.is_empty() {
		let host_allowed = |uri: &String| {
			Url::parse(uri).is_ok_and(|url| {
				url.host_str()
					.is_some_and(|host| allowed.iter().any(|entry| entry.as_str() == host))
			})
		};

		if !body.redirect_uris.iter().all(host_allowed) {
			return Err!(Request(Forbidden(
				"A redirect_uri host is not in the registration allowlist"
			)));
		}
	}

	let reg = oidc.register_client(body).await?;

	info!(
		"OIDC client registered: {} ({})",
		reg.client_id,
		reg.client_name.as_deref().unwrap_or("unnamed")
	);

	Ok((
		StatusCode::CREATED,
		Json(json!({
			"client_id": reg.client_id,
			"client_id_issued_at": reg.registered_at,
			"redirect_uris": reg.redirect_uris,
			"client_name": reg.client_name,
			"client_uri": reg.client_uri,
			"logo_uri": reg.logo_uri,
			"contacts": reg.contacts,
			"token_endpoint_auth_method": reg.token_endpoint_auth_method,
			"grant_types": reg.grant_types,
			"response_types": reg.response_types,
			"application_type": reg.application_type,
			"policy_uri": reg.policy_uri,
			"tos_uri": reg.tos_uri,
			"software_id": reg.software_id,
			"software_version": reg.software_version,
		})),
	)
		.into_response())
}

fn validate_client_metadata(body: &DcrRequest, require_client_uri: bool) -> Result<(), DcrError> {
	if body.redirect_uris.is_empty() {
		return Err(DcrError::RedirectUri("redirect_uris must not be empty"));
	}

	let client_url = match body.client_uri.as_deref() {
		| Some(uri) => Some(parse_https(uri).ok_or(DcrError::Metadata(
			"client_uri must be an https URL with a host and no userinfo",
		))?),
		| None if require_client_uri => return Err(DcrError::Metadata("client_uri is required")),
		| None => None,
	};
	let base = client_url.as_ref().and_then(Url::host_str);

	for uri in [&body.logo_uri, &body.tos_uri, &body.policy_uri]
		.into_iter()
		.flatten()
	{
		match parse_https(uri).as_ref().and_then(Url::host_str) {
			| Some(host) if shares_base(host, base) => {},
			| Some(_) =>
				return Err(DcrError::Metadata("a metadata URI must share the client_uri host")),
			| None => return Err(DcrError::Metadata("a metadata URI must be an https URL")),
		}
	}

	let native = body.application_type.as_deref() == Some("native");
	for uri in &body.redirect_uris {
		validate_redirect_uri(uri, native, base)?;
	}

	if body
		.response_types
		.as_ref()
		.is_some_and(|types| !types.iter().any(|ty| ty == "code"))
	{
		return Err(DcrError::Metadata("response_types must include \"code\""));
	}

	if body.grant_types.as_ref().is_some_and(|types| {
		!types.iter().any(|ty| ty == "authorization_code")
			|| !types.iter().any(|ty| ty == "refresh_token")
	}) {
		return Err(DcrError::Metadata(
			"grant_types must include \"authorization_code\" and \"refresh_token\"",
		));
	}

	Ok(())
}

fn parse_https(uri: &str) -> Option<Url> {
	let url = Url::parse(uri).ok()?;
	let clean = url.scheme() == "https"
		&& url.host().is_some()
		&& url.username().is_empty()
		&& url.password().is_none();

	clean.then_some(url)
}

fn shares_base(host: &str, base: Option<&str>) -> bool {
	base.is_none_or(|base| {
		host == base
			|| host
				.strip_suffix(base)
				.is_some_and(|prefix| prefix.ends_with('.'))
	})
}

fn validate_redirect_uri(uri: &str, native: bool, base: Option<&str>) -> Result<(), DcrError> {
	let url =
		Url::parse(uri).map_err(|_| DcrError::RedirectUri("redirect_uri is not a valid URI"))?;

	// RFC 6749 §3.1.2 / MSC2966: a redirect URI carries no fragment, in all cases.
	if url.fragment().is_some() {
		return Err(DcrError::RedirectUri("redirect_uri must not contain a fragment"));
	}

	match url.scheme() {
		| "https" => validate_web_redirect(&url, base),
		// RFC 8252 §7.3: native loopback http with no registered port.
		| "http" if native && is_loopback(&url) && url.port().is_none() => Ok(()),
		// RFC 8252 §7.1: native private-use scheme, reverse-DNS, no authority.
		| scheme if native && url.host().is_none() && is_reverse_dns(scheme, base) => Ok(()),
		| _ => Err(DcrError::RedirectUri(
			"redirect_uri scheme is not permitted for this application_type",
		)),
	}
}

fn validate_web_redirect(url: &Url, base: Option<&str>) -> Result<(), DcrError> {
	if !url.username().is_empty() || url.password().is_some() {
		return Err(DcrError::RedirectUri("redirect_uri must not contain userinfo"));
	}

	match url.host_str() {
		| Some(host) if shares_base(host, base) => Ok(()),
		| Some(_) =>
			Err(DcrError::RedirectUri("redirect_uri host must share the client_uri host")),
		| None => Err(DcrError::RedirectUri("redirect_uri must have a host")),
	}
}

fn is_loopback(url: &Url) -> bool {
	match url.host() {
		| Some(Host::Domain(domain)) => domain == "localhost",
		| Some(Host::Ipv4(ip)) => ip.is_loopback(),
		| Some(Host::Ipv6(ip)) => ip.is_loopback(),
		| _ => false,
	}
}

fn is_reverse_dns(scheme: &str, base: Option<&str>) -> bool {
	if !scheme.contains('.') {
		return false;
	}

	let Some(host) = base else {
		return true;
	};

	let mut scheme_labels = scheme.split('.');
	host.rsplit('.')
		.all(|label| scheme_labels.next() == Some(label))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request(client_uri: Option<&str>, redirect_uris: &[&str]) -> DcrRequest {
		DcrRequest {
			redirect_uris: redirect_uris
				.iter()
				.copied()
				.map(ToOwned::to_owned)
				.collect(),
			client_name: None,
			client_uri: client_uri.map(ToOwned::to_owned),
			logo_uri: None,
			contacts: Vec::new(),
			token_endpoint_auth_method: None,
			grant_types: None,
			response_types: None,
			application_type: None,
			policy_uri: None,
			tos_uri: None,
			software_id: None,
			software_version: None,
		}
	}

	fn native(mut request: DcrRequest) -> DcrRequest {
		request.application_type = Some("native".to_owned());
		request
	}

	#[test]
	fn web_redirect_rules() {
		let ok = request(Some("https://example.com"), &["https://example.com/cb"]);
		validate_client_metadata(&ok, true).unwrap();

		let subdomain = request(Some("https://example.com"), &["https://app.example.com/cb"]);
		validate_client_metadata(&subdomain, true).unwrap();

		let off_base = request(Some("https://example.com"), &["https://evil.com/cb"]);
		assert!(matches!(
			validate_client_metadata(&off_base, true),
			Err(DcrError::RedirectUri(_))
		));

		let fragment = request(Some("https://example.com"), &["https://example.com/cb#x"]);
		assert!(matches!(
			validate_client_metadata(&fragment, true),
			Err(DcrError::RedirectUri(_))
		));

		let userinfo = request(Some("https://example.com"), &["https://u:p@example.com/cb"]);
		assert!(matches!(
			validate_client_metadata(&userinfo, true),
			Err(DcrError::RedirectUri(_))
		));
	}

	#[test]
	fn client_uri_rules() {
		let missing = request(None, &["https://example.com/cb"]);
		assert!(matches!(validate_client_metadata(&missing, true), Err(DcrError::Metadata(_))));

		let relaxed = request(None, &["https://anywhere.test/cb"]);
		validate_client_metadata(&relaxed, false).unwrap();

		let not_https = request(Some("http://example.com"), &["https://example.com/cb"]);
		assert!(matches!(validate_client_metadata(&not_https, true), Err(DcrError::Metadata(_))));

		let userinfo = request(Some("https://u:p@example.com"), &["https://example.com/cb"]);
		assert!(matches!(validate_client_metadata(&userinfo, true), Err(DcrError::Metadata(_))));

		let empty = request(Some("https://example.com"), &[]);
		assert!(matches!(validate_client_metadata(&empty, true), Err(DcrError::RedirectUri(_))));
	}

	#[test]
	fn native_redirect_rules() {
		let loopback = native(request(Some("https://example.com"), &["http://127.0.0.1/cb"]));
		validate_client_metadata(&loopback, true).unwrap();

		let localhost = native(request(Some("https://example.com"), &["http://localhost/cb"]));
		validate_client_metadata(&localhost, true).unwrap();

		let ipv6 = native(request(Some("https://example.com"), &["http://[::1]/cb"]));
		validate_client_metadata(&ipv6, true).unwrap();

		let ported = native(request(Some("https://example.com"), &["http://127.0.0.1:8080/cb"]));
		assert!(matches!(validate_client_metadata(&ported, true), Err(DcrError::RedirectUri(_))));

		let private = native(request(Some("https://example.com"), &["com.example.app:/cb"]));
		validate_client_metadata(&private, true).unwrap();

		for (client_uri, redirect_uri) in [
			("https://javascript", "javascript:alert(1)"),
			("https://data", "data:text/html,dangerous"),
			("https://vbscript", "vbscript:msgbox(1)"),
			("https://myapp", "myapp:/cb"),
		] {
			let dotless = native(request(Some(client_uri), &[redirect_uri]));
			assert!(matches!(
				validate_client_metadata(&dotless, true),
				Err(DcrError::RedirectUri(_))
			));
		}

		let bad_private = native(request(Some("https://example.com"), &["com.evil.app:/cb"]));
		assert!(matches!(
			validate_client_metadata(&bad_private, true),
			Err(DcrError::RedirectUri(_))
		));

		let claimed = native(request(Some("https://example.com"), &["https://example.com/cb"]));
		validate_client_metadata(&claimed, true).unwrap();

		let web_http = request(Some("https://example.com"), &["http://127.0.0.1/cb"]);
		assert!(matches!(
			validate_client_metadata(&web_http, true),
			Err(DcrError::RedirectUri(_))
		));
	}

	#[test]
	fn grant_and_response_rules() {
		let mut bad_response = request(Some("https://example.com"), &["https://example.com/cb"]);
		bad_response.response_types = Some(vec!["token".to_owned()]);
		assert!(matches!(
			validate_client_metadata(&bad_response, true),
			Err(DcrError::Metadata(_))
		));

		let mut ok_response = request(Some("https://example.com"), &["https://example.com/cb"]);
		ok_response.response_types = Some(vec!["code".to_owned(), "token".to_owned()]);
		validate_client_metadata(&ok_response, true).unwrap();

		let mut bad_grant = request(Some("https://example.com"), &["https://example.com/cb"]);
		bad_grant.grant_types = Some(vec!["authorization_code".to_owned()]);
		assert!(matches!(validate_client_metadata(&bad_grant, true), Err(DcrError::Metadata(_))));

		let mut ok_grant = request(Some("https://example.com"), &["https://example.com/cb"]);
		ok_grant.grant_types = Some(vec![
			"authorization_code".to_owned(),
			"refresh_token".to_owned(),
			"urn:custom".to_owned(),
		]);
		validate_client_metadata(&ok_grant, true).unwrap();
	}

	#[test]
	fn metadata_uri_common_base() {
		let mut off_base = request(Some("https://example.com"), &["https://example.com/cb"]);
		off_base.logo_uri = Some("https://cdn.evil.com/logo.png".to_owned());
		assert!(matches!(validate_client_metadata(&off_base, true), Err(DcrError::Metadata(_))));

		let mut on_base = request(Some("https://example.com"), &["https://example.com/cb"]);
		on_base.logo_uri = Some("https://cdn.example.com/logo.png".to_owned());
		validate_client_metadata(&on_base, true).unwrap();
	}

	#[test]
	fn fragment_rejected_in_all_cases() {
		let loopback = native(request(Some("https://example.com"), &["http://127.0.0.1/cb#x"]));
		assert!(matches!(
			validate_client_metadata(&loopback, true),
			Err(DcrError::RedirectUri(_))
		));

		let private = native(request(Some("https://example.com"), &["com.example.app:/cb#x"]));
		assert!(matches!(
			validate_client_metadata(&private, true),
			Err(DcrError::RedirectUri(_))
		));
	}

	#[test]
	fn error_envelope_is_bad_request() {
		assert_eq!(DcrError::Metadata("x").into_response().status(), StatusCode::BAD_REQUEST);
		assert_eq!(
			DcrError::RedirectUri("x")
				.into_response()
				.status(),
			StatusCode::BAD_REQUEST
		);
	}
}
