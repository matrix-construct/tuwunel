use std::{net::TcpListener, sync::Arc, time::Duration};

use futures::{StreamExt, future::join};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;
use tuwunel_core::{
	Result, err,
	jwt::{EncodingKey, Header, encode},
	ruma::{MilliSecondsSinceUnixEpoch, UserId, thirdparty::Medium},
	utils::hash::verify_password,
};
use tuwunel_service::{
	Services,
	users::{PASSWORD_SENTINEL, Register},
};

use super::{
	Reset, RestartState, Substitution,
	http::{
		AuthenticatedPasswordChange, PasswordTokenRequest, authenticated_password_change,
		confirm_email, jwt_password_change, password_uiaa_session, request_password_token,
		request_token_failure, reset_password, view_confirmation, wait_until_ready,
	},
};

const A_OLD_PASSWORD: &str = "email-reset-a-old-password";
const A_NEW_PASSWORD: &str = "email-reset-a-new-password";
const A_UNCONFIRMED_PASSWORD: &str = "email-reset-a-unconfirmed-password";
const A_UNKNOWN_PASSWORD: &str = "email-reset-a-unknown-password";
const A_EXPIRED_PASSWORD: &str = "email-reset-a-expired-password";
const A_WRONG_SECRET_PASSWORD: &str = "email-reset-a-wrong-secret-password";
const A_UNBOUND_PASSWORD: &str = "email-reset-a-unbound-password";
const A_KEEP_DEVICES_PASSWORD: &str = "email-reset-a-keep-devices-password";
const A_REPLAY_PASSWORD: &str = "email-reset-a-replay-password";
const A_DEACTIVATED_PASSWORD: &str = "email-reset-a-deactivated-password";
const A_EMAIL: &str = "strass-reset@example.org";
const A_EMAIL_VARIANT: &str = "Straß-Reset@Example.Org";
const A_INITIAL_TOKENS: [&str; 2] = [
	"email-reset-user-a-device-token-one-0001",
	"email-reset-user-a-device-token-two-0002",
];
const A_DEFAULT_TOKENS: [&str; 2] = [
	"email-reset-user-a-default-token-one-0003",
	"email-reset-user-a-default-token-two-0004",
];
const B_PASSWORD: &str = "email-reset-b-password";
const B_NEW_PASSWORD: &str = "email-reset-b-new-password";
const B_JWT_PASSWORD: &str = "email-reset-b-jwt-password";
const B_TOKENLESS_JWT_PASSWORD: &str = "email-reset-b-tokenless-jwt-password";
const B_EMAIL: &str = "reset-b@example.org";
const B_TOKEN: &str = "email-reset-user-b-device-token-0001";
const C_EMAIL: &str = "reset-non-password@example.org";
const C_FAILED_PASSWORD: &str = "email-reset-c-failed-password";
const C_REPLAY_PASSWORD: &str = "email-reset-c-replay-password";
const CLIENT_SECRET: &str = "email-reset-client-secret";
pub(super) const JWT_SECRET: &str = "email-reset-jwt-secret";

#[derive(Clone, Copy)]
struct Phase<'a> {
	services: &'a Services,
	client: &'a Client,
	base: &'a str,
	state: &'a RestartState,
	user_a: &'a UserId,
	user_b: &'a UserId,
	substitution: Substitution<'a>,
}

