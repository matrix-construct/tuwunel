#![cfg(test)]

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

use serde_json::Value;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::fixture::boot;

const OIDC_FEATURES: [&str; 6] = [
	"org.matrix.msc3861",
	"org.matrix.msc2964",
	"org.matrix.msc2965",
	"org.matrix.msc2966",
	"org.matrix.msc2967",
	"org.matrix.msc3824",
];

#[test]
fn unstable_features_follow_runtime_state() -> Result {
	// One OIDC input alone leaves the server unbuilt, so the flags stay absent.
	let enabled = false;
	let unbuilt = [
		("versions-gates-off", None),
		("versions-gates-native", Some("authentication.oidc.native_auth=true")),
		("versions-gates-client", Some("well_known.client=\"https://localhost\"")),
	];

	for (name, input) in unbuilt {
		let options = ["logging.enable=false", "rooms.policy_server.enable=false"]
			.into_iter()
			.chain(input);

		boot(name, options, async |services, base| {
			check_features(services, base, enabled).await
		})?;
	}

	let enabled = true;
	let options = [
		"logging.enable=false",
		"well_known.client=\"https://localhost\"",
		"authentication.oidc.native_auth=true",
		"rooms.policy_server.enable=true",
	];

	boot("versions-gates-on", options, async |services, base| {
		check_features(services, base, enabled).await
	})
}

async fn check_features(services: &Services, base: &str, enabled: bool) -> Result {
	let client = &services.client.clients.default;
	let versions: Value = client
		.get(format!("{base}/_matrix/client/versions"))
		.send()
		.await?
		.json()
		.await?;

	let features = &versions["unstable_features"];

	assert_eq!(features["org.matrix.simplified_msc3575"], true, "{versions}");

	for feature in OIDC_FEATURES {
		let advertised = features.get(feature).and_then(Value::as_bool);

		assert_eq!(advertised, enabled.then_some(true), "{feature}: {versions}");
	}

	// MSC4284 defines no unstable flag and landed in v1.18, so the knob has no say.
	assert_eq!(services.config.rooms.policy_server.enable, enabled, "knob applied");
	assert_eq!(features.get("org.matrix.msc4284"), None, "{versions}");

	let metadata = client
		.get(format!("{base}/_matrix/client/v1/auth_metadata"))
		.send()
		.await?;

	assert_eq!(metadata.status().is_success(), enabled, "{metadata:?}");

	Ok(())
}
