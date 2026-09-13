use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64encode};
use ruma::thirdparty::Medium;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tuwunel_core::{
	Err, Result, implement,
	result::NotFound,
	smallstr::SmallString,
	utils::{
		self,
		hash::sha256,
		time::{timepoint_from_now, timepoint_has_passed},
	},
};
use tuwunel_database::{Cbor, Deserialized};

use super::{Association, UiaaKey};

type ClaimSid = SmallString<[u8; 43]>;

/// Characters minted for the single-use, server-private validation token.
const TOKEN_LENGTH: usize = 48;

/// Failed-validation ceiling: the session self-destructs once this many wrong
/// submissions have been counted, so the Nth burns and N-1 are tolerated. Caps
/// token brute-force (mirrors the device-grant ceiling).
const MAX_VERIFY_ATTEMPTS: u32 = 5;

/// Persistence lifetime shared by UIAA sessions and their threepid claims.
const UIAA_SESSION_TTL: Duration = Duration::from_hours(24);

/// Single-use state of a validated pending session.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
enum PendingUse {
	#[default]
	Available,
	Claimed(Box<UiaaKey>),
	Spent,
}

/// CBOR value of a `threepidsid_pending` row. The whole row carries a TTL via
/// `expires_at` so a validated-but-unconsumed session self-reaps rather than
/// leaking.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Pending {
	client_secret: String,
	medium: Medium,
	address: String,
	token: String,
	send_attempt: u64,
	attempts: u32,
	validated_at: Option<SystemTime>,
	expires_at: Option<SystemTime>,
	#[serde(default)]
	use_state: PendingUse,
}

/// Result of [`create_or_reuse_pending`]: the session id to hand the client,
/// and the freshly minted token when a new message must be sent. A reused
/// session yields `None`, signalling no new mail.
#[derive(Clone, Debug)]
pub struct PendingOutcome {
	pub sid: String,
	pub freshly_minted_token: Option<String>,
}

/// Open a pending verification, or reuse an in-flight one for the same
/// request identity. The session id is derived from `(medium, address,
/// client_secret)`, so a resubmit collides on the same row: a non-validated
/// session whose `send_attempt` did not advance returns the same `sid` with no
/// new token (and thus no new mail), per the send-attempt dedup rule.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, client_secret))]
pub async fn create_or_reuse_pending(
	&self,
	client_secret: &str,
	medium: Medium,
	address: &str,
	send_attempt: u64,
	ttl: Duration,
) -> Result<PendingOutcome> {
	let sid = derive_sid(&medium, address, client_secret);
	let _pending_lock = self.pending_mutex.lock(&sid).await;

	match self.get_pending(&sid).await {
		| Err(error) if error.is_not_found() => (),
		| Err(error) => return Err(error),
		| Ok(existing) if expired(&existing) => {
			self.delete_pending_state(&sid, &existing).await?;
		},
		| Ok(existing) => {
			if !matches!(existing.use_state, PendingUse::Available) {
				return Err!(Request(ThreepidAuthFailed(
					"The verification session has already been used"
				)));
			}

			if existing.validated_at.is_none() && send_attempt <= existing.send_attempt {
				return Ok(PendingOutcome { sid, freshly_minted_token: None });
			}
		},
	}

	let token = utils::random_string(TOKEN_LENGTH);
	let expires_at = Some(timepoint_from_now(ttl)?);
	let pending = Pending {
		client_secret: client_secret.to_owned(),
		medium,
		address: address.to_owned(),
		token: token.clone(),
		send_attempt,
		attempts: 0,
		validated_at: None,
		expires_at,
		use_state: PendingUse::Available,
	};

	self.persist_pending(&sid, &pending);

	Ok(PendingOutcome { sid, freshly_minted_token: Some(token) })
}

