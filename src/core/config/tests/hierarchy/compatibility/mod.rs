use super::Files;
use crate::config::Config;

#[test]
fn profile_and_login_aliases_follow_source_precedence() {
	let files = Files::new();
	let canonical = concat!(
		"profile.allow_inbound_federation_lookups = true\n",
		"authentication.login.oidc_aware_preferred = true\n",
	);

	let new = files.write("canonical.toml", &format!("[global]\n{canonical}"));

	for (alias, profile, login) in [
		("allow_profile_lookup_federation_requests", false, true),
		("profile.allow_profile_lookup_federation_requests", false, true),
		("sso_aware_preferred", true, false),
		("authentication.login.sso_aware_preferred", true, false),
	] {
		let assignment = format!("{alias} = false\n");
		let old = files.write("alias.toml", &format!("[global]\n{assignment}"));

		for (paths, profile, login) in
			[([&old, &new], true, true), ([&new, &old], profile, login)]
		{
			let raw = Config::load_files(paths.into_iter()).expect("ordered aliases load");
			let config = Config::new(&raw).expect("actual profile and login groups extract");

			assert_eq!(config.profile.allow_inbound_federation_lookups, profile);
			assert_eq!(config.authentication.login.oidc_aware_preferred, login);
			assert!(!raw.contains(alias));
		}

		for (first, last) in [(assignment.as_str(), canonical), (canonical, assignment.as_str())]
		{
			let document = format!("[global]\n{first}{last}");
			let path = files.write("conflict.toml", &document);
			let error = Config::load_files([&path].into_iter())
				.expect_err("same-source alias collision rejected")
				.to_string();

			assert!(error.contains("conflict.toml"), "{error}");
			assert!(error.contains(alias), "{error}");
		}
	}
}
