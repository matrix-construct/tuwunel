use std::{fmt::Debug, time::Duration};

use http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue};
use ruma::{
	Mxc, ServerName, UserId,
	api::{
		OutgoingRequest,
		client::{
			error::ErrorKind::{NotFound, Unrecognized},
			media,
		},
		federation,
		federation::authenticated_media::{Content, FileOrLocation},
	},
};
use tuwunel_core::{
	Err, Error, Result, debug_warn, err, implement,
	utils::content_disposition::make_content_disposition,
};

use super::{Dim, FileMeta};

#[implement(super::Service)]
pub async fn fetch_remote_thumbnail(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	timeout_ms: Duration,
	dim: &Dim,
) -> Result<FileMeta> {
	self.check_fetch_authorized(mxc)?;

	let result = self
		.fetch_thumbnail_authenticated(mxc, user, server, timeout_ms, dim)
		.await;

	if let Err(Error::Request(NotFound, ..)) = &result
		&& self.services.server.config.request_legacy_media
	{
		return self
			.fetch_thumbnail_unauthenticated(mxc, user, server, timeout_ms, dim)
			.await;
	}

	result
}

#[implement(super::Service)]
pub async fn fetch_remote_content(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	timeout_ms: Duration,
) -> Result<FileMeta> {
	self.check_fetch_authorized(mxc)?;

	let result = self
		.fetch_content_authenticated(mxc, user, server, timeout_ms)
		.await;

	if let Err(Error::Request(NotFound, ..)) = &result
		&& self.services.server.config.request_legacy_media
	{
		return self
			.fetch_content_unauthenticated(mxc, user, server, timeout_ms)
			.await;
	}

	result
}

#[implement(super::Service)]
async fn fetch_thumbnail_authenticated(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	timeout_ms: Duration,
	dim: &Dim,
) -> Result<FileMeta> {
	use federation::authenticated_media::get_content_thumbnail::v1::{Request, Response};

	let request = Request {
		media_id: mxc.media_id.into(),
		method: dim.method.clone().into(),
		width: dim.width.into(),
		height: dim.height.into(),
		animated: true.into(),
		timeout_ms,
	};

	let Response { content, .. } = self
		.federation_request(mxc, user, server, request)
		.await?;

	match content {
		| FileOrLocation::File(content) =>
			self.handle_thumbnail_file(mxc, user, dim, content)
				.await,
		| FileOrLocation::Location(location) => self.handle_location(mxc, user, &location).await,
	}
}

#[implement(super::Service)]
async fn fetch_content_authenticated(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	timeout_ms: Duration,
) -> Result<FileMeta> {
	use federation::authenticated_media::get_content::v1::{Request, Response};

	let request = Request {
		media_id: mxc.media_id.into(),
		timeout_ms,
	};

	let Response { content, .. } = self
		.federation_request(mxc, user, server, request)
		.await?;

	match content {
		| FileOrLocation::File(content) => self.handle_content_file(mxc, user, content).await,
		| FileOrLocation::Location(location) => self.handle_location(mxc, user, &location).await,
	}
}

#[expect(deprecated)]
#[implement(super::Service)]
async fn fetch_thumbnail_unauthenticated(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	timeout_ms: Duration,
	dim: &Dim,
) -> Result<FileMeta> {
	use media::get_content_thumbnail::v3::{Request, Response};

	let request = Request {
		allow_remote: true,
		allow_redirect: true,
		animated: true.into(),
		method: dim.method.clone().into(),
		width: dim.width.into(),
		height: dim.height.into(),
		server_name: mxc.server_name.into(),
		media_id: mxc.media_id.into(),
		timeout_ms,
	};

	let Response {
		file, content_type, content_disposition, ..
	} = self
		.federation_request(mxc, user, server, request)
		.await?;

	let content = Content { file, content_type, content_disposition };

	self.handle_thumbnail_file(mxc, user, dim, content)
		.await
}

#[expect(deprecated)]
#[implement(super::Service)]
async fn fetch_content_unauthenticated(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	timeout_ms: Duration,
) -> Result<FileMeta> {
	use media::get_content::v3::{Request, Response};

	let request = Request {
		allow_remote: true,
		allow_redirect: true,
		server_name: mxc.server_name.into(),
		media_id: mxc.media_id.into(),
		timeout_ms,
	};

	let Response {
		file, content_type, content_disposition, ..
	} = self
		.federation_request(mxc, user, server, request)
		.await?;

	let content = Content { file, content_type, content_disposition };

	self.handle_content_file(mxc, user, content).await
}

#[implement(super::Service)]
async fn handle_thumbnail_file(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	dim: &Dim,
	content: Content,
) -> Result<FileMeta> {
	let content_disposition = make_content_disposition(
		content.content_disposition.as_ref(),
		content.content_type.as_deref(),
		None,
	);

	self.upload_thumbnail(
		mxc,
		user,
		Some(&content_disposition),
		content.content_type.as_deref(),
		dim,
		&content.file,
	)
	.await
	.map(|()| FileMeta {
		content: Some(content.file),
		content_type: content.content_type.map(Into::into),
		content_disposition: Some(content_disposition),
	})
}

#[implement(super::Service)]
async fn handle_content_file(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	content: Content,
) -> Result<FileMeta> {
	let content_disposition = make_content_disposition(
		content.content_disposition.as_ref(),
		content.content_type.as_deref(),
		None,
	);

	self.create(
		mxc,
		user,
		Some(&content_disposition),
		content.content_type.as_deref(),
		&content.file,
	)
	.await
	.map(|()| FileMeta {
		content: Some(content.file),
		content_type: content.content_type.map(Into::into),
		content_disposition: Some(content_disposition),
	})
}