/// Validate a submitted token against a pending session. A wrong
/// `client_secret` or `token` counts toward the attempt ceiling and burns the
/// session once exceeded; the caller learns nothing about session or token
/// liveness beyond pass or fail.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, client_secret, token))]
pub async fn validate_pending_token(
	&self,
	sid: &str,
	client_secret: &str,
	token: &str,
) -> Result<()> {
	let _pending_lock = self.pending_mutex.lock(sid).await;
	let pending = self.get_pending(sid).await?;

	if expired(&pending) {
		self.delete_pending_state(sid, &pending).await?;

		return Err!(Request(NotFound("The verification session has expired")));
	}

	if !matches!(pending.use_state, PendingUse::Available) {
		return Err!(Request(ThreepidAuthFailed(
			"The verification session has already been used"
		)));
	}

	if pending.validated_at.is_some() {
		return Err!(Request(ThreepidAuthFailed(
			"The verification session has already been validated"
		)));
	}

	let secret_ok = ct_eq(&pending.client_secret, client_secret);
	let token_ok = ct_eq(&pending.token, token);

	if !secret_ok || !token_ok {
		let attempts = pending.attempts.saturating_add(1);
		match attempts >= MAX_VERIFY_ATTEMPTS {
			| true => self.delete_pending_state(sid, &pending).await?,
			| false => self.persist_pending(sid, &Pending { attempts, ..pending }),
		}

		return Err!(Request(ThreepidAuthFailed("Invalid verification token")));
	}

	let validated_at = Some(SystemTime::now());
	self.persist_pending(sid, &Pending { validated_at, ..pending });

	Ok(())
}

/// Exclusively claim a validated pending session for one UIAA transaction.
///
/// Invalid or unavailable proofs return `false`; storage and decoding failures
/// remain errors so registration cannot silently continue through them.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, client_secret, claim))]
pub async fn claim_validated(
	&self,
	sid: &str,
	client_secret: &str,
	claim: UiaaKey,
) -> Result<bool> {
	let _pending_lock = self.pending_mutex.lock(sid).await;
	let pending = match self.get_pending(sid).await {
		| Ok(pending) => pending,
		| Err(error) if error.is_not_found() => return Ok(false),
		| Err(error) => return Err(error),
	};

	if expired(&pending) {
		self.delete_pending_state(sid, &pending).await?;

		return Ok(false);
	}

	if !ct_eq(&pending.client_secret, client_secret) {
		return Ok(false);
	}

	if pending.validated_at.is_none() {
		return Ok(false);
	}

	match &pending.use_state {
		| PendingUse::Available => (),
		| PendingUse::Claimed(owner) if owner.as_ref() == &claim => (),
		| PendingUse::Claimed(_) | PendingUse::Spent => return Ok(false),
	}

	let _claim_lock = self.claim_mutex.lock(&claim).await;

	if self
		.claim_sid(&claim)
		.await?
		.is_some_and(|claimed_sid| claimed_sid != sid)
	{
		return Ok(false);
	}

	let expires_at = Some(timepoint_from_now(UIAA_SESSION_TTL)?).max(pending.expires_at);
	let mut txn = self.db.database.txn();

	txn.put_raw(&self.db.userdevicesessionid_threepid, &claim, sid);

	let pending = Pending {
		expires_at,
		use_state: PendingUse::Claimed(Box::new(claim)),
		..pending
	};

	txn.raw_put(&self.db.threepidsid_pending, sid, Cbor(&pending));
	txn.execute();

	Ok(true)
}

/// Refresh a claim that is still owned by one UIAA transaction.
///
/// Rewriting both rows keeps their persistence lifetime aligned with later
/// successful stages that refresh the owning UIAA session.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, claim))]
pub async fn refresh_claim(&self, claim: &UiaaKey) -> Result<bool> {
	let Some(sid) = self.claim_sid(claim).await? else {
		return Ok(false);
	};

	let _pending_lock = self.pending_mutex.lock(sid.as_str()).await;
	let _claim_lock = self.claim_mutex.lock(claim).await;

	if self.claim_sid(claim).await?.as_deref() != Some(sid.as_str()) {
		return Ok(false);
	}

	let pending = match self.get_pending(&sid).await {
		| Ok(pending) => pending,
		| Err(error) if error.is_not_found() => {
			self.delete_claim_index(claim);

			return Ok(false);
		},
		| Err(error) => return Err(error),
	};

	if expired(&pending) {
		self.delete_pending_rows(&sid, Some(claim));

		return Ok(false);
	}

	if !matches!(&pending.use_state, PendingUse::Claimed(owner) if owner.as_ref() == claim) {
		self.delete_claim_index(claim);

		return Ok(false);
	}

	let expires_at = Some(timepoint_from_now(UIAA_SESSION_TTL)?).max(pending.expires_at);
	let pending = Pending { expires_at, ..pending };
	let mut txn = self.db.database.txn();

	txn.raw_put(&self.db.threepidsid_pending, &sid, Cbor(&pending));
	txn.put_raw(&self.db.userdevicesessionid_threepid, claim, &sid);
	txn.execute();

	Ok(true)
}

