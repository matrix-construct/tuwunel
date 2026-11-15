use serde_html_form::from_bytes;

use super::{
	ACCOUNT_MANAGEMENT_ACTIONS_SUPPORTED, AccountCallbackParams, normalize_account_action,
};

#[test]
fn stable_names_map_to_aliases() {
	assert_eq!(normalize_account_action("org.matrix.devices_list"), "org.matrix.sessions_list");
	assert_eq!(normalize_account_action("org.matrix.device_view"), "org.matrix.session_view");
	assert_eq!(normalize_account_action("org.matrix.device_delete"), "org.matrix.session_end");
}

#[test]
fn aliases_and_others_pass_through() {
	for action in [
		"org.matrix.sessions_list",
		"org.matrix.session_view",
		"org.matrix.session_end",
		"org.matrix.profile",
		"org.matrix.account_deactivate",
		"org.matrix.cross_signing_reset",
	] {
		assert_eq!(normalize_account_action(action), action);
	}
}

#[test]
fn stable_actions_are_advertised() {
	for action in [
		"org.matrix.profile",
		"org.matrix.devices_list",
		"org.matrix.device_view",
		"org.matrix.device_delete",
		"org.matrix.account_deactivate",
		"org.matrix.cross_signing_reset",
	] {
		assert!(ACCOUNT_MANAGEMENT_ACTIONS_SUPPORTED.contains(&action));
	}
}

#[test]
fn repeated_device_ids_parse() {
	let form = b"action=org.matrix.session_end&device_id=FIRST&device_id=SECOND&loginToken=TOKEN";
	let params: AccountCallbackParams = from_bytes(form).expect("callback form");

	assert_eq!(params.device_ids.as_slice(), ["FIRST", "SECOND"]);
	assert_eq!(params.login_token.as_deref(), Some("TOKEN"));
}

#[test]
fn single_and_absent_device_ids_parse() {
	let params: AccountCallbackParams = from_bytes(b"device_id=ONLY").expect("callback form");

	assert_eq!(params.device_ids.as_slice(), ["ONLY"]);

	let params: AccountCallbackParams =
		from_bytes(b"action=org.matrix.sessions_list").expect("callback form");

	assert!(params.device_ids.is_empty(), "{:?}", params.device_ids);

	// Every sign-in leg carries an empty `device_id` to the callback.
	let params: AccountCallbackParams =
		from_bytes(b"action=org.matrix.sessions_list&device_id=").expect("callback form");

	assert_eq!(params.device_ids.as_slice(), [""]);
}

#[test]
fn repeated_action_is_refused() {
	let form = b"action=org.matrix.session_end&action=org.matrix.session_end";
	let result: Result<AccountCallbackParams, _> = from_bytes(form);

	result.expect_err("one action only");
}
