use std::iter::once;

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
	Ok(get_supported_versions::Response {
		versions: VERSIONS.into_iter().map(Into::into).collect(),

		unstable_features: UNSTABLE_FEATURES
			.into_iter()
			.map(Into::into)
			.zip(once(true).cycle())
			.collect(),
	})
}

static VERSIONS: [&str; 17] = [
	"r0.0.1", /* Historical */
	"r0.1.0", /* Historical */
	"r0.2.0", /* Historical */
	"r0.3.0", /* Historical */
	"r0.4.0", /* Historical */
	"r0.5.0", /* Historical */
	"r0.6.0", /* Historical */
	"r0.6.1", /* Historical */
	"v1.1",   /* Stable; Tested */
	"v1.2",   /* Stable; Tested */
	"v1.3",   /* Stable; Tested */
	"v1.4",   /* Stable; Tested */
	"v1.5",   /* Stable; Tested */
	"v1.10",  /* Tested; relations recursion */
	"v1.11",  /* Tested; authenticated media */
	"v1.12",  /* m.tz */
	"v1.15",  /* custom profile fields */
];

static UNSTABLE_FEATURES: [&str; 18] = [
	"org.matrix.e2e_cross_signing",
	// private read receipts (https://github.com/matrix-org/matrix-spec-proposals/pull/2285)
	"org.matrix.msc2285.stable",
	// appservice ping https://github.com/matrix-org/matrix-spec-proposals/pull/2659)
	"fi.mau.msc2659.stable",
	// query mutual rooms (https://github.com/matrix-org/matrix-spec-proposals/pull/2666)
	"uk.half-shot.msc2666.query_mutual_rooms",
	// threading/threads (https://github.com/matrix-org/matrix-spec-proposals/pull/2836)
	"org.matrix.msc2836",
	// spaces/hierarchy summaries (https://github.com/matrix-org/matrix-spec-proposals/pull/2946)
	"org.matrix.msc2946",
	// busy presence status (https://github.com/matrix-org/matrix-spec-proposals/pull/3026)
	"org.matrix.msc3026.busy_presence",
	// sliding sync (https://github.com/matrix-org/matrix-spec-proposals/pull/3575/files#r1588877046)
	"org.matrix.msc3575",
	// dehydrated devices
	"org.matrix.msc3814",
	// filtering of /publicRooms by room type (https://github.com/matrix-org/matrix-spec-proposals/pull/3827)
	"org.matrix.msc3827",
	"org.matrix.msc3827.stable",
	// authenticated media (https://github.com/matrix-org/matrix-spec-proposals/pull/3916)
	"org.matrix.msc3916.stable",
	// intentional mentions (https://github.com/matrix-org/matrix-spec-proposals/pull/3952)
	"org.matrix.msc3952_intentional_mentions",
	// Extending User Profile API with Key:Value Pairs (https://github.com/matrix-org/matrix-spec-proposals/pull/4133)
	"uk.tcpip.msc4133",
	// Profile field for user time zone (https://github.com/matrix-org/matrix-spec-proposals/pull/4175)
	"us.cloke.msc4175",
	// stable flag for 3916 (https://github.com/matrix-org/matrix-spec-proposals/pull/4180)
	"org.matrix.msc4180",
	// Simplified Sliding sync (https://github.com/matrix-org/matrix-spec-proposals/pull/4186)
	"org.matrix.simplified_msc3575",
	// Allow room moderators to view redacted event content (https://github.com/matrix-org/matrix-spec-proposals/pull/2815)
	"fi.mau.msc2815",
];
