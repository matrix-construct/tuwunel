#![cfg(feature = "email")]
#![expect(
	clippy::tests_outside_test_module,
	reason = "this integration target requires one top-level test entry point"
)]

use std::{env::var, path::PathBuf, sync::Arc, time::Duration};

use futures::future::join;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel_core::{
	Result, err,
	ruma::{DeviceId, OwnedUserId, UserId, thirdparty::Medium},
};
use tuwunel_service::{Services, threepid::UiaaKey, users::Register};

use self::harness::{
	CHILD_DB_ENV, CHILD_PHASE_ENV, ClaimState, DatabasePath, Registration, RegistrationConfig,
	read_claim_state, run_child, run_server, write_claim_state,
};

#[path = "email_registration_claim/harness.rs"]
mod harness;

const BURN_ATTEMPTS: usize = 5;
const CLIENT_SECRET: &str = "email-registration-claim-secret";
const EMAIL: &str = "registration-claim@example.test";
const EMAIL_STAGE: &str = "m.login.email.identity";
const FIRST_USERNAME: &str = "email_claim_first";
const PASSWORD: &str = "email-registration-claim-password";
const REGISTRATION_TOKEN: &str = "email-registration-token";
const REGISTRATION_TOKEN_STAGE: &str = "m.login.registration_token";
const SECOND_USERNAME: &str = "email_claim_second";
const TERMS_STAGE: &str = "m.login.terms";
const WRONG_SECRET: &str = "wrong-email-registration-secret";

const EMAIL_ONLY_CONFIG: RegistrationConfig = RegistrationConfig { terms: false, token: None };
const EMAIL_TERMS_CONFIG: RegistrationConfig = RegistrationConfig { terms: true, token: None };
const EMAIL_TOKEN_TERMS_CONFIG: RegistrationConfig = RegistrationConfig {
	terms: true,
	token: Some(REGISTRATION_TOKEN),
};

#[derive(Debug, Eq, PartialEq)]
struct ExternalFailure {
	status: u16,
	errcode: String,
	error: String,
}

/// Proves a verified email can authorize only one registration across restart.
///
/// Two UIAA sessions race for one proof before the owner finishes terms. The
/// database is reopened, then only the owner can register and bind the address.
#[test]
fn one_email_proof_has_one_registration_owner_across_restart() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		return email_registration_claim_child(&phase);
	}

	let claim_db = DatabasePath::new("restart");
	let state_path = claim_db.0.with_extension("state.json");

	run_child(&claim_db.0, "restart-first", Some(&state_path))?;
	run_child(&claim_db.0, "restart-second", Some(&state_path))?;

	let email_only_db = DatabasePath::new("email-only");

	run_child(&email_only_db.0, "email-only", None)?;

	let token_terms_db = DatabasePath::new("token-terms");

	run_child(&token_terms_db.0, "token-terms", None)
}

fn email_registration_claim_child(phase: &str) -> Result {
	let db_path = var(CHILD_DB_ENV)
		.map(PathBuf::from)
		.map_err(|e| err!("child database path is unavailable: {e}"))?;

	match phase {
		| "restart-first" => {
			let claim = run_server(&db_path, &["fresh"], EMAIL_TERMS_CONFIG, first_phase)?;

			write_claim_state(&claim)
		},
		| "restart-second" => {
			let claim = read_claim_state()?;

			run_server(&db_path, &[], EMAIL_TERMS_CONFIG, |services, client, base| {
				second_phase(services, client, base, claim)
			})
		},
		| "email-only" => run_server(&db_path, &["fresh"], EMAIL_ONLY_CONFIG, email_only_phase),
		| "token-terms" =>
			run_server(&db_path, &["fresh"], EMAIL_TOKEN_TERMS_CONFIG, token_terms_phase),
		| phase => Err(err!("unknown email registration claim child phase: {phase}")),
	}
}

