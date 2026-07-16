use axum::extract::State;
use ruma::{Mxc, ServerName, UInt};
use synapse_admin_api::media::query_media::v1::{MediaInfo, Request, Response};
use tuwunel_core::{Result, err};
use tuwunel_service::media::UserMediaEntry;

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/media/{server_name}/{media_id}`
pub(crate) async fn admin_query_media_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	let entry = services
		.media
		.media_entry(&mxc)
		.await
		.ok_or_else(|| err!(Request(NotFound("Unknown media"))))?;

	let local = services.globals.server_is_ours(&body.server_name);

	Ok(Response {
		media_info: into_media_info(local, &body.server_name, entry),
	})
}

fn into_media_info(local: bool, server_name: &ServerName, entry: UserMediaEntry) -> MediaInfo {
	MediaInfo {
		media_origin: (!local).then(|| server_name.to_owned()),
		user_id: entry.user_id.filter(|_| local),
		media_id: entry
			.mxc
			.media_id()
			.unwrap_or_default()
			.to_owned(),
		media_type: entry.media_type.unwrap_or_default(),
		media_length: entry
			.media_length
			.and_then(|len| UInt::try_from(len).ok()),
		upload_name: entry.upload_name,
		created_ts: UInt::try_from(entry.created_ts).unwrap_or(UInt::MAX),
		filesystem_id: None,
		url_cache: None,
		last_access_ts: None,
		quarantined_by: None,
		authenticated: None,
		safe_from_quarantine: local.then_some(false),
		sha256: None,
	}
}

#[cfg(test)]
mod tests {
	use ruma::{UInt, server_name, user_id};

	use super::{UserMediaEntry, into_media_info};

	fn entry(
		user: bool,
		media_type: Option<&str>,
		upload_name: Option<&str>,
		media_length: Option<u64>,
		created_ts: u64,
	) -> UserMediaEntry {
		UserMediaEntry {
			mxc: "mxc://example.org/abc123".into(),
			media_type: media_type.map(ToOwned::to_owned),
			upload_name: upload_name.map(ToOwned::to_owned),
			media_length,
			created_ts,
			user_id: user.then(|| user_id!("@alice:example.org").to_owned()),
		}
	}

	#[test]
	fn local_media_fills_uploader_and_constants() {
		let info = into_media_info(
			true,
			server_name!("example.org"),
			entry(true, Some("image/png"), Some("pic.png"), Some(1024), 1_600_000_000_000),
		);

		assert_eq!(info.media_origin, None);
		assert_eq!(info.user_id, Some(user_id!("@alice:example.org").to_owned()));
		assert_eq!(info.media_id, "abc123");
		assert_eq!(info.media_type, "image/png");
		assert_eq!(info.media_length, Some(UInt::from(1024_u32)));
		assert_eq!(info.upload_name.as_deref(), Some("pic.png"));
		assert_eq!(info.created_ts, UInt::try_from(1_600_000_000_000_u64).unwrap());
		assert_eq!(info.safe_from_quarantine, Some(false));
		assert_eq!(info.filesystem_id, None);
		assert_eq!(info.url_cache, None);
		assert_eq!(info.last_access_ts, None);
		assert_eq!(info.quarantined_by, None);
		assert_eq!(info.authenticated, None);
		assert_eq!(info.sha256, None);
	}

	#[test]
	fn remote_media_sets_origin_and_drops_uploader() {
		let info = into_media_info(
			false,
			server_name!("remote.example.com"),
			entry(true, Some("image/png"), Some("pic.png"), Some(1024), 1_600_000_000_000),
		);

		assert_eq!(info.media_origin, Some(server_name!("remote.example.com").to_owned()));
		assert_eq!(info.user_id, None);
		assert_eq!(info.safe_from_quarantine, None);
	}

	#[test]
	fn absent_derivables_stay_null() {
		let info =
			into_media_info(true, server_name!("example.org"), entry(false, None, None, None, 0));

		assert_eq!(info.media_type, "");
		assert_eq!(info.media_length, None);
		assert_eq!(info.upload_name, None);
		assert_eq!(info.user_id, None);
		assert_eq!(info.created_ts, UInt::from(0_u32));
	}

	#[test]
	fn oversized_lengths_saturate() {
		let info = into_media_info(
			true,
			server_name!("example.org"),
			entry(true, Some("image/png"), Some("pic.png"), Some(u64::MAX), u64::MAX),
		);

		assert_eq!(info.media_length, None);
		assert_eq!(info.created_ts, UInt::MAX);
	}
}