pub(super) async fn first_phase(
	services: Arc<Services>,
	client: Client,
	base: String,
	smtp_listener: TcpListener,
) -> Result<RestartState> {
	wait_until_ready(&client, &base).await?;

	let user_a = UserId::parse_with_server_name("email-reset-a", services.globals.server_name())?;
	let user_b = UserId::parse_with_server_name("email-reset-b", services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_a),
			password: Some(A_OLD_PASSWORD),
			..Default::default()
		})
		.await?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_b),
			password: Some(B_PASSWORD),
			..Default::default()
		})
		.await?;

	create_devices(&services, &user_a, &A_INITIAL_TOKENS).await?;
	let b_device = services
		.users
		.create_device(&user_b, None, (Some(B_TOKEN), None), None, None, None)
		.await?;

	assert_eq!(
		services
			.users
			.all_device_ids(&user_a)
			.count()
			.await,
		2
	);

	let a_password_hash = services.users.password_hash(&user_a).await?;
	let b_password_hash = services.users.password_hash(&user_b).await?;
	let now = MilliSecondsSinceUnixEpoch::now();

	services
		.threepid
		.put_binding(&user_a, A_EMAIL, Medium::Email, now, now)
		.await;

	let unknown_token = request_token_failure(
		&client,
		&base,
		"missing-reset@example.org",
		"email-reset-missing-client-secret",
	)
	.await?;

	let unknown_body = serde_json::from_str::<Value>(&unknown_token.1)?;

	assert_ne!(unknown_token.0, 200, "unknown email requestToken succeeded");
	assert_eq!(
		unknown_body
			.get("errcode")
			.and_then(Value::as_str),
		Some("M_THREEPID_NOT_FOUND")
	);

	assert_eq!(services.users.password_hash(&user_a).await?, a_password_hash);
	assert_eq!(services.users.password_hash(&user_b).await?, b_password_hash);

	let b_session = password_uiaa_session(&client, &base, B_TOKEN).await?;
	let substitution = Substitution {
		session: &b_session,
		user: user_b.as_str(),
		email: B_EMAIL,
		device: b_device.as_str(),
	};

	let (sid, token) = request_password_token(PasswordTokenRequest {
		client: &client,
		smtp_listener,
		base: &base,
		email: A_EMAIL_VARIANT,
		canonical_email: A_EMAIL,
		client_secret: CLIENT_SECRET,
	})
	.await?;

	let confirmation = view_confirmation(&client, &base, &sid, CLIENT_SECRET, &token).await?;

	for field in ["name=\"sid\"", "name=\"client_secret\"", "name=\"token\""] {
		assert!(confirmation.contains(field), "confirmation page omitted {field}");
	}

	assert!(
		!services
			.threepid
			.session_validated(&sid, CLIENT_SECRET)
			.await,
		"confirmation GET validated the pending proof"
	);

	let unconfirmed = Reset {
		sid: &sid,
		client_secret: CLIENT_SECRET,
		new_password: A_UNCONFIRMED_PASSWORD,
		logout_devices: Some(true),
		substitution,
	};

	let masked = reset_password(&client, &base, unconfirmed).await?;
	let masked_body = serde_json::from_str::<Value>(&masked.1)?;

	assert_eq!(masked.0, 403, "unconfirmed password reset: {}", masked.1);
	assert_eq!(masked_body.get("errcode").and_then(Value::as_str), Some("M_FORBIDDEN"));

	let unknown = Reset {
		sid: "email-reset-unknown-sid",
		client_secret: "email-reset-unknown-secret",
		new_password: A_UNKNOWN_PASSWORD,
		logout_devices: Some(true),
		substitution,
	};

	let response = reset_password(&client, &base, unknown).await?;

	assert_eq!(response, masked, "unknown proof exposed a different failure");

	let expired_secret = "email-reset-expired-secret";
	let expired = services
		.threepid
		.create_or_reuse_pending(expired_secret, Medium::Email, A_EMAIL, 1, Duration::ZERO)
		.await?;

	sleep(Duration::from_millis(2)).await;

	let expired = Reset {
		sid: &expired.sid,
		client_secret: expired_secret,
		new_password: A_EXPIRED_PASSWORD,
		logout_devices: Some(true),
		substitution,
	};

	let response = reset_password(&client, &base, expired).await?;

	assert_eq!(response, masked, "expired proof exposed a different failure");
	assert_eq!(services.users.password_hash(&user_a).await?, a_password_hash);
	assert_password(&services, &user_a, A_OLD_PASSWORD, true).await?;
	assert_password(&services, &user_a, A_UNCONFIRMED_PASSWORD, false).await?;
	assert_password(&services, &user_a, A_UNKNOWN_PASSWORD, false).await?;
	assert_password(&services, &user_a, A_EXPIRED_PASSWORD, false).await?;

	confirm_email(&client, &base, &sid, CLIENT_SECRET, &token).await?;

	assert!(
		services
			.threepid
			.session_validated(&sid, CLIENT_SECRET)
			.await,
		"the confirmation replay destroyed the validated proof"
	);

	Ok(RestartState {
		sid,
		b_session,
		b_device: b_device.to_string(),
		a_password_hash,
		b_password_hash,
		masked,
	})
}