async fn first_phase(
	services: Arc<Services>,
	client: Client,
	base: String,
) -> Result<ClaimState> {
	wait_until_ready(&client, &base).await?;

	let sid = seed_validated(&services, CLIENT_SECRET, EMAIL, 1).await?;

	let first = Registration {
		username: FIRST_USERNAME,
		session: begin_registration(&client, &base, FIRST_USERNAME).await?,
	};

	let second = Registration {
		username: SECOND_USERNAME,
		session: begin_registration(&client, &base, SECOND_USERNAME).await?,
	};

	assert_ne!(
		first.session, second.session,
		"distinct registrations must have distinct UIAA sessions"
	);

	let first_auth = email_auth(&sid, CLIENT_SECRET, Some(&first.session));
	let first_request = registration_body(first.username, Some(first_auth));
	let second_auth = email_auth(&sid, CLIENT_SECRET, Some(&second.session));
	let second_request = registration_body(second.username, Some(second_auth));
	let (first_response, second_response) = join(
		post_registration(&client, &base, &first_request),
		post_registration(&client, &base, &second_request),
	)
	.await;

	let (first_status, first_response) = first_response?;
	let (second_status, second_response) = second_response?;

	assert_eq!(first_status, 401, "first claim response: {first_response}");
	assert_eq!(second_status, 401, "second claim response: {second_response}");

	let first_owns = completed(&first_response, EMAIL_STAGE);
	let second_owns = completed(&second_response, EMAIL_STAGE);

	assert_ne!(
		first_owns, second_owns,
		"claim responses: first={first_response}, second={second_response}"
	);

	let (owner, loser) = if first_owns { (first, second) } else { (second, first) };
	let owner_user = registration_user_id(&services, owner.username)?;
	let loser_user = registration_user_id(&services, loser.username)?;

	assert!(
		!services.users.exists(&owner_user).await,
		"claim owner must not exist before completing UIAA"
	);
	assert!(
		!services.users.exists(&loser_user).await,
		"claim loser must not exist before completing UIAA"
	);

	Ok(ClaimState { sid, owner, loser })
}

async fn second_phase(
	services: Arc<Services>,
	client: Client,
	base: String,
	claim: ClaimState,
) -> Result {
	wait_until_ready(&client, &base).await?;

	let retry_auth = email_auth(&claim.sid, CLIENT_SECRET, Some(&claim.loser.session));
	let retry_request = registration_body(claim.loser.username, Some(retry_auth));
	let (retry_status, retry_response) =
		post_registration(&client, &base, &retry_request).await?;

	assert_eq!(retry_status, 401, "loser retry response: {retry_response}");
	assert!(
		!completed(&retry_response, EMAIL_STAGE),
		"losing UIAA session must not complete the email stage: {retry_response}"
	);
	let claimed_failure = external_failure(retry_status, &retry_response)?;

	assert_invalid_proof_liveness(&services, &client, &base, &claimed_failure).await?;

	let owner_user = registration_user_id(&services, claim.owner.username)?;
	let loser_user = registration_user_id(&services, claim.loser.username)?;

	assert!(
		!services.users.exists(&owner_user).await,
		"claim owner must remain unregistered after restart"
	);
	assert!(
		!services.users.exists(&loser_user).await,
		"claim loser must remain unregistered after restart"
	);

	let loser_terms = terms_auth(&claim.loser.session);
	let loser_request = registration_body(claim.loser.username, Some(loser_terms));
	let (loser_status, loser_response) =
		post_registration(&client, &base, &loser_request).await?;

	assert_eq!(loser_status, 401, "loser terms response: {loser_response}");
	assert!(
		completed(&loser_response, TERMS_STAGE),
		"losing UIAA session must retain its terms stage: {loser_response}"
	);
	assert!(
		!completed(&loser_response, EMAIL_STAGE),
		"losing UIAA session must not gain the email stage: {loser_response}"
	);
	assert!(
		!services.users.exists(&loser_user).await,
		"losing UIAA session must not create an account"
	);

	let owner_terms = terms_auth(&claim.owner.session);
	let owner_request = registration_body(claim.owner.username, Some(owner_terms));
	let (owner_status, owner_response) =
		post_registration(&client, &base, &owner_request).await?;

	assert_eq!(owner_status, 200, "owner terms response: {owner_response}");
	assert_eq!(
		owner_response
			.get("user_id")
			.and_then(Value::as_str),
		Some(owner_user.as_str()),
		"owner response must identify the registered account: {owner_response}"
	);

	assert!(
		services.users.exists(&owner_user).await,
		"claim owner account must exist after registration"
	);
	assert!(
		!services.users.exists(&loser_user).await,
		"claim loser account must remain absent"
	);

	let binding = services.threepid.user_id_for_email(EMAIL).await?;

	assert_eq!(
		binding.as_deref(),
		Some(&*owner_user),
		"verified email must bind to the claim owner"
	);

	assert_terms_first_registration(&services, &client, &base).await?;
	assert_claim_boundary_failure(&services, &client, &base).await?;

	Ok(())
}

