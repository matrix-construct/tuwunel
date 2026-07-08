mod support;

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use axum::{Router, body::Body};
	use tower::ServiceExt;
	use tuwunel_core::{
		Result,
		http::{Request, StatusCode, header},
	};
	use url::Url;

	use super::support::Harness;

	#[test]
	fn sso_redirect_keeps_extra_authorization_params_without_overriding_grant_params() -> Result {
		let mut harness = Harness::new("mindroom_rebase_sso_redirect", [])?;
		let discovery = harness.discovery_server("https://idp.example.test")?;
		harness.args.option.extend([
			"identity_provider.test.brand=\"test\"".to_owned(),
			"identity_provider.test.client_id=\"test-client\"".to_owned(),
			"identity_provider.test.client_secret=\"secret\"".to_owned(),
			"identity_provider.test.issuer_url=\"https://idp.example.test\"".to_owned(),
			format!("identity_provider.test.discovery_url=\"{}\"", discovery.url),
			"identity_provider.test.callback_url=\"https://matrix.example.test/_matrix/client/unstable/login/sso/callback/test-client\"".to_owned(),
			"identity_provider.test.extra_authorization_parameters={prompt=\"login\",client_id=\"evil-client\",state=\"evil-state\",redirect_uri=\"https://evil.example/callback\",response_type=\"implicit\"}".to_owned(),
		]);

		let result = harness.with_services(|services| async move {
			let (state, _guard) = tuwunel_api::router::state::create(services.clone());
			let router =
				tuwunel_api::router::build(Router::new(), &services.server).with_state(state);
			let response = router
				.oneshot(
					Request::builder()
						.method("GET")
						.uri(
							"/_matrix/client/v3/login/sso/redirect/test-client?\
							 redirectUrl=https%3A%2F%2Fclient.example.test%2Fdone",
						)
						.header("X-Forwarded-For", "127.0.0.1")
						.body(Body::empty())
						.expect("valid request"),
				)
				.await
				.expect("router response");

			assert_eq!(response.status(), StatusCode::FOUND);
			let location = response
				.headers()
				.get(header::LOCATION)
				.expect("SSO response should include Location")
				.to_str()
				.expect("Location header should be UTF-8");
			let location = Url::parse(location).expect("Location should be an absolute URL");
			let query = location
				.query_pairs()
				.into_owned()
				.collect::<BTreeMap<_, _>>();

			assert_eq!(query.get("prompt").map(String::as_str), Some("login"));
			assert_eq!(query.get("client_id").map(String::as_str), Some("test-client"));
			assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
			assert_eq!(
				query.get("redirect_uri").map(String::as_str),
				Some(
					"https://matrix.example.test/_matrix/client/unstable/login/sso/callback/test-client",
				),
			);
			assert_ne!(query.get("state").map(String::as_str), Some("evil-state"));

			let set_cookie = response
				.headers()
				.get(header::SET_COOKIE)
				.expect("SSO response should include grant cookie")
				.to_str()
				.expect("Set-Cookie header should be UTF-8");
			assert!(
				set_cookie.contains("Path=/_matrix/client/"),
				"Matrix client callback should use the hardened shared client cookie path: \
				 {set_cookie}",
			);

			Ok(())
		});

		discovery.handle.abort();
		result
	}
}
