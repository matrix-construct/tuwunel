//! Wire checks shared by both media framing configurations.
//!
//! Each probe visits every client download and thumbnail registration and
//! checks document attachments against an independently supplied policy.

use std::fmt::Debug;

use futures::{StreamExt, TryStreamExt};
use tuwunel_core::{
	Err, Result, err,
	itertools::Itertools,
	smallstr::SmallString,
	utils::{BoolExt, stream::IterStream},
};
use tuwunel_service::Services;

use crate::media::{Source, authorize, describe, header, upload};

/// Access token registered by each media policy fixture.
///
/// Authenticated requests and uploads share this token within one test server.
pub(crate) const TOKEN: &str = "media-baseline-harness-access-token";

type Header = SmallString<[u8; 32]>;

/// The route families the policy is read on, with the token each takes.
///
/// The policy is a layer on the registration rather than on the handler,
/// and the two legacy prefixes are registered apart, so all three are
/// read rather than one per handler.
const PREFIXES: &[(&str, Option<&str>)] = &[
	("_matrix/client/v1/media", Some(TOKEN)),
	("_matrix/media/v3", None),
	("_matrix/media/r0", None),
];

/// The picture the policy is read on, which every route serves unchanged.
///
/// A 1x1 still upscales at every bucket and so comes back as itself from
/// a thumbnail too, which lets one shape be expected whatever size is
/// asked.
const STILL: Source = Source {
	name: "still_1x1.png",
	content_type: "image/png",
	bytes: include_bytes!("fixtures/still_1x1.png"),
};

/// The two uploads a browser would render rather than show.
///
/// Neither type is on the inline list, so both are served as an
/// attachment, which is the line the policy stands behind. The bytes need
/// not parse, since no download decodes its body.
const DOCUMENTS: &[Source] = &[
	Source {
		name: "page.html",
		content_type: "text/html",
		bytes: b"<!doctype html><title>x</title>",
	},
	Source {
		name: "doc.pdf",
		content_type: "application/pdf",
		bytes: b"%PDF-1.4\n",
	},
];

/// What one request was answered with, past the policy it must carry.
///
/// The body is kept as bytes rather than described, because a download
/// must return the upload unchanged.
struct Served {
	/// Response `Content-Type`, absent when the response carried none.
	content_type: Option<Header>,

	/// Response `Content-Disposition`, absent when none was carried.
	disposition: Option<Header>,

	/// Response `X-Frame-Options`, which only the HTML layer sets.
	frame_options: Option<Header>,

	/// Response `X-Content-Type-Options`, set on every response.
	type_options: Option<Header>,

	/// The body, unchanged.
	body: Vec<u8>,
}

/// Reads the policy on every client media registration.
///
/// One picture serves the nine registrations, since the answer on each is
/// the same upload under one policy. The two documents are uploaded
/// apart to verify their attachment disposition. The caller must register
/// [`TOKEN`] before invoking this probe.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn check(services: &Services, base: &str, expected: &str) -> Result {
	let server_name = services.globals.server_name();
	let media_id = upload(services, base, TOKEN, &STILL, None).await?;
	let path = format!("{server_name}/{media_id}");

	PREFIXES
		.iter()
		.copied()
		.stream()
		.then(async |(prefix, token)| {
			family(services, &format!("{base}/{prefix}"), token, &path, expected).await
		})
		.try_collect::<()>()
		.await?;

	DOCUMENTS
		.iter()
		.stream()
		.then(async |source| {
			document(services, base, server_name.as_str(), source, expected).await
		})
		.try_collect::<()>()
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn family(
	services: &Services,
	root: &str,
	token: Option<&str>,
	path: &str,
	expected: &str,
) -> Result {
	let url = format!("{root}/download/{path}");

	download(services, &url, token, &STILL, "inline", expected).await?;

	let url = format!("{root}/download/{path}/{}", STILL.name);
	let disposition = format!("inline; filename={}", STILL.name);

	download(services, &url, token, &STILL, &disposition, expected).await?;

	let url = format!("{root}/thumbnail/{path}?width=32&height=32&method=scale");
	let answer = served(services, &url, token, expected).await?;
	let shape = describe(&answer.body);
	let expected = (Some(STILL.content_type), Some("inline"), "Png 1x1 f1");
	let answered =
		(answer.content_type.as_deref(), answer.disposition.as_deref(), shape.as_str());

	expect(&url, &answered, &expected)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn document(
	services: &Services,
	base: &str,
	server_name: &str,
	source: &Source,
	expected: &str,
) -> Result {
	let media_id = upload(services, base, TOKEN, source, None).await?;
	let url = format!("{base}/_matrix/client/v1/media/download/{server_name}/{media_id}");
	let answer = download(services, &url, Some(TOKEN), source, "attachment", expected).await?;
	let html = source.content_type == "text/html";
	let expected = (html.then_some("DENY"), Some("nosniff"));
	let answered = (answer.frame_options.as_deref(), answer.type_options.as_deref());

	expect(&url, &answered, &expected)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn download(
	services: &Services,
	url: &str,
	token: Option<&str>,
	source: &Source,
	disposition: &str,
	expected: &str,
) -> Result<Served> {
	let answer = served(services, url, token, expected).await?;
	let expected = (Some(source.content_type), Some(disposition), true);
	let answered = (
		answer.content_type.as_deref(),
		answer.disposition.as_deref(),
		answer.body == source.bytes,
	);

	expect(url, &answered, &expected).map(|()| answer)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn served(
	services: &Services,
	url: &str,
	token: Option<&str>,
	expected: &str,
) -> Result<Served> {
	let request = services.client.clients.default.get(url);
	let response = authorize(request, token).send().await?;
	let status = response.status().as_u16();
	let policy = response
		.headers()
		.get_all("content-security-policy")
		.iter()
		.exactly_one()
		.ok()
		.and_then(|value| value.to_str().ok());

	if (status, policy) != (200, Some(expected)) {
		return Err!("{url} answered {status} with policy {policy:?}");
	}

	let read = |name| header(&response, name).map(Header::from);
	let content_type = read("content-type");
	let disposition = read("content-disposition");
	let frame_options = read("x-frame-options");
	let type_options = read("x-content-type-options");
	let body = response.bytes().await?.into();

	Ok(Served {
		content_type,
		disposition,
		frame_options,
		type_options,
		body,
	})
}

fn expect<T: PartialEq + Debug>(url: &str, answered: &T, expected: &T) -> Result {
	// into_option keeps the BoolExt import live where std's bool::ok_or_else shadows it
	answered
		.eq(expected)
		.into_option()
		.ok_or_else(|| err!("{url} answered {answered:?}"))
}
