use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{CanonicalJsonValue, DeviceId, UserId, api::client::uiaa::UiaaInfo},
};
use tuwunel_service::Services;

pub(super) async fn exercise(services: &Services, base: &str) -> Result {
	for case in ["cached", "evicted", "oversized"] {
		exercise_case(services, base, case).await?;
	}

	Ok(())
}

async fn exercise_case(services: &Services, base: &str, case: &str) -> Result {
	let localpart = format!("signing-{case}");
	let user = UserId::parse_with_server_name(&localpart, services.globals.server_name())?;
	let token = format!("signing-retention-access-token-{case}");

	services
		.users
		.create(&user, Some("signing-password"), Some("password"))
		.await?;

	let device = services
		.users
		.create_device(&user, None, (Some(&token), None), None, None, None)
		.await?;

	let url = format!("{base}/_matrix/client/v3/keys/device_signing/upload");
	let original = keys(&user, "A", 0);
	let replacement = keys(&user, "B", 0);
	let padding = if case == "oversized" { 8192 } else { 0 };
	let initial = keys(&user, "B", padding);

	let response = upload(services, &url, &token, &original).await?;

	assert_eq!(response.status(), StatusCode::OK);
	assert_keys(services, &user, &original).await?;

	let response = upload(services, &url, &token, &json!({})).await?;

	assert_eq!(response.status(), StatusCode::OK);
	assert_keys(services, &user, &original).await?;

	let response = upload(services, &url, &token, &initial).await?;

	assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

	let challenge: Value = response.json().await?;
	let session = challenge["session"]
		.as_str()
		.expect("UIAA session");

	let retained = || {
		services
			.uiaa
			.get_uiaa_request(&user, Some(&device), session)
	};

	assert_eq!(retained().is_some(), case != "oversized");
	assert_keys(services, &user, &original).await?;

	if case != "oversized" {
		let bytes = serde_json::to_vec(&initial)?
			.len()
			.saturating_add(user.as_str().len())
			.saturating_add(device.as_str().len())
			.saturating_add(session.len());

		assert!(bytes < 4096, "ordinary three-key upload must fit retention budget");
	}

	if case == "evicted" {
		displace(services, &user, &device)?;
		assert!(retained().is_none());
	}

	let auth = json!({
		"type": "m.login.password",
		"identifier": {"type": "m.id.user", "user": user},
		"password": "signing-password",
		"session": session,
	});

	let auth_only = json!({"auth": auth});
	let response = upload(services, &url, &token, &auth_only).await?;

	if case == "cached" {
		assert_eq!(response.status(), StatusCode::OK);
	} else {
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);

		let error: Value = response.json().await?;

		assert_eq!(error["errcode"], "M_MISSING_PARAM");
		assert_keys(services, &user, &original).await?;
		assert_session(services, &user, &device, session, true).await;

		let retry = authenticated(replacement.clone(), auth);
		let response = upload(services, &url, &token, &retry).await?;

		assert_eq!(response.status(), StatusCode::OK);
	}

	assert_keys(services, &user, &replacement).await?;
	assert!(retained().is_none());
	assert_session(services, &user, &device, session, false).await;

	let replay = authenticated(original, auth_only["auth"].clone());
	let response = upload(services, &url, &token, &replay).await?;

	assert_ne!(response.status(), StatusCode::OK);
	assert_keys(services, &user, &replacement).await?;

	Ok(())
}

fn keys(user: &UserId, material: &str, padding: usize) -> Value {
	let key = |usage, suffix| {
		let material = format!("{}{suffix}", material.repeat(42));

		json!({
			"user_id": user,
			"usage": [usage],
			"keys": {format!("ed25519:{material}"): material},
		})
	};

	json!({
		"master_key": key("master", "A"),
		"self_signing_key": key("self_signing", "E"),
		"user_signing_key": key("user_signing", "I"),
		"padding": "x".repeat(padding),
	})
}

async fn upload(services: &Services, url: &str, token: &str, body: &Value) -> Result<Response> {
	Ok(services
		.client
		.clients
		.default
		.post(url)
		.bearer_auth(token)
		.json(body)
		.send()
		.await?)
}

async fn assert_keys(services: &Services, user: &UserId, expected: &Value) -> Result {
	let primary = services
		.users
		.get_master_key(Some(user), user, &|_| true)
		.await?;

	let own = services
		.users
		.get_self_signing_key(Some(user), user, &|_| true)
		.await?;

	let others = services.users.get_user_signing_key(user).await?;

	for (name, raw) in
		[("master_key", primary), ("self_signing_key", own), ("user_signing_key", others)]
	{
		let actual: Value = serde_json::from_str(raw.json().get())?;

		for field in ["user_id", "usage", "keys"] {
			assert_eq!(actual[field], expected[name][field], "{name}.{field}");
		}
	}

	Ok(())
}

fn displace(services: &Services, user: &UserId, device: &DeviceId) -> Result {
	let body = CanonicalJsonValue::try_from(json!({"devices": []}))?;

	// Unanswered challenges retain their bodies and displace the older upload.
	for sequence in 0..1024 {
		let info = UiaaInfo {
			session: Some(format!("newer-signing-{sequence}")),
			..Default::default()
		};

		services.uiaa.create(user, device, &info, &body);
	}

	Ok(())
}

fn authenticated(mut keys: Value, auth: Value) -> Value {
	keys["auth"] = auth;
	keys
}

async fn assert_session(
	services: &Services,
	user: &UserId,
	device: &DeviceId,
	session: &str,
	expected: bool,
) {
	let key = (user, device, session);
	let exists = services.db["userdevicesessionid_uiaainfo"]
		.qry(&key)
		.await
		.is_ok();

	assert_eq!(exists, expected, "persisted UIAA session presence");
}