/// Spends the validated threepid owned by one UIAA transaction.
///
/// Redemption atomically records the pending proof as spent and removes the
/// claim index, so retries cannot yield the association again.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, claim))]
pub async fn redeem_claim(&self, claim: &UiaaKey) -> Result<Association> {
	let sid = self
		.db
		.userdevicesessionid_threepid
		.qry(claim)
		.await
		.deserialized::<ClaimSid>()?;

	let _pending_lock = self.pending_mutex.lock(sid.as_str()).await;
	let _claim_lock = self.claim_mutex.lock(claim).await;
	let current_sid = self
		.db
		.userdevicesessionid_threepid
		.qry(claim)
		.await
		.deserialized::<ClaimSid>()?;

	if current_sid != sid {
		return Err!(Request(ThreepidAuthFailed("The verification session claim has changed")));
	}

	let pending = match self.get_pending(&sid).await {
		| Ok(pending) => pending,
		| Err(error) if error.is_not_found() => {
			self.delete_claim_index(claim);

			return Err(error);
		},
		| Err(error) => return Err(error),
	};

	if expired(&pending) {
		self.delete_pending_rows(&sid, Some(claim));

		return Err!(Request(NotFound("The verification session has expired")));
	}

	if !matches!(&pending.use_state, PendingUse::Claimed(owner) if owner.as_ref() == claim) {
		self.delete_claim_index(claim);

		return Err!(Request(ThreepidAuthFailed(
			"The verification session is not owned by this transaction"
		)));
	}

	let association = Association {
		medium: pending.medium.clone(),
		address: pending.address.clone(),
	};

	let pending = Pending { use_state: PendingUse::Spent, ..pending };
	let mut txn = self.db.database.txn();

	txn.raw_put(&self.db.threepidsid_pending, &sid, Cbor(&pending));
	txn.del(&self.db.userdevicesessionid_threepid, claim);
	txn.execute();

	Ok(association)
}

/// Spends an unclaimed validated session directly, returning its association.
///
/// The pending row remains as a spent tombstone until expiry so replayed
/// requests fail closed instead of reusing a previously accepted proof.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, client_secret))]
pub async fn redeem_validated(&self, sid: &str, client_secret: &str) -> Result<Association> {
	let _pending_lock = self.pending_mutex.lock(sid).await;
	let pending = self.get_pending(sid).await?;

	if expired(&pending) {
		self.delete_pending_state(sid, &pending).await?;

		return Err!(Request(NotFound("The verification session has expired")));
	}

	if !ct_eq(&pending.client_secret, client_secret) {
		return Err!(Request(ThreepidAuthFailed("Client secret does not match")));
	}

	if pending.validated_at.is_none() {
		return Err!(Request(ThreepidAuthFailed("The address has not been validated")));
	}

	if !matches!(pending.use_state, PendingUse::Available) {
		return Err!(Request(ThreepidAuthFailed(
			"The verification session has already been used"
		)));
	}

	let association = Association {
		medium: pending.medium.clone(),
		address: pending.address.clone(),
	};

	self.persist_pending(sid, &Pending { use_state: PendingUse::Spent, ..pending });

	Ok(association)
}

/// Reports whether an unclaimed pending session is ready for UIAA.
///
/// This non-consuming gate maps wrong secrets, expired or unknown sessions,
/// spent proofs, and storage failures to `false`, revealing no extra liveness.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, client_secret))]
pub async fn session_validated(&self, sid: &str, client_secret: &str) -> bool {
	let Ok(pending) = self.get_pending(sid).await else {
		return false;
	};

	!expired(&pending)
		&& ct_eq(&pending.client_secret, client_secret)
		&& pending.validated_at.is_some()
		&& matches!(pending.use_state, PendingUse::Available)
}

#[implement(super::Service)]
fn persist_pending(&self, sid: &str, pending: &Pending) {
	self.db
		.threepidsid_pending
		.raw_put(sid, Cbor(pending));
}

#[implement(super::Service)]
async fn delete_pending_state(&self, sid: &str, pending: &Pending) -> Result<()> {
	let PendingUse::Claimed(claim) = &pending.use_state else {
		self.delete_pending_rows(sid, None);

		return Ok(());
	};

	let claim = claim.as_ref();
	let _claim_lock = self.claim_mutex.lock(claim).await;
	let claim = self
		.claim_sid(claim)
		.await?
		.as_deref()
		.is_some_and(|claimed_sid| claimed_sid == sid)
		.then_some(claim);

	self.delete_pending_rows(sid, claim);

	Ok(())
}

