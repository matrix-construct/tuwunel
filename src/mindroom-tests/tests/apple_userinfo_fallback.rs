mod support;

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use axum::{Json, Router, body::Body, http::StatusCode as AxumStatus, routing::any};
	use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
	use serde_json::json;
	use tower::ServiceExt;
	use tuwunel_core::{
		Result,
		http::{Request, StatusCode, header},
		ruma::user_id,
	};
	use url::Url;

	use super::support::Harness;

	/// End-to-end pin of the Apple userinfo fallback through the full SSO
	/// callback flow: the token response's `id_token` must be persisted on the
	/// session (`apply_token_response`), and when the userinfo endpoint fails
	/// for an `appleoidc` provider, login must still complete from the
	/// id_token claims (`complete_sso_session`). Both behaviors live in
	/// rebase-sensitive regions of `sso_callback_route`.
	#[test]
	fn apple_login_succeeds_from_id_token_when_userinfo_fails() -> Result {
		let mut harness = Harness::new("mindroom_rebase_apple_fallback", [])?;

		let id_token = build_unsigned_id_token(&json!({
			"sub": "apple-user-1",
			"email": "fallback@example.test",
		}));
		let token_response = json!({
			"token_type": "Bearer",
			"access_token": "mock-apple-access-token",
			"expires_in": 3600,
			"id_token": id_token,
		});

		let mock = harness.mock_server(idp_router(token_response))?;
		let base = mock.base_url.clone();
		harness.args.option.extend([
			"identity_provider.apple.brand=\"appleoidc\"".to_owned(),
			"identity_provider.apple.client_id=\"apple-client\"".to_owned(),
			"identity_provider.apple.client_secret=\"secret\"".to_owned(),
			format!("identity_provider.apple.issuer_url=\"{base}\""),
			format!(
				"identity_provider.apple.discovery_url=\"{base}/.well-known/openid-configuration\""
			),
			"identity_provider.apple.callback_url=\"https://matrix.example.test/_matrix/client/unstable/login/sso/callback/apple-client\"".to_owned(),
		]);

		let expected_id_token = build_unsigned_id_token(&json!({
			"sub": "apple-user-1",
			"email": "fallback@example.test",
		}));

		let result = harness.with_services(|services| async move {
			// SSO registration is gated by the provider's own `registration`
			// flag (default true), so the callback registers this user from
			// the id_token's email-derived preferred_username.
			let user_id = user_id!("@fallback:localhost");

			let (state, _guard) = tuwunel_api::router::state::create(services.clone());
			let router =
				tuwunel_api::router::build(Router::new(), &services.server).with_state(state);

			// Step 1: redirect grants a session and the grant cookie.
			let response = router
				.clone()
				.oneshot(
					Request::builder()
						.method("GET")
						.uri(
							"/_matrix/client/v3/login/sso/redirect/apple-client?\
							 redirectUrl=https%3A%2F%2Fclient.example.test%2Fdone",
						)
						.header("X-Forwarded-For", "127.0.0.1")
						.body(Body::empty())
						.expect("valid request"),
				)
				.await
				.expect("router response");
			assert_eq!(response.status(), StatusCode::FOUND, "SSO redirect should succeed");

			let location = response
				.headers()
				.get(header::LOCATION)
				.expect("redirect Location")
				.to_str()
				.expect("UTF-8 Location")
				.to_owned();
			let location = Url::parse(&location).expect("absolute redirect URL");
			let query = location
				.query_pairs()
				.into_owned()
				.collect::<BTreeMap<_, _>>();
			let sess_id = query
				.get("state")
				.expect("state in grant query")
				.clone();

			let grant_cookie = response
				.headers()
				.get(header::SET_COOKIE)
				.expect("grant session cookie")
				.to_str()
				.expect("UTF-8 cookie")
				.split(';')
				.next()
				.expect("cookie name=value pair")
				.to_owned();

			// Step 2: callback exchanges the code; the mock IdP serves the
			// token (with id_token) but hard-fails the userinfo endpoint.
			let response = router
				.clone()
				.oneshot(
					Request::builder()
						.method("GET")
						.uri(format!(
							"/_matrix/client/unstable/login/sso/callback/apple-client?\
							 code=mock-code&state={sess_id}",
						))
						.header(header::COOKIE, &grant_cookie)
						.header("X-Forwarded-For", "127.0.0.1")
						.body(Body::empty())
						.expect("valid request"),
				)
				.await
				.expect("router response");

			let status = response.status();
			let location = response
				.headers()
				.get(header::LOCATION)
				.map(|location| {
					location
						.to_str()
						.expect("UTF-8 Location")
						.to_owned()
				});
			assert_eq!(
				status,
				StatusCode::FOUND,
				"callback should complete login via the id_token fallback: {location:?}",
			);

			let location =
				Url::parse(&location.expect("callback Location")).expect("absolute URL");
			assert!(
				location
					.as_str()
					.starts_with("https://client.example.test/done"),
				"callback should redirect to the client redirectUrl: {location}",
			);
			assert!(
				location
					.query_pairs()
					.any(|(key, value)| key == "loginToken" && !value.is_empty()),
				"callback redirect should carry a loginToken: {location}",
			);

			// The session must have persisted the id_token the fallback used.
			let session = services.oauth.sessions.get(&sess_id).await?;
			assert_eq!(
				session.id_token.as_deref(),
				Some(expected_id_token.as_str()),
				"token response id_token should be persisted on the session",
			);
			assert_eq!(
				session.user_id.as_deref(),
				Some(user_id),
				"session should bind the user derived from the id_token email claim",
			);

			// And the registered user must be an active SSO-origin account.
			assert!(services.users.exists(user_id).await, "fallback user should be registered");
			assert_eq!(services.users.origin(user_id).await?, "sso");
			assert!(services.users.is_active_local(user_id).await);

			Ok(())
		});

		mock.handle.abort();
		result
	}

	/// Discovery + token + failing-userinfo mock IdP. The fallback only
	/// base64-decodes the id_token payload, so an unsigned JWT suffices.
	fn idp_router(token_response: serde_json::Value) -> Router {
		Router::new()
			.route(
				"/.well-known/openid-configuration",
				any(|request: Request<Body>| async move {
					let base = format!(
						"http://{}",
						request
							.headers()
							.get(header::HOST)
							.expect("Host header")
							.to_str()
							.expect("UTF-8 Host"),
					);
					Json(json!({
						"issuer": base,
						"authorization_endpoint": format!("{base}/authorize"),
						"token_endpoint": format!("{base}/token"),
						"userinfo_endpoint": format!("{base}/userinfo"),
						"revocation_endpoint": format!("{base}/revoke"),
						"introspection_endpoint": format!("{base}/introspect"),
					}))
				}),
			)
			.route(
				"/token",
				any(move || {
					let token_response = token_response.clone();
					async move { Json(token_response) }
				}),
			)
			.route(
				"/userinfo",
				any(|| async { (AxumStatus::INTERNAL_SERVER_ERROR, "userinfo unavailable") }),
			)
	}

	fn build_unsigned_id_token(claims: &serde_json::Value) -> String {
		let header = b64.encode(serde_json::to_vec(&json!({"alg": "none"})).expect("header"));
		let payload = b64.encode(serde_json::to_vec(claims).expect("claims"));
		format!("{header}.{payload}.signature")
	}
}