async fn assert_invalid_proof_liveness(
	services: &Services,
	client: &Client,
	base: &str,
	claimed_failure: &ExternalFailure,
) -> Result {
	let wrong_secret_sid =
		seed_validated(services, CLIENT_SECRET, "wrong-secret-proof@example.test", 1).await?;

	let wrong_secret = invalid_proof_failure(
		services,
		client,
		base,
		"email_claim_wrong_secret",
		&wrong_secret_sid,
		WRONG_SECRET,
	)
	.await?;

	let expired = services
		.threepid
		.create_or_reuse_pending(
			CLIENT_SECRET,
			Medium::Email,
			"expired-proof@example.test",
			1,
			Duration::ZERO,
		)
		.await?;

	let expired = invalid_proof_failure(
		services,
		client,
		base,
		"email_claim_expired",
		&expired.sid,
		CLIENT_SECRET,
	)
	.await?;

	let burned = services
		.threepid
		.create_or_reuse_pending(
			CLIENT_SECRET,
			Medium::Email,
			"burned-proof@example.test",
			1,
			Duration::from_mins(10),
		)
		.await?;

	for _ in 0..BURN_ATTEMPTS {
		services
			.threepid
			.validate_pending_token(&burned.sid, CLIENT_SECRET, "wrong-token")
			.await
			.expect_err("an incorrect token must fail validation");
	}
	let burned = invalid_proof_failure(
		services,
		client,
		base,
		"email_claim_burned",
		&burned.sid,
		CLIENT_SECRET,
	)
	.await?;

	assert_eq!(
		&wrong_secret, claimed_failure,
		"wrong-secret failure must match a claimed-proof failure"
	);
	assert_eq!(
		&expired, claimed_failure,
		"expired-proof failure must match a claimed-proof failure"
	);
	assert_eq!(
		&burned, claimed_failure,
		"burned-proof failure must match a claimed-proof failure"
	);

	Ok(())
}

async fn invalid_proof_failure(
	services: &Services,
	client: &Client,
	base: &str,
	username: &str,
	sid: &str,
	client_secret: &str,
) -> Result<ExternalFailure> {
	let session = begin_registration(client, base, username).await?;
	let request =
		registration_body(username, Some(email_auth(sid, client_secret, Some(&session))));

	let (status, response) = post_registration(client, base, &request).await?;
	let user_id = registration_user_id(services, username)?;

	assert_eq!(status, 401, "invalid proof response: {response}");
	assert!(
		!completed(&response, EMAIL_STAGE),
		"invalid proof must not complete the email stage: {response}"
	);
	assert!(
		!services.users.exists(&user_id).await,
		"invalid proof must not create an account"
	);

	external_failure(status, &response)
}

async fn assert_terms_first_registration(
	services: &Services,
	client: &Client,
	base: &str,
) -> Result {
	let username = "email_claim_terms_first";
	let email = "terms-first@example.test";
	let sid = seed_validated(services, CLIENT_SECRET, email, 1).await?;
	let session = begin_registration(client, base, username).await?;
	let terms_request = registration_body(username, Some(terms_auth(&session)));
	let (terms_status, terms_response) = post_registration(client, base, &terms_request).await?;

	assert_eq!(terms_status, 401, "terms-first response: {terms_response}");
	assert!(
		completed(&terms_response, TERMS_STAGE),
		"terms-first flow must retain the terms stage: {terms_response}"
	);
	assert!(
		!completed(&terms_response, EMAIL_STAGE),
		"terms-first flow must still require email: {terms_response}"
	);

	let email_request =
		registration_body(username, Some(email_auth(&sid, CLIENT_SECRET, Some(&session))));

	let (status, response) = post_registration(client, base, &email_request).await?;

	assert_registered_and_bound(services, username, email, status, &response).await
}

