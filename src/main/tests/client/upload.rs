//! Raw-body media requests for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use reqwest::{Method, Response, header::CONTENT_TYPE};
use tuwunel_core::{Result, implement, ruma::MxcUri};

use super::client::Client;

/// A URL under the media-API root, from what follows it.
///
/// Uploads are addressed there rather than under the client-API root that
/// `client_url` names.
#[implement(Client, params = "<'_>")]
pub(crate) fn media_url(&self, rest: &str) -> String {
	format!("{}/_matrix/media/{rest}", self.base)
}

/// Send a file to an absolute URL with any method as this user and keep the
/// raw reply.
///
/// The bytes twin of `request`, for the routes that take a file rather than
/// JSON; the status stays the caller's to judge.
#[implement(Client, params = "<'_>")]
pub(crate) async fn upload(
	&self,
	method: Method,
	url: &str,
	content_type: &str,
	file: &'static [u8],
) -> Result<Response> {
	self.services
		.client
		.clients
		.default
		.request(method, url)
		.bearer_auth(self.token)
		.header(CONTENT_TYPE, content_type)
		.body(file)
		.send()
		.await
		.map_err(Into::into)
}

/// The `{serverName}/{mediaId}` path a content URI names.
///
/// Media routes address a file by these two segments, so a test passes on the
/// URI the server answered with rather than spelling either part.
pub(crate) fn media_path(content_uri: &str) -> Result<String> {
	let mxc = <&MxcUri>::from(content_uri).parts()?;

	Ok(format!("{}/{}", mxc.server_name, mxc.media_id))
}
