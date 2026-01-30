use std::collections::BTreeMap;

use axum::{Json, extract::State, response::IntoResponse};
use futures::StreamExt;
use ruma::api::client::discovery::get_supported_versions;
use tuwunel_core::Result;

use crate::Ruma;

/// # `GET /_matrix/client/versions`
///
/// Get the versions of the specification and unstable features supported by
/// this server.
///
/// - Versions take the form MAJOR.MINOR.PATCH
/// - Only the latest PATCH release will be reported for each MAJOR.MINOR value
/// - Unstable features are namespaced and may include version information in
///   their name
///
/// Note: Unstable features are used while developing new features. Clients
/// should avoid using unstable features in their stable releases
pub(crate) async fn get_supported_versions_route(
	_body: Ruma<get_supported_versions::Request>,
) -> Result<get_supported_versions::Response> {
	let resp = get_supported_versions::Response {
		versions: vec![
			"r0.0.1".to_owned(),
			"r0.1.0".to_owned(),
			"r0.2.0".to_owned(),
			"r0.3.0".to_owned(),
			"r0.4.0".to_owned(),
			"r0.5.0".to_owned(),
			"r0.6.0".to_owned(),
			"r0.6.1".to_owned(),
			"v1.1".to_owned(),
			"v1.2".to_owned(),
			"v1.3".to_owned(),
			"v1.4".to_owned(),
			"v1.5".to_owned(),
			"v1.10".to_owned(), // relations recursion
			"v1.11".to_owned(), // authenticated media
			"v1.12".to_owned(), // m.tz
			"v1.15".to_owned(), // custom profile fields
		],
		unstable_features: BTreeMap::from_iter([
			("org.matrix.e2e_cross_signing".to_owned(), true),
			("org.matrix.msc2285.stable".to_owned(), true), /* private read receipts (https://github.com/matrix-org/matrix-spec-proposals/pull/2285) */
			("fi.mau.msc2659.stable".to_owned(), true), /* appservice ping https://github.com/matrix-org/matrix-spec-proposals/pull/2659) */
			("uk.half-shot.msc2666.query_mutual_rooms".to_owned(), true), /* query mutual rooms (https://github.com/matrix-org/matrix-spec-proposals/pull/2666) */
			("org.matrix.msc2836".to_owned(), true), /* threading/threads (https://github.com/matrix-org/matrix-spec-proposals/pull/2836) */
			("org.matrix.msc2946".to_owned(), true), /* spaces/hierarchy summaries (https://github.com/matrix-org/matrix-spec-proposals/pull/2946) */
			("org.matrix.msc3026.busy_presence".to_owned(), true), /* busy presence status (https://github.com/matrix-org/matrix-spec-proposals/pull/3026) */
			("org.matrix.msc3575".to_owned(), true), /* sliding sync (https://github.com/matrix-org/matrix-spec-proposals/pull/3575/files#r1588877046) */
			("org.matrix.msc3814".to_owned(), true), /* dehydrated devices */
			("org.matrix.msc3827".to_owned(), true), /* filtering of /publicRooms by room type (https://github.com/matrix-org/matrix-spec-proposals/pull/3827) */
			("org.matrix.msc3916.stable".to_owned(), true), /* authenticated media (https://github.com/matrix-org/matrix-spec-proposals/pull/3916) */
			("org.matrix.msc3952_intentional_mentions".to_owned(), true), /* intentional mentions (https://github.com/matrix-org/matrix-spec-proposals/pull/3952) */
			("uk.tcpip.msc4133".to_owned(), true), /* Extending User Profile API with Key:Value Pairs (https://github.com/matrix-org/matrix-spec-proposals/pull/4133) */
			("us.cloke.msc4175".to_owned(), true), /* Profile field for user time zone (https://github.com/matrix-org/matrix-spec-proposals/pull/4175) */
			("org.matrix.msc4180".to_owned(), true), /* stable flag for 3916 (https://github.com/matrix-org/matrix-spec-proposals/pull/4180) */
			("org.matrix.simplified_msc3575".to_owned(), true), /* Simplified Sliding sync (https://github.com/matrix-org/matrix-spec-proposals/pull/4186) */
			("fi.mau.msc2815".to_owned(), true), /* Allow room moderators to view redacted event content (https://github.com/matrix-org/matrix-spec-proposals/pull/2815) */
		]),
	};

	Ok(resp)
}

/// # `GET /_tuwunel/server_version`
///
/// Tuwunel-specific API to get the server version, results akin to
/// `/_matrix/federation/v1/version`
pub(crate) async fn tuwunel_server_version() -> Result<impl IntoResponse> {
	Ok(Json(serde_json::json!({
		"name": tuwunel_core::version::name(),
		"version": tuwunel_core::version::version(),
	})))
}

/// # `GET /_tuwunel/local_user_count`
///
/// Tuwunel-specific API to return the amount of users registered on this
/// homeserver. Endpoint is disabled if federation is disabled for privacy. This
/// only includes active users (not deactivated, no guests, etc)
pub(crate) async fn tuwunel_local_user_count(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let user_count = services.users.list_local_users().count().await;

	Ok(Json(serde_json::json!({
		"count": user_count
	})))
}