#[implement(super::Service)]
fn delete_pending_rows(&self, sid: &str, claim: Option<&UiaaKey>) {
	let mut txn = self.db.database.txn();
	txn.del_raw(&self.db.threepidsid_pending, sid);

	if let Some(claim) = claim {
		txn.del(&self.db.userdevicesessionid_threepid, claim);
	}

	txn.execute();
}

#[implement(super::Service)]
fn delete_claim_index(&self, claim: &UiaaKey) { self.db.userdevicesessionid_threepid.del(claim); }

#[implement(super::Service)]
async fn claim_sid(&self, claim: &UiaaKey) -> Result<Option<ClaimSid>> {
	self.db
		.userdevicesessionid_threepid
		.qry(claim)
		.await
		.deserialized::<ClaimSid>()
		.optional()
}

#[implement(super::Service)]
async fn get_pending(&self, sid: &str) -> Result<Pending> {
	self.db
		.threepidsid_pending
		.get(sid)
		.await
		.deserialized::<Cbor<_>>()
		.map(|Cbor(pending)| pending)
}

/// Deterministic session id binding the request identity to one storage key.
fn derive_sid(medium: &Medium, address: &str, client_secret: &str) -> String {
	let parts = [medium.as_str().as_bytes(), address.as_bytes(), client_secret.as_bytes()];
	let digest = sha256::delimited(parts.into_iter());

	b64encode.encode(digest)
}

fn expired(pending: &Pending) -> bool {
	pending
		.expires_at
		.is_some_and(timepoint_has_passed)
}

fn ct_eq(a: &str, b: &str) -> bool { a.as_bytes().ct_eq(b.as_bytes()).into() }

#[cfg(test)]
mod tests {
	use std::time::SystemTime;

	use ruma::{device_id, thirdparty::Medium, user_id};
	use serde::Serialize;
	use tuwunel_database::{Cbor, deserialize_from_slice, serialize_to_vec};

	use super::{Pending, PendingUse};

	#[derive(Serialize)]
	struct LegacyPending {
		client_secret: String,
		medium: Medium,
		address: String,
		token: String,
		send_attempt: u64,
		attempts: u32,
		validated_at: Option<SystemTime>,
		expires_at: Option<SystemTime>,
	}

	fn pending(use_state: PendingUse) -> Pending {
		Pending {
			client_secret: "secret".into(),
			medium: Medium::Email,
			address: "user@example.com".into(),
			token: "token".into(),
			send_attempt: 1,
			attempts: 0,
			validated_at: None,
			expires_at: None,
			use_state,
		}
	}

	fn round_trip(pending: Pending) -> Pending {
		let encoded = serialize_to_vec(Cbor(pending)).expect("pending row should serialize");
		let Cbor(pending): Cbor<Pending> =
			deserialize_from_slice(&encoded).expect("pending row should deserialize");

		pending
	}

	#[test]
	fn legacy_pending_defaults_to_available() {
		let legacy = LegacyPending {
			client_secret: "secret".into(),
			medium: Medium::Email,
			address: "user@example.com".into(),
			token: "token".into(),
			send_attempt: 1,
			attempts: 0,
			validated_at: None,
			expires_at: None,
		};

		let encoded =
			serialize_to_vec(Cbor(legacy)).expect("legacy pending row should serialize");

		let Cbor(pending): Cbor<Pending> =
			deserialize_from_slice(&encoded).expect("legacy pending row should deserialize");

		assert_eq!(pending.use_state, PendingUse::Available);
	}

	#[test]
	fn claimed_pending_round_trip_preserves_exact_key() {
		let claim = (
			user_id!("@owner:example.org").to_owned(),
			device_id!("DEVICE").to_owned(),
			"0123456789abcdefghijklmnopqrstuv".into(),
		);
		let pending = round_trip(pending(PendingUse::Claimed(Box::new(claim.clone()))));

		assert_eq!(pending.use_state, PendingUse::Claimed(Box::new(claim)));
	}

	#[test]
	fn spent_pending_round_trip_preserves_tombstone() {
		let pending = round_trip(pending(PendingUse::Spent));

		assert_eq!(pending.use_state, PendingUse::Spent);
	}
}
