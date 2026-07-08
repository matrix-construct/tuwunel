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
	use tuwunel_service::users::DeactivationReason;
	use url::Url;

	use super::support::Harness;

	/// Pin the branches of `complete_sso_session` that the new-user happy path
	/// in `apple_userinfo_fallback` does not reach. `complete_sso_session` is
	/// the helper extracted during the upstream-main rebase so the browser SSO
	/// callback and the native Apple endpoint share one identity -> account
	/// completion path; a rebase that silently drops or reorders a step inside
	/// it still compiles, so these drive the real router (redirect -> callback)
	/// to catch that.
	///
	/// The three scenarios run against three separate providers/identities in a
	/// single server, because each `with_services` initializes the global
	/// tracing subscriber and a test binary can only do that once.
	#[test]
	fn complete_sso_session_reuse_rejection_and_reactivation() -> Result {
		let mut harness = Harness::new("mindroom_rebase_sso_completion", [])?;

		// Scenario A: re-login reuses the identity and deletes the prior session.
		let reuse = harness.mock_server(idp_router("sub-reuse", "reuseone@example.test"))?;
		// Scenario B: an admin-deactivated account is rejected on re-login.
		let admin = harness.mock_server(idp_router("sub-admin", "reusetwo@example.test"))?;
		// Scenario C: a self-deactivated account is reactivated on re-login.
		let reactivate =
			harness.mock_server(idp_router("sub-self", "reusethree@example.test"))?;

		harness
			.args
			.option
			.extend(provider_options("reuse", "reuse-client", &reuse.base_url));
		harness
			.args
			.option
			.extend(provider_options("admin", "admin-client", &admin.base_url));
		harness.args.option.extend(provider_options(
			"reactivate",
			"reactivate-client",
			&reactivate.base_url,
		));

		let result = harness.with_services(|services| async move {
			let (state, _guard) = tuwunel_api::router::state::create(services.clone());
			let router =
				tuwunel_api::router::build(Router::new(), &services.server).with_state(state);

			// --- Scenario A: identity reuse + previous-session deletion --------
			let reuse_user = user_id!("@reuseone:localhost");
			let (sess1, status1, location1) = drive_login(&router, "reuse-client").await;
			assert_eq!(status1, StatusCode::FOUND, "first login should complete: {location1:?}");
			assert!(has_login_token(location1.as_ref()), "first login should mint a loginToken");
			assert!(services.users.exists(reuse_user).await, "first login should register");
			assert_eq!(services.users.origin(reuse_user).await?, "sso");
			assert!(
				services.oauth.sessions.get(&sess1).await.is_ok(),
				"first login's session should be committed",
			);

			let (sess2, status2, location2) = drive_login(&router, "reuse-client").await;
			assert_eq!(status2, StatusCode::FOUND, "re-login should complete: {location2:?}");
			assert!(has_login_token(location2.as_ref()), "re-login should mint a loginToken");
			assert_ne!(sess1, sess2, "re-login should use a fresh session id");
			assert!(services.users.is_active_local(reuse_user).await);
			assert!(
				services.oauth.sessions.get(&sess2).await.is_ok(),
				"the newest session should be committed",
			);
			assert!(
				services.oauth.sessions.get(&sess1).await.is_err(),
				"re-login should delete the previous session for the same identity",
			);

			// --- Scenario B: admin-deactivated accounts cannot re-login --------
			let admin_user = user_id!("@reusetwo:localhost");
			let (_a1, admin_status1, _al1) = drive_login(&router, "admin-client").await;
			assert_eq!(admin_status1, StatusCode::FOUND, "admin-scenario first login registers");

			services
				.deactivate
				.full_deactivate(admin_user, false, DeactivationReason::Admin)
				.await?;
			assert!(services.users.is_deactivated(admin_user).await?);

			let (_a2, admin_status2, admin_location2) =
				drive_login(&router, "admin-client").await;
			assert_ne!(
				admin_status2,
				StatusCode::FOUND,
				"admin-deactivated account must not complete SSO login: {admin_location2:?}",
			);
			assert!(
				!has_login_token(admin_location2.as_ref()),
				"a rejected login must not mint a loginToken: {admin_location2:?}",
			);
			assert!(
				services.users.is_deactivated(admin_user).await?,
				"account must stay deactivated after the rejected re-login",
			);

			// --- Scenario C: self-deactivated accounts reactivate on re-login --
			let self_user = user_id!("@reusethree:localhost");
			let (_c1, self_status1, _cl1) = drive_login(&router, "reactivate-client").await;
			assert_eq!(self_status1, StatusCode::FOUND, "self-scenario first login registers");

			services
				.deactivate
				.full_deactivate(self_user, false, DeactivationReason::SelfService)
				.await?;
			assert!(services.users.is_deactivated(self_user).await?);

			let (_c2, self_status2, self_location2) =
				drive_login(&router, "reactivate-client").await;
			assert_eq!(
				self_status2,
				StatusCode::FOUND,
				"self-deactivated account should reactivate + complete: {self_location2:?}",
			);
			assert!(
				has_login_token(self_location2.as_ref()),
				"reactivated login should mint a loginToken: {self_location2:?}",
			);
			assert!(
				services.users.is_active_local(self_user).await,
				"self-deactivated account should be active again after SSO re-login",
			);

			Ok(())
		});

		reuse.handle.abort();
		admin.handle.abort();
		reactivate.handle.abort();
		result
	}

	fn provider_options(id: &str, client: &str, base: &str) -> Vec<String> {
		vec![
			format!("identity_provider.{id}.brand=\"appleoidc\""),
			format!("identity_provider.{id}.client_id=\"{client}\""),
			format!("identity_provider.{id}.client_secret=\"secret\""),
			format!("identity_provider.{id}.issuer_url=\"{base}\""),
			format!(
				"identity_provider.{id}.discovery_url=\"{base}/.well-known/openid-configuration\""
			),
			format!(
				"identity_provider.{id}.callback_url=\"https://matrix.example.test/_matrix/client/unstable/login/sso/callback/{client}\""
			),
		]
	}

	/// Drive one full redirect -> callback for `client` and return
	/// `(sess_id, callback_status, callback_location)`. The redirect is always
	/// expected to succeed (pre-auth); the callback status/location are
	/// returned unasserted so callers can pin both success and rejection.
	async fn drive_login(router: &Router, client: &str) -> (String, StatusCode, Option<String>) {
		let response = router
			.clone()
			.oneshot(
				Request::builder()
					.method("GET")
					.uri(format!(
						"/_matrix/client/v3/login/sso/redirect/{client}?redirectUrl=https%3A%2F%\
						 2Fclient.example.test%2Fdone",
					))
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
		let sess_id = location
			.query_pairs()
			.into_owned()
			.collect::<BTreeMap<_, _>>()
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

		let response = router
			.clone()
			.oneshot(
				Request::builder()
					.method("GET")
					.uri(format!(
						"/_matrix/client/unstable/login/sso/callback/{client}?code=mock-code&\
						 state={sess_id}",
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
			.map(|value| value.to_str().expect("UTF-8 Location").to_owned());

		(sess_id, status, location)
	}

	fn has_login_token(location: Option<&String>) -> bool {
		location
			.and_then(|location| Url::parse(location).ok())
			.is_some_and(|location| {
				location
					.query_pairs()
					.any(|(key, value)| key == "loginToken" && !value.is_empty())
			})
	}

	/// Discovery + token + failing-userinfo mock IdP; the appleoidc id_token
	/// fallback carries a stable `sub` so every login maps to one identity.
	fn idp_router(sub: &str, email: &str) -> Router {
		let id_token = build_unsigned_id_token(&json!({ "sub": sub, "email": email }));
		let token_response = json!({
			"token_type": "Bearer",
			"access_token": "mock-access-token",
			"expires_in": 3600,
			"id_token": id_token,
		});

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