async fn assert_claim_boundary_failure(
	services: &Services,
	client: &Client,
	base: &str,
) -> Result {
	let username = "email_claim_storage_failure";
	let sid =
		seed_validated(services, CLIENT_SECRET, "claim-storage-failure@example.test", 1).await?;

	let session = begin_registration(client, base, username).await?;

	services.db["threepidsid_pending"].insert(sid.as_str(), b"invalid-cbor");

	let request =
		registration_body(username, Some(email_auth(&sid, CLIENT_SECRET, Some(&session))));

	let (status, response) = post_registration(client, base, &request).await?;
	let user_id = registration_user_id(services, username)?;

	assert_ne!(status, 200, "claim storage failure response: {response}");
	assert!(
		!services.users.exists(&user_id).await,
		"claim storage failure must not create an account"
	);

	Ok(())
}

async fn email_only_phase(services: Arc<Services>, client: Client, base: String) -> Result {
	wait_until_ready(&client, &base).await?;

	let email = "email-only@example.test";
	let username = "email_claim_email_only";
	let sid = seed_validated(&services, CLIENT_SECRET, email, 1).await?;
	let session = begin_registration(&client, &base, username).await?;
	let request =
		registration_body(username, Some(email_auth(&sid, CLIENT_SECRET, Some(&session))));

	let (status, response) = post_registration(&client, &base, &request).await?;

	assert_registered_and_bound(&services, username, email, status, &response).await?;

	let no_session_email = "no-session@example.test";
	let no_session_username = "email_claim_no_session";
	let no_session_sid = seed_validated(&services, CLIENT_SECRET, no_session_email, 1).await?;
	let no_session_request = registration_body(
		no_session_username,
		Some(email_auth(&no_session_sid, CLIENT_SECRET, None)),
	);

	let (no_session_status, no_session_response) =
		post_registration(&client, &base, &no_session_request).await?;

	assert_registered_and_bound(
		&services,
		no_session_username,
		no_session_email,
		no_session_status,
		&no_session_response,
	)
	.await?;

	service_level_spent_claim_survives_full_register_failure(&services).await?;
	service_level_same_owner_claim_is_exclusive(&services).await
}

async fn token_terms_phase(services: Arc<Services>, client: Client, base: String) -> Result {
	wait_until_ready(&client, &base).await?;

	let username = "email_claim_token_terms";
	let email = "token-terms@example.test";
	let sid = seed_validated(&services, CLIENT_SECRET, email, 1).await?;
	let initial_request = registration_body(username, None);
	let (initial_status, initial_response) =
		post_registration(&client, &base, &initial_request).await?;

	assert_eq!(initial_status, 401, "token terms flow: {initial_response}");
	assert!(
		flow_has_exact_stages(&initial_response, &[
			REGISTRATION_TOKEN_STAGE,
			EMAIL_STAGE,
			TERMS_STAGE
		],),
		"token terms flow must require token, email, and terms: {initial_response}"
	);

	let session = response_session(&initial_response)?;

	let terms_request = registration_body(username, Some(terms_auth(&session)));
	let (terms_status, terms_response) =
		post_registration(&client, &base, &terms_request).await?;

	assert_eq!(terms_status, 401, "token terms stage: {terms_response}");
	assert!(
		completed(&terms_response, TERMS_STAGE),
		"token terms flow must retain the terms stage: {terms_response}"
	);

	let token_request = registration_body(username, Some(registration_token_auth(&session)));
	let (token_status, token_response) =
		post_registration(&client, &base, &token_request).await?;

	assert_eq!(token_status, 401, "registration token stage: {token_response}");
	assert!(
		completed(&token_response, TERMS_STAGE),
		"registration-token response must retain terms: {token_response}"
	);
	assert!(
		completed(&token_response, REGISTRATION_TOKEN_STAGE),
		"registration-token response must complete the token stage: {token_response}"
	);

	let email_request =
		registration_body(username, Some(email_auth(&sid, CLIENT_SECRET, Some(&session))));

	let (status, response) = post_registration(&client, &base, &email_request).await?;

	assert_registered_and_bound(&services, username, email, status, &response).await
}