#[implement(super::Service)]
async fn handle_location(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	location: &str,
) -> Result<FileMeta> {
	self.location_request(location)
		.await
		.map_err(|error| {
			err!(Request(NotFound(
				debug_warn!(%mxc, ?user, ?location, ?error, "Fetching media from location failed")
			)))
		})
}

#[implement(super::Service)]
async fn location_request(&self, location: &str) -> Result<FileMeta> {
	let response = self
		.services
		.client
		.extern_media
		.get(location)
		.send()
		.await?;

	let content_type = response
		.headers()
		.get(CONTENT_TYPE)
		.map(HeaderValue::to_str)
		.and_then(Result::ok)
		.map(str::to_owned);

	let content_disposition = response
		.headers()
		.get(CONTENT_DISPOSITION)
		.map(HeaderValue::as_bytes)
		.map(TryFrom::try_from)
		.and_then(Result::ok);

	response
		.bytes()
		.await
		.map(Vec::from)
		.map_err(Into::into)
		.map(|content| FileMeta {
			content: Some(content),
			content_type: content_type.clone(),
			content_disposition: Some(make_content_disposition(
				content_disposition.as_ref(),
				content_type.as_deref(),
				None,
			)),
		})
}

#[implement(super::Service)]
async fn federation_request<Request>(
	&self,
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	request: Request,
) -> Result<Request::IncomingResponse>
where
	Request: OutgoingRequest + Send + Debug,
{
	self.services
		.federation
		.execute(server.unwrap_or(mxc.server_name), request)
		.await
		.map_err(|error| handle_federation_error(mxc, user, server, error))
}

// Handles and adjusts the error for the caller to determine if they should
// request the fallback endpoint or give up.
fn handle_federation_error(
	mxc: &Mxc<'_>,
	user: Option<&UserId>,
	server: Option<&ServerName>,
	error: Error,
) -> Error {
	let fallback = || {
		err!(Request(NotFound(
			debug_error!(%mxc, ?user, ?server, ?error, "Remote media not found")
		)))
	};

	// Matrix server responses for fallback always taken.
	if error.kind() == NotFound || error.kind() == Unrecognized {
		return fallback();
	}

	// If we get these from any middleware we'll try the other endpoint rather than
	// giving up too early.
	if error.status_code().is_redirection()
		|| error.status_code().is_client_error()
		|| error.status_code().is_server_error()
	{
		return fallback();
	}

	// Reached for 5xx errors. This is where we don't fallback given the likelihood
	// the other endpoint will also be a 5xx and we're wasting time.
	error
}

#[implement(super::Service)]
#[expect(deprecated)]
pub async fn fetch_remote_thumbnail_legacy(
	&self,
	body: &media::get_content_thumbnail::v3::Request,
) -> Result<media::get_content_thumbnail::v3::Response> {
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	self.check_legacy_freeze()?;
	self.check_fetch_authorized(&mxc)?;
	let response = self
		.services
		.federation
		.execute(mxc.server_name, media::get_content_thumbnail::v3::Request {
			allow_remote: body.allow_remote,
			height: body.height,
			width: body.width,
			method: body.method.clone(),
			server_name: body.server_name.clone(),
			media_id: body.media_id.clone(),
			timeout_ms: body.timeout_ms,
			allow_redirect: body.allow_redirect,
			animated: body.animated,
		})
		.await?;

	let dim = Dim::from_ruma(body.width, body.height, body.method.clone())?;
	self.upload_thumbnail(
		&mxc,
		None,
		None,
		response.content_type.as_deref(),
		&dim,
		&response.file,
	)
	.await?;

	Ok(response)
}

#[implement(super::Service)]
#[expect(deprecated)]
pub async fn fetch_remote_content_legacy(
	&self,
	mxc: &Mxc<'_>,
	allow_redirect: bool,
	timeout_ms: Duration,
) -> Result<media::get_content::v3::Response, Error> {
	self.check_legacy_freeze()?;
	self.check_fetch_authorized(mxc)?;
	let response = self
		.services
		.federation
		.execute(mxc.server_name, media::get_content::v3::Request {
			allow_remote: true,
			server_name: mxc.server_name.into(),
			media_id: mxc.media_id.into(),
			timeout_ms,
			allow_redirect,
		})
		.await?;

	let content_disposition = make_content_disposition(
		response.content_disposition.as_ref(),
		response.content_type.as_deref(),
		None,
	);

	self.create(
		mxc,
		None,
		Some(&content_disposition),
		response.content_type.as_deref(),
		&response.file,
	)
	.await?;

	Ok(response)
}

#[implement(super::Service)]
fn check_fetch_authorized(&self, mxc: &Mxc<'_>) -> Result {
	if self
		.services
		.server
		.config
		.prevent_media_downloads_from
		.is_match(mxc.server_name.host())
		|| self
			.services
			.server
			.config
			.forbidden_remote_server_names
			.is_match(mxc.server_name.host())
	{
		// we'll lie to the client and say the blocked server's media was not found and
		// log. the client has no way of telling anyways so this is a security bonus.
		debug_warn!(%mxc, "Received request for media on blocklisted server");
		return Err!(Request(NotFound("Media not found.")));
	}

	Ok(())
}

#[implement(super::Service)]
fn check_legacy_freeze(&self) -> Result {
	self.services
		.server
		.config
		.freeze_legacy_media
		.then_some(())
		.ok_or(err!(Request(NotFound("Remote media is frozen."))))
}