pub(super) async fn second_phase(
	services: Arc<Services>,
	client: Client,
	base: String,
	state: RestartState,
) -> Result {
	wait_until_ready(&client, &base).await?;

	let user_a = UserId::parse_with_server_name("email-reset-a", services.globals.server_name())?;
	let user_b = UserId::parse_with_server_name("email-reset-b", services.globals.server_name())?;
	let substitution = Substitution {
		session: &state.b_session,
		user: user_b.as_str(),
		email: B_EMAIL,
		device: &state.b_device,
	};

	let phase = Phase {
		services: &services,
		client: &client,
		base: &base,
		state: &state,
		user_a: &user_a,
		user_b: &user_b,
		substitution,
	};

	verify_restart_state(phase).await?;

	complete_persisted_reset(phase).await?;

	reject_unbound_reset(phase).await?;

	failed_password_write(&services, &client, &base, substitution, &state.masked).await?;

	verify_device_modes(phase).await?;

	verify_authenticated_changes(phase).await?;

	reject_deactivated_reset(phase).await
}

async fn verify_restart_state(phase: Phase<'_>) -> Result {
	assert!(
		phase
			.services
			.threepid
			.session_validated(&phase.state.sid, CLIENT_SECRET)
			.await,
		"validated password-reset SID did not survive restart"
	);

	assert_eq!(
		phase
			.services
			.users
			.password_hash(phase.user_a)
			.await?,
		phase.state.a_password_hash
	);

	assert_eq!(
		phase
			.services
			.users
			.password_hash(phase.user_b)
			.await?,
		phase.state.b_password_hash
	);

	assert_eq!(
		phase
			.services
			.users
			.all_device_ids(phase.user_a)
			.count()
			.await,
		2,
		"restart lost a user A device"
	);

	let (token_user, token_device, _) = phase
		.services
		.users
		.find_from_token(B_TOKEN)
		.await?;

	assert_eq!(token_user.as_str(), phase.user_b.as_str());
	assert_eq!(token_device.as_str(), phase.state.b_device);

	Ok(())
}

async fn complete_persisted_reset(phase: Phase<'_>) -> Result {
	let wrong_secret = Reset {
		sid: &phase.state.sid,
		client_secret: "email-reset-wrong-client-secret",
		new_password: A_WRONG_SECRET_PASSWORD,
		logout_devices: Some(true),
		substitution: phase.substitution,
	};

	let response = reset_password(phase.client, phase.base, wrong_secret).await?;

	assert_eq!(response, phase.state.masked, "wrong secret exposed a different failure");
	assert!(
		phase
			.services
			.threepid
			.session_validated(&phase.state.sid, CLIENT_SECRET)
			.await,
		"wrong secret consumed the valid proof"
	);

	assert_eq!(
		phase
			.services
			.users
			.password_hash(phase.user_a)
			.await?,
		phase.state.a_password_hash
	);

	assert_password(phase.services, phase.user_a, A_WRONG_SECRET_PASSWORD, false).await?;

	let reset = Reset {
		sid: &phase.state.sid,
		client_secret: CLIENT_SECRET,
		new_password: A_NEW_PASSWORD,
		logout_devices: Some(true),
		substitution: phase.substitution,
	};

	let response = reset_password(phase.client, phase.base, reset).await?;

	assert_eq!(response.0, 200, "logged-out password reset: {}", response.1);
	assert_password(phase.services, phase.user_a, A_OLD_PASSWORD, false).await?;
	assert_password(phase.services, phase.user_a, A_NEW_PASSWORD, true).await?;
	assert_eq!(
		phase
			.services
			.users
			.all_device_ids(phase.user_a)
			.count()
			.await,
		0,
		"logout_devices retained a user A device"
	);

	let replay = Reset {
		sid: &phase.state.sid,
		client_secret: CLIENT_SECRET,
		new_password: A_REPLAY_PASSWORD,
		logout_devices: Some(true),
		substitution: phase.substitution,
	};

	let response = reset_password(phase.client, phase.base, replay).await?;

	assert_eq!(response, phase.state.masked, "proof replay exposed a different failure");
	assert_password(phase.services, phase.user_a, A_NEW_PASSWORD, true).await?;
	assert_password(phase.services, phase.user_a, A_REPLAY_PASSWORD, false).await
}

