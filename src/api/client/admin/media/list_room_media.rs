use std::iter::once;

use axum::extract::State;
use ruma::{Mxc, OwnedMxcUri, ServerName};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use synapse_admin_api::media::list_room_media::v1::{Request, Response};
use tuwunel_core::{
	Result,
	matrix::Event,
	utils::{ReadyExt, stream::TryIgnore},
};

use crate::{Ruma, client::admin::require_admin};

type MxcLists = (Vec<OwnedMxcUri>, Vec<OwnedMxcUri>);

#[derive(Deserialize)]
struct ExtractUrls {
	url: Option<String>,
	info: Option<JsonValue>,
}

/// # `GET /_synapse/admin/v1/room/{room_id}/media`
///
/// Lists every MXC URI referenced by the room's unencrypted events, newest
/// first, split by origin server. Unknown rooms yield empty lists.
pub(crate) async fn admin_list_room_media_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let server_name = services.globals.server_name();

	let (local, remote) = services
		.timeline
		.pdus_rev(None, &body.room_id, None)
		.ignore_err()
		.ready_filter_map(|(_, pdu)| pdu.get_content().ok())
		.ready_fold_default(|lists, content: ExtractUrls| {
			collect_urls(lists, server_name, &content)
		})
		.await;

	Ok(Response { local, remote })
}

/// An event contributes only when its content carries a string `url`, mirroring
/// Synapse's contains_url row filter; `info.thumbnail_url` rides along.
/// Candidates parsing as MXC URIs are appended by origin, newest first,
/// duplicates preserved.
fn collect_urls(
	(mut local, mut remote): MxcLists,
	server_name: &ServerName,
	content: &ExtractUrls,
) -> MxcLists {
	let Some(url) = content.url.as_deref() else {
		return (local, remote);
	};

	let thumbnail_url = content
		.info
		.as_ref()
		.and_then(JsonValue::as_object)
		.and_then(|info| info.get("thumbnail_url"))
		.and_then(JsonValue::as_str);

	once(url)
		.chain(thumbnail_url)
		.filter_map(|url| Mxc::try_from(url).ok().map(|mxc| (url, mxc)))
		.for_each(|(url, mxc)| {
			if mxc.server_name == server_name {
				local.push(url.into());
			} else {
				remote.push(url.into());
			}
		});

	(local, remote)
}

#[cfg(test)]
mod tests {
	use ruma::server_name;
	use serde_json::json;

	use super::{ExtractUrls, MxcLists, collect_urls};

	fn collect(content: serde_json::Value) -> MxcLists {
		let content: ExtractUrls = serde_json::from_value(content).expect("valid ExtractUrls");

		collect_urls(MxcLists::default(), server_name!("example.org"), &content)
	}

	#[test]
	fn url_and_thumbnail_split_by_origin() {
		let (local, remote) = collect(json!({
			"url": "mxc://example.org/abc",
			"info": { "thumbnail_url": "mxc://remote.example/def" },
		}));

		assert_eq!(local, ["mxc://example.org/abc"]);
		assert_eq!(remote, ["mxc://remote.example/def"]);
	}

	#[test]
	fn thumbnail_only_event_contributes_nothing() {
		let (local, remote) = collect(json!({
			"info": { "thumbnail_url": "mxc://example.org/xyz" },
		}));

		assert!(local.is_empty());
		assert!(remote.is_empty());
	}

	#[test]
	fn non_object_info_still_lists_url() {
		let (local, remote) = collect(json!({
			"url": "mxc://example.org/abc",
			"info": "weird",
		}));

		assert_eq!(local, ["mxc://example.org/abc"]);
		assert!(remote.is_empty());
	}

	#[test]
	fn non_mxc_and_invalid_urls_skipped() {
		let (local, remote) = collect(json!({
			"url": "https://example.org/pic.png",
			"info": { "thumbnail_url": "mxc://example.org/has/slash" },
		}));

		assert!(local.is_empty());
		assert!(remote.is_empty());
	}

	#[test]
	fn empty_url_still_lists_thumbnail() {
		let (local, remote) = collect(json!({
			"url": "",
			"info": { "thumbnail_url": "mxc://remote.example/def" },
		}));

		assert!(local.is_empty());
		assert_eq!(remote, ["mxc://remote.example/def"]);
	}

	#[test]
	fn duplicates_preserved_across_events() {
		let content: ExtractUrls =
			serde_json::from_value(json!({ "url": "mxc://example.org/abc" }))
				.expect("valid ExtractUrls");

		let lists = collect_urls(MxcLists::default(), server_name!("example.org"), &content);
		let (local, remote) = collect_urls(lists, server_name!("example.org"), &content);

		assert_eq!(local, ["mxc://example.org/abc", "mxc://example.org/abc"]);
		assert!(remote.is_empty());
	}

	#[test]
	fn non_string_url_fails_the_row_filter() {
		let result = serde_json::from_value::<ExtractUrls>(json!({ "url": 5 }));

		assert!(result.is_err());
	}
}