async fn service_level_spent_claim_survives_full_register_failure(services: &Services) -> Result {
	let email = "spent-before-register-failure@example.test";
	let sid = seed_validated(services, CLIENT_SECRET, email, 1).await?;
	let server_user = UserId::parse_with_server_name("", services.globals.server_name())?;
	let server_device: &DeviceId = "".into();
	let owner: UiaaKey = (
		server_user.clone(),
		server_device.to_owned(),
		"service-level-full-register-owner".into(),
	);

	let loser: UiaaKey = (
		server_user,
		server_device.to_owned(),
		"service-level-full-register-loser".into(),
	);

	assert!(
		services
			.threepid
			.claim_validated(&sid, CLIENT_SECRET, owner.clone())
			.await?,
		"validated proof must be claimable by its owner"
	);

	let association = services.threepid.redeem_claim(&owner).await?;
	assert_eq!(association.medium, Medium::Email, "redeemed proof must preserve its medium");
	assert_eq!(association.address, email, "redeemed proof must preserve its email address");

	let remote_user = UserId::parse("@claim_failure:elsewhere.test")?;
	let register_result = services
		.users
		.full_register(Register {
			user_id: Some(&remote_user),
			password: Some(PASSWORD),
			..Default::default()
		})
		.await;

	register_result.expect_err("registration for a remote user must fail");
	assert!(
		!services.users.exists(&remote_user).await,
		"failed registration must not create the remote user"
	);
	assert!(
		!services
			.threepid
			.claim_validated(&sid, CLIENT_SECRET, loser)
			.await?,
		"spent proof must not be claimable by another owner"
	);

	services
		.threepid
		.redeem_claim(&owner)
		.await
		.expect_err("spent proof must not be redeemable again");

	let fresh_email = "fresh-after-register-failure@example.test";
	let fresh_sid = seed_validated(services, CLIENT_SECRET, fresh_email, 1).await?;
	assert!(
		services
			.threepid
			.claim_validated(&fresh_sid, CLIENT_SECRET, owner.clone())
			.await?,
		"fresh proof must remain claimable after the failed registration"
	);

	let fresh_association = services.threepid.redeem_claim(&owner).await?;

	assert_eq!(
		fresh_association.address, fresh_email,
		"fresh proof must preserve its email address"
	);

	Ok(())
}

async fn service_level_same_owner_claim_is_exclusive(services: &Services) -> Result {
	let first_email = "same-owner-first@example.test";
	let second_email = "same-owner-second@example.test";
	let first_sid = seed_validated(services, CLIENT_SECRET, first_email, 1).await?;
	let second_sid = seed_validated(services, CLIENT_SECRET, second_email, 1).await?;
	let server_user = UserId::parse_with_server_name("", services.globals.server_name())?;
	let server_device: &DeviceId = "".into();
	let owner: UiaaKey =
		(server_user, server_device.to_owned(), "service-level-same-owner-race".into());

	let (first_claimed, second_claimed) = join(
		services
			.threepid
			.claim_validated(&first_sid, CLIENT_SECRET, owner.clone()),
		services
			.threepid
			.claim_validated(&second_sid, CLIENT_SECRET, owner.clone()),
	)
	.await;

	let first_claimed = first_claimed?;
	let second_claimed = second_claimed?;

	assert_ne!(
		first_claimed, second_claimed,
		"one owner must not claim two proofs concurrently"
	);

	let expected_email = if first_claimed { first_email } else { second_email };
	let association = services.threepid.redeem_claim(&owner).await?;

	assert_eq!(association.medium, Medium::Email, "winning proof must preserve its medium");
	assert_eq!(
		association.address, expected_email,
		"redeemed association must match the winning proof"
	);

	Ok(())
}

async fn seed_validated(
	services: &Services,
	client_secret: &str,
	email: &str,
	send_attempt: u64,
) -> Result<String> {
	let pending = services
		.threepid
		.create_or_reuse_pending(
			client_secret,
			Medium::Email,
			email,
			send_attempt,
			Duration::from_mins(10),
		)
		.await?;

	let token = pending
		.freshly_minted_token
		.as_deref()
		.ok_or_else(|| err!("pending verification did not mint a token"))?;

	services
		.threepid
		.validate_pending_token(&pending.sid, client_secret, token)
		.await?;

	Ok(pending.sid)
}