async fn reject_unbound_reset(phase: Phase<'_>) -> Result {
	let unbound_secret = "email-reset-unbound-secret";
	let unbound = phase
		.services
		.threepid
		.create_or_reuse_pending(
			unbound_secret,
			Medium::Email,
			A_EMAIL,
			1,
			Duration::from_mins(5),
		)
		.await?;

	let unbound_token = unbound
		.freshly_minted_token
		.as_deref()
		.ok_or_else(|| err!("unbound password-reset proof did not mint a token"))?;

	phase
		.services
		.threepid
		.validate_pending_token(&unbound.sid, unbound_secret, unbound_token)
		.await?;

	phase
		.services
		.threepid
		.del_binding(phase.user_a, A_EMAIL)
		.await;

	let unbound = Reset {
		sid: &unbound.sid,
		client_secret: unbound_secret,
		new_password: A_UNBOUND_PASSWORD,
		logout_devices: Some(true),
		substitution: phase.substitution,
	};

	let response = reset_password(phase.client, phase.base, unbound).await?;

	assert_eq!(response, phase.state.masked, "unbound proof exposed a different failure");
	assert_password(phase.services, phase.user_a, A_NEW_PASSWORD, true).await?;
	assert_password(phase.services, phase.user_a, A_UNBOUND_PASSWORD, false).await?;

	let now = MilliSecondsSinceUnixEpoch::now();

	phase
		.services
		.threepid
		.put_binding(phase.user_a, A_EMAIL, Medium::Email, now, now)
		.await;

	Ok(())
}

async fn failed_password_write(
	services: &Services,
	client: &Client,
	base: &str,
	substitution: Substitution<'_>,
	masked: &(u16, String),
) -> Result {
	const CLIENT_SECRET: &str = "email-reset-failed-write-secret";

	let user = UserId::parse_with_server_name(
		"email-reset-non-password",
		services.globals.server_name(),
	)?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user),
			password: Some(PASSWORD_SENTINEL),
			origin: Some("jwt"),
			..Default::default()
		})
		.await?;

	let password_hash = services.users.password_hash(&user).await?;
	let now = MilliSecondsSinceUnixEpoch::now();

	assert_eq!(password_hash, PASSWORD_SENTINEL);
	assert_eq!(services.users.origin(&user).await?, "jwt");

	services
		.threepid
		.put_binding(&user, C_EMAIL, Medium::Email, now, now)
		.await;

	let pending = services
		.threepid
		.create_or_reuse_pending(CLIENT_SECRET, Medium::Email, C_EMAIL, 1, Duration::from_mins(5))
		.await?;

	let token = pending
		.freshly_minted_token
		.as_deref()
		.ok_or_else(|| err!("failed-write password-reset proof did not mint a token"))?;

	confirm_email(client, base, &pending.sid, CLIENT_SECRET, token).await?;

	let reset = Reset {
		sid: &pending.sid,
		client_secret: CLIENT_SECRET,
		new_password: C_FAILED_PASSWORD,
		logout_devices: Some(true),
		substitution,
	};

	let response = reset_password(client, base, reset).await?;
	let body = serde_json::from_str::<Value>(&response.1)?;

	assert_eq!(response.0, 400, "non-password-origin reset succeeded: {}", response.1);
	assert_eq!(body.get("errcode").and_then(Value::as_str), Some("M_INVALID_PARAM"));
	assert_eq!(services.users.password_hash(&user).await?, password_hash);
	assert_eq!(services.users.origin(&user).await?, "jwt");
	assert!(
		!services
			.threepid
			.session_validated(&pending.sid, CLIENT_SECRET)
			.await,
		"failed password write left the proof redeemable"
	);

	let replay = Reset {
		sid: &pending.sid,
		client_secret: CLIENT_SECRET,
		new_password: C_REPLAY_PASSWORD,
		logout_devices: Some(true),
		substitution,
	};

	let response = reset_password(client, base, replay).await?;

	assert_eq!(&response, masked, "failed-write proof replay exposed a different failure");
	assert_eq!(services.users.password_hash(&user).await?, password_hash);
	assert_eq!(services.users.origin(&user).await?, "jwt");

	Ok(())
}

