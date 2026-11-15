//! MSC4512: the paths an appservice claims and the requests forwarded to it.
//!
//! The claim grammar is shared with `fed_proxy`; the send goes through the
//! appservice's own client pool, so a proxy URL takes the forms `url` does.

use bytes::Bytes;
use http::{
	Error as HttpError, HeaderValue, Request as HttpRequest, Uri, header::AUTHORIZATION,
	uri::PathAndQuery,
};
use reqwest::{Request, Response};
use tuwunel_core::{Result, err, implement, warn};

use super::RegistrationInfo;

/// MSC4512: the API a proxied path belongs to.
///
/// It decides how the forwarded request is authenticated and which identity
/// header carries the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Root {
	/// `/_matrix/client/`, authenticated by an access token.
	Client,

	/// `/_matrix/federation/`, authenticated by a server signature.
	Federation,
}

/// MSC4512: splits a raw request path into its API and the remainder after
/// the version.
///
/// The version is one `v<x>` segment or the two segments `unstable/<ns>`.
/// The remainder is what a proxy claim is matched against.
#[must_use]
pub fn split_path(path: &str) -> Option<(Root, &str)> {
	let (root, versioned) = path
		.strip_prefix("/_matrix/client/")
		.map(|rest| (Root::Client, rest))
		.or_else(|| {
			path.strip_prefix("/_matrix/federation/")
				.map(|rest| (Root::Federation, rest))
		})?;

	let (version, rest) = versioned.split_once('/')?;
	let remainder = match version {
		| "unstable" => rest
			.split_once('/')
			.filter(|(namespace, _)| !namespace.is_empty())
			.map(|(_, remainder)| remainder)?,
		| _ if version.len() > 1 && version.starts_with('v') => rest,
		| _ => return None,
	};

	Some((root, remainder))
}

/// MSC4512: forwards a proxied request to the appservice holding the claim.
///
/// The request's URI is the path and query to append to the claim's URL; its
/// headers are already filtered. The `hs_token` goes in as a bearer token,
/// never in the query. The response comes back unread, so its body can
/// stream to the caller.
#[implement(super::Service)]
#[tracing::instrument(name = "proxy", level = "debug", skip_all)]
pub async fn send_proxied(
	&self,
	appservice: &RegistrationInfo,
	request: HttpRequest<Bytes>,
) -> Result<Response> {
	let id = &appservice.registration.id;
	let url = appservice
		.proxy_url()
		.ok_or_else(|| err!("Appservice {id} claims no proxy prefix"))?;

	let (base, client) = self.transport(url)?;
	let request = with_credentials(request, base, &appservice.registration.hs_token)?;
	let request = Request::try_from(request)?;

	client.execute(request).await.map_err(|e| {
		let e = e.without_url();

		warn!(appservice = %id, error = ?e, "Could not forward a proxied request");
		e.into()
	})
}

fn with_credentials(
	request: HttpRequest<Bytes>,
	base: &str,
	hs_token: &str,
) -> Result<HttpRequest<Bytes>> {
	let (mut parts, body) = request.into_parts();
	let path = parts
		.uri
		.path_and_query()
		.map_or("/", PathAndQuery::as_str);

	parts.uri = Uri::try_from(format!("{base}{path}")).map_err(HttpError::from)?;
	parts
		.headers
		.insert(AUTHORIZATION, HeaderValue::try_from(format!("Bearer {hs_token}"))?);

	Ok(HttpRequest::from_parts(parts, body))
}