async fn assert_registered_and_bound(
	services: &Services,
	username: &str,
	email: &str,
	status: u16,
	response: &Value,
) -> Result {
	let user_id = registration_user_id(services, username)?;

	assert_eq!(status, 200, "registration response: {response}");
	assert_eq!(
		response.get("user_id").and_then(Value::as_str),
		Some(user_id.as_str()),
		"registration response must identify the created account: {response}"
	);
	assert!(
		services.users.exists(&user_id).await,
		"successful registration must create the account"
	);
	let binding = services.threepid.user_id_for_email(email).await?;

	assert_eq!(
		binding.as_deref(),
		Some(&*user_id),
		"successful registration must bind the verified email"
	);

	Ok(())
}

async fn begin_registration(client: &Client, base: &str, username: &str) -> Result<String> {
	let request = registration_body(username, None);
	let (status, response) = post_registration(client, base, &request).await?;

	assert_eq!(status, 401, "initial registration response: {response}");

	response
		.get("session")
		.and_then(Value::as_str)
		.map(ToOwned::to_owned)
		.ok_or_else(|| err!("initial registration response omitted its session: {response}"))
}

async fn post_registration(client: &Client, base: &str, request: &Value) -> Result<(u16, Value)> {
	let url = format!("{base}/_matrix/client/v3/register");
	let response = client
		.post(url)
		.header("Connection", "close")
		.json(request)
		.send()
		.await?;

	let status = response.status().as_u16();
	let response = response.json().await?;

	Ok((status, response))
}

fn registration_body(username: &str, auth: Option<Value>) -> Value {
	let auth = auth.unwrap_or(Value::Null);

	json!({
		"username": username,
		"password": PASSWORD,
		"inhibit_login": true,
		"auth": auth,
	})
}

fn email_auth(sid: &str, client_secret: &str, session: Option<&str>) -> Value {
	let mut auth = json!({
		"type": EMAIL_STAGE,
		"threepid_creds": {
			"sid": sid,
			"client_secret": client_secret,
		},
	});

	if let Some(session) = session {
		auth["session"] = session.into();
	}

	auth
}

fn terms_auth(session: &str) -> Value {
	json!({
		"type": TERMS_STAGE,
		"session": session,
	})
}

fn registration_token_auth(session: &str) -> Value {
	json!({
		"type": REGISTRATION_TOKEN_STAGE,
		"token": REGISTRATION_TOKEN,
		"session": session,
	})
}

fn registration_user_id(services: &Services, username: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(username, services.globals.server_name()).map_err(Into::into)
}

fn completed(response: &Value, stage: &str) -> bool {
	response
		.get("completed")
		.and_then(Value::as_array)
		.is_some_and(|completed| {
			completed
				.iter()
				.any(|value| value.as_str() == Some(stage))
		})
}

fn flow_has_exact_stages(response: &Value, required: &[&str]) -> bool {
	response
		.get("flows")
		.and_then(Value::as_array)
		.is_some_and(|flows| {
			flows.iter().any(|flow| {
				flow.get("stages")
					.and_then(Value::as_array)
					.is_some_and(|stages| stages_match(stages, required))
			})
		})
}

fn stages_match(stages: &[Value], required: &[&str]) -> bool {
	stages.len() == required.len()
		&& required.iter().all(|required| {
			stages
				.iter()
				.any(|stage| stage.as_str() == Some(*required))
		})
}

fn response_session(response: &Value) -> Result<String> {
	response
		.get("session")
		.and_then(Value::as_str)
		.map(ToOwned::to_owned)
		.ok_or_else(|| err!("UIAA response omitted its session: {response}"))
}

fn external_failure(status: u16, response: &Value) -> Result<ExternalFailure> {
	let errcode = response
		.get("errcode")
		.and_then(Value::as_str)
		.map(ToOwned::to_owned)
		.ok_or_else(|| err!("UIAA failure omitted errcode: {response}"))?;

	let error = response
		.get("error")
		.and_then(Value::as_str)
		.map(ToOwned::to_owned)
		.ok_or_else(|| err!("UIAA failure omitted error: {response}"))?;

	Ok(ExternalFailure { status, errcode, error })
}

async fn wait_until_ready(client: &Client, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	timeout(Duration::from_secs(10), async {
		while client
			.get(&url)
			.header("Connection", "close")
			.send()
			.await
			.is_err()
		{
			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))?;

	Ok(())
}