async fn verify_device_modes(phase: Phase<'_>) -> Result {
	create_devices(phase.services, phase.user_a, &A_DEFAULT_TOKENS).await?;
	assert_eq!(
		phase
			.services
			.users
			.all_device_ids(phase.user_a)
			.count()
			.await,
		2
	);

	let keep_secret = "email-reset-keep-devices-secret";
	let keep = phase
		.services
		.threepid
		.create_or_reuse_pending(keep_secret, Medium::Email, A_EMAIL, 1, Duration::from_mins(5))
		.await?;

	let keep_token = keep
		.freshly_minted_token
		.as_deref()
		.ok_or_else(|| err!("keep-devices password-reset proof did not mint a token"))?;

	phase
		.services
		.threepid
		.validate_pending_token(&keep.sid, keep_secret, keep_token)
		.await?;

	let keep = Reset {
		sid: &keep.sid,
		client_secret: keep_secret,
		new_password: A_KEEP_DEVICES_PASSWORD,
		logout_devices: Some(false),
		substitution: phase.substitution,
	};

	let response = reset_password(phase.client, phase.base, keep).await?;

	assert_eq!(response.0, 200, "logout_devices=false password reset: {}", response.1);
	assert_password(phase.services, phase.user_a, A_NEW_PASSWORD, false).await?;
	assert_password(phase.services, phase.user_a, A_KEEP_DEVICES_PASSWORD, true).await?;
	assert_eq!(
		phase
			.services
			.users
			.all_device_ids(phase.user_a)
			.count()
			.await,
		2,
		"logout_devices=false removed a user A device"
	);

	for token in A_DEFAULT_TOKENS {
		assert!(
			phase
				.services
				.users
				.find_from_token(token)
				.await
				.is_ok(),
			"logout_devices=false removed user A token {token}"
		);
	}

	concurrent_completion(
		phase.services,
		phase.client,
		phase.base,
		phase.user_a,
		A_EMAIL,
		phase.substitution,
	)
	.await?;

	for token in A_INITIAL_TOKENS
		.into_iter()
		.chain(A_DEFAULT_TOKENS)
	{
		assert!(
			phase
				.services
				.users
				.find_from_token(token)
				.await
				.is_err(),
			"user A token survived logout_devices: {token}"
		);
	}

	Ok(())
}

async fn verify_authenticated_changes(phase: Phase<'_>) -> Result {
	assert_eq!(
		phase
			.services
			.users
			.password_hash(phase.user_b)
			.await?,
		phase.state.b_password_hash
	);

	assert_password(phase.services, phase.user_b, B_PASSWORD, true).await?;

	let (token_user, token_device, _) = phase
		.services
		.users
		.find_from_token(B_TOKEN)
		.await?;

	assert_eq!(token_user.as_str(), phase.user_b.as_str());
	assert_eq!(token_device.as_str(), phase.state.b_device);

	let b_session = password_uiaa_session(phase.client, phase.base, B_TOKEN).await?;
	let response = authenticated_password_change(AuthenticatedPasswordChange {
		client: phase.client,
		base: phase.base,
		user_id: phase.user_b,
		access_token: B_TOKEN,
		old_password: B_PASSWORD,
		new_password: B_NEW_PASSWORD,
		session: &b_session,
	})
	.await?;

	assert_eq!(response.0, 200, "authenticated password change: {}", response.1);
	assert_password(phase.services, phase.user_b, B_PASSWORD, false).await?;
	assert_password(phase.services, phase.user_b, B_NEW_PASSWORD, true).await?;

	let (token_user, token_device, _) = phase
		.services
		.users
		.find_from_token(B_TOKEN)
		.await?;

	assert_eq!(token_user.as_str(), phase.user_b.as_str());
	assert_eq!(token_device.as_str(), phase.state.b_device);

	let token = jwt_token(phase.user_b)?;
	let response =
		jwt_password_change(phase.client, phase.base, None, &token, B_TOKENLESS_JWT_PASSWORD)
			.await?;
	let body = serde_json::from_str::<Value>(&response.1)?;

	assert_eq!(response.0, 401, "tokenless JWT password change: {}", response.1);
	assert_eq!(body.get("errcode").and_then(Value::as_str), Some("M_MISSING_TOKEN"));
	assert_password(phase.services, phase.user_b, B_NEW_PASSWORD, true).await?;
	assert_password(phase.services, phase.user_b, B_TOKENLESS_JWT_PASSWORD, false).await?;

	let response =
		jwt_password_change(phase.client, phase.base, Some(B_TOKEN), &token, B_JWT_PASSWORD)
			.await?;

	assert_eq!(response.0, 200, "JWT password change: {}", response.1);
	assert_password(phase.services, phase.user_b, B_NEW_PASSWORD, false).await?;
	assert_password(phase.services, phase.user_b, B_JWT_PASSWORD, true).await?;

	let (token_user, token_device, _) = phase
		.services
		.users
		.find_from_token(B_TOKEN)
		.await?;

	assert_eq!(token_user.as_str(), phase.user_b.as_str());
	assert_eq!(token_device.as_str(), phase.state.b_device);

	Ok(())
}

