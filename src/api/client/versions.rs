use std::iter::once;

use axum::extract::State;
use ruma::api::client::discovery::get_supported_versions::{self, Server};
use tuwunel_core::{
	Result,
	info::rustc::version as rustc_version,
	version::{name as package_name, version as package_version},
};

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
	State(services): State<crate::State>,
	_body: Ruma<get_supported_versions::Request>,
) -> Result<get_supported_versions::Response> {
	// MSC4383: client-side parity with /_matrix/federation/v1/version.
	let server = Server {
		compiler: rustc_version().map(Into::into),
		..Server::new(package_name().into(), package_version().into())
	};

	Ok(get_supported_versions::Response {
		versions: VERSIONS.into_iter().map(Into::into).collect(),

		unstable_features: UNSTABLE_FEATURES
			.into_iter()
			.chain(
				services
					.config
					.rendezvous_enabled
					.then_some("org.matrix.msc4108"),
			)
			.map(Into::into)
			.zip(once(true).cycle())
			.collect(),

		server: Some(server),
	})
}

static VERSIONS: [&str; 27] = [
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
	"v1.4",   /* Tested; private read receipts, threads */
	"v1.5",   /* Stable; Tested */
	"v1.6",   /* jump to date (element-web labs gate) */
	"v1.7",   /* intentional mentions */
	"v1.8",   /* no action */
	"v1.9",   /* no action */
	"v1.10",  /* Tested; relations recursion */
	"v1.11",  /* Tested; authenticated media */
	"v1.12",  /* no action */
	"v1.13",  /* no action */
	"v1.14",  /* no action */
	"v1.15",  /* OIDC auth metadata */
	"v1.16",  /* extended profiles (MSC4133) */
	"v1.17",  /* no action */
	"v1.18",  /* policy servers (MSC4284) */
	"v1.19",  /* mutual rooms (MSC2666) */
];

static UNSTABLE_FEATURES: [&str; 38] = [
	"org.matrix.e2e_cross_signing",
	// private read receipts (https://github.com/matrix-org/matrix-spec-proposals/pull/2285)
	"org.matrix.msc2285.stable",
	// appservice ping https://github.com/matrix-org/matrix-spec-proposals/pull/2659)
	"fi.mau.msc2659.stable",
	// query mutual rooms (https://github.com/matrix-org/matrix-spec-proposals/pull/2666)
	"uk.half-shot.msc2666.query_mutual_rooms",
	"uk.half-shot.msc2666.query_mutual_rooms.stable",
	// threading/threads (https://github.com/matrix-org/matrix-spec-proposals/pull/2836)
	"org.matrix.msc2836",
	// jump to date (https://github.com/matrix-org/matrix-spec-proposals/pull/3030)
	"org.matrix.msc3030",
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
	// MatrixRTC transport discovery (https://github.com/matrix-org/matrix-spec-proposals/pull/4143)
	"org.matrix.msc4143",
	// MSC4133 (custom profile fields) and MSC4175 (m.tz) stabilized in
	// Matrix 1.16; advertise the historical unstable prefixes alongside
	// the post-merge `.stable` flags for clients that haven't migrated.
	"uk.tcpip.msc4133",
	"uk.tcpip.msc4133.stable",
	"us.cloke.msc4175",
	"us.cloke.msc4175.stable",
	// stable flag for 3916 (https://github.com/matrix-org/matrix-spec-proposals/pull/4180)
	"org.matrix.msc4180",
	// Simplified Sliding sync (https://github.com/matrix-org/matrix-spec-proposals/pull/4186)
	"org.matrix.simplified_msc3575",
	// Allow room moderators to view redacted event content (https://github.com/matrix-org/matrix-spec-proposals/pull/2815)
	"fi.mau.msc2815",
	// OIDC-native auth umbrella (https://github.com/matrix-org/matrix-spec-proposals/pull/3861)
	"org.matrix.msc3861",
	// OIDC-native auth: authorization code grant (https://github.com/matrix-org/matrix-spec-proposals/pull/2964)
	"org.matrix.msc2964",
	// OIDC-native auth: auth issuer discovery (https://github.com/matrix-org/matrix-spec-proposals/pull/2965)
	"org.matrix.msc2965",
	// OIDC-native auth: dynamic client registration (https://github.com/matrix-org/matrix-spec-proposals/pull/2966)
	"org.matrix.msc2966",
	// OIDC-native auth: API scopes (https://github.com/matrix-org/matrix-spec-proposals/pull/2967)
	"org.matrix.msc2967",
	// OIDC delegation aware
	"org.matrix.msc3824",
	// Backwards-compatible redaction sending via /send (https://github.com/matrix-org/matrix-spec-proposals/pull/4169)
	"com.beeper.msc4169",
	// Invite blocking via m.invite_permission_config (https://github.com/matrix-org/matrix-spec-proposals/pull/4380)
	"org.matrix.msc4380",
	"org.matrix.msc4380.stable",
	// Client-server discovery of server version (https://github.com/matrix-org/matrix-spec-proposals/pull/4383)
	"net.zemos.msc4383",
	// Policy servers (https://github.com/matrix-org/matrix-spec-proposals/pull/4284)
	"org.matrix.msc4284",
	// Read receipts for threads (https://github.com/matrix-org/matrix-spec-proposals/pull/3771)
	"org.matrix.msc3771",
	// Notifications for threads (https://github.com/matrix-org/matrix-spec-proposals/pull/3773)
	"org.matrix.msc3773",
	// Threading via m.thread relations, stable since Matrix 1.4 (https://github.com/matrix-org/matrix-spec-proposals/pull/3440)
	"org.matrix.msc3440.stable",
	// state_after on /sync (https://github.com/matrix-org/matrix-spec-proposals/pull/4222)
	"org.matrix.msc4222",
];