async fn reject_deactivated_reset(phase: Phase<'_>) -> Result {
	let secret = "email-reset-deactivated-secret";

	phase
		.services
		.users
		.deactivate_account(phase.user_a)
		.await?;

	let pending = phase
		.services
		.threepid
		.create_or_reuse_pending(secret, Medium::Email, A_EMAIL, 1, Duration::from_mins(5))
		.await?;

	let token = pending
		.freshly_minted_token
		.as_deref()
		.ok_or_else(|| err!("deactivated password-reset proof did not mint a token"))?;

	phase
		.services
		.threepid
		.validate_pending_token(&pending.sid, secret, token)
		.await?;

	let reset = Reset {
		sid: &pending.sid,
		client_secret: secret,
		new_password: A_DEACTIVATED_PASSWORD,
		logout_devices: Some(true),
		substitution: phase.substitution,
	};

	let response = reset_password(phase.client, phase.base, reset).await?;
	let body = serde_json::from_str::<Value>(&response.1)?;

	assert_eq!(response.0, 403, "deactivated account password reset: {}", response.1);
	assert_eq!(body.get("errcode").and_then(Value::as_str), Some("M_USER_DEACTIVATED"));
	assert!(
		phase
			.services
			.users
			.is_deactivated(phase.user_a)
			.await?,
		"password reset reactivated a deactivated account"
	);

	Ok(())
}

fn jwt_token(user_id: &UserId) -> Result<String> {
	let claims = json!({"sub": user_id.localpart()});

	encode(&Header::default(), &claims, &EncodingKey::from_secret(JWT_SECRET.as_bytes()))
		.map_err(|error| err!("failed to mint JWT reauthentication token: {error}"))
}

async fn create_devices(services: &Services, user_id: &UserId, tokens: &[&str]) -> Result {
	for &token in tokens {
		services
			.users
			.create_device(user_id, None, (Some(token), None), None, None, None)
			.await?;
	}

	Ok(())
}

async fn concurrent_completion(
	services: &Services,
	client: &Client,
	base: &str,
	user_id: &UserId,
	email: &str,
	substitution: Substitution<'_>,
) -> Result {
	const CLIENT_SECRET: &str = "email-reset-concurrent-secret";
	const FIRST_PASSWORD: &str = "email-reset-concurrent-first";
	const SECOND_PASSWORD: &str = "email-reset-concurrent-second";
	const REPLAY_PASSWORD: &str = "email-reset-concurrent-replay";

	let pending = services
		.threepid
		.create_or_reuse_pending(CLIENT_SECRET, Medium::Email, email, 1, Duration::from_mins(5))
		.await?;

	let token = pending
		.freshly_minted_token
		.as_deref()
		.ok_or_else(|| err!("concurrent password-reset proof did not mint a token"))?;

	services
		.threepid
		.validate_pending_token(&pending.sid, CLIENT_SECRET, token)
		.await?;

	let first = Reset {
		sid: &pending.sid,
		client_secret: CLIENT_SECRET,
		new_password: FIRST_PASSWORD,
		logout_devices: None,
		substitution,
	};

	let second = Reset { new_password: SECOND_PASSWORD, ..first };
	let first = reset_password(client, base, first);
	let second = reset_password(client, base, second);
	let (first, second) = join(first, second).await;
	let first = first?;
	let second = second?;

	assert_ne!(
		first.0 == 200,
		second.0 == 200,
		"concurrent password resets returned statuses {} and {}",
		first.0,
		second.0
	);

	let (winner, loser) = if first.0 == 200 {
		(FIRST_PASSWORD, SECOND_PASSWORD)
	} else {
		(SECOND_PASSWORD, FIRST_PASSWORD)
	};

	assert_password(services, user_id, winner, true).await?;
	assert_password(services, user_id, loser, false).await?;
	assert_eq!(
		services
			.users
			.all_device_ids(user_id)
			.count()
			.await,
		0
	);

	let replay = Reset {
		sid: &pending.sid,
		client_secret: CLIENT_SECRET,
		new_password: REPLAY_PASSWORD,
		logout_devices: None,
		substitution,
	};

	let response = reset_password(client, base, replay).await?;

	assert_ne!(response.0, 200, "concurrent proof replay: {}", response.1);
	assert_password(services, user_id, winner, true).await?;
	assert_password(services, user_id, REPLAY_PASSWORD, false).await
}

async fn assert_password(
	services: &Services,
	user_id: &UserId,
	password: &str,
	matches: bool,
) -> Result {
	let password_hash = services.users.password_hash(user_id).await?;

	assert_eq!(
		verify_password(password, &password_hash).is_ok(),
		matches,
		"password verification result did not match expectation"
	);

	Ok(())
}
