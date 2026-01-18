pub mod association;

use std::{
	sync::{Arc, Mutex},
	time::SystemTime,
};

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, at, implement, utils::stream::TryExpect};
use tuwunel_database::{Cbor, Deserialized, Map};
use url::Url;

use super::{Provider, Providers, UserInfo, unique_id};
use crate::SelfServices;

pub struct Sessions {
	_services: SelfServices,
	association_pending: Mutex<association::Pending>,
	providers: Arc<Providers>,
	db: Data,
}

struct Data {
	oauthid_session: Arc<Map>,
	oauthuniqid_oauthid: Arc<Map>,
	userid_oauthid: Arc<Map>,
}

/// Session ultimately represents an OAuth authorization session yielding an
/// associated matrix user registration.
///
/// Mixed-use structure capable of deserializing response values, maintaining
/// the state between authorization steps, and maintaining the association to
/// the matrix user until deactivation or revocation.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Session {
	/// Identity Provider ID (the `client_id` in the configuration) associated
	/// with this session.
	pub idp_id: Option<String>,

	/// Session ID used as the index key for this session itself.
	pub sess_id: Option<String>,

	/// Token type (bearer, mac, etc).
	pub token_type: Option<String>,

	/// Access token to the provider.
	pub access_token: Option<String>,

	/// Duration in seconds the access_token is valid for.
	pub expires_in: Option<u64>,

	/// Point in time that the access_token expires.
	pub expires_at: Option<SystemTime>,

	/// Token used to refresh the access_token.
	pub refresh_token: Option<String>,

	/// Duration in seconds the refresh_token is valid for
	pub refresh_token_expires_in: Option<u64>,

	/// Point in time that the refresh_token expires.
	pub refresh_token_expires_at: Option<SystemTime>,

	/// Access scope actually granted (if supported).
	pub scope: Option<String>,

	/// Redirect URL
	pub redirect_url: Option<Url>,

	/// Challenge preimage
	pub code_verifier: Option<String>,

	/// Random string passed exclusively in the grant session cookie.
	pub cookie_nonce: Option<String>,

	/// Random single-use string passed in the provider redirect.
	pub query_nonce: Option<String>,

	/// Point in time the authorization grant session expires.
	pub authorize_expires_at: Option<SystemTime>,

	/// Associated User Id registration.
	pub user_id: Option<OwnedUserId>,

	/// Last userinfo response persisted here.
	pub user_info: Option<UserInfo>,
}

/// Number of characters generated for our code_verifier. The code_verifier is a
/// random string which must be between 43 and 128 characters.
pub const CODE_VERIFIER_LENGTH: usize = 64;

/// Number of characters we will generate for the Session ID.
pub const SESSION_ID_LENGTH: usize = 32;

#[implement(Sessions)]
pub(super) fn build(args: &crate::Args<'_>, providers: Arc<Providers>) -> Self {
	Self {
		_services: args.services.clone(),
		association_pending: Default::default(),
		providers,
		db: Data {
			oauthid_session: args.db["oauthid_session"].clone(),
			oauthuniqid_oauthid: args.db["oauthuniqid_oauthid"].clone(),
			userid_oauthid: args.db["userid_oauthid"].clone(),
		},
	}
}

#[implement(Sessions)]
pub fn users(&self) -> impl Stream<Item = OwnedUserId> + Send {
	self.db
		.userid_oauthid
		.keys()
		.expect_ok()
		.map(UserId::to_owned)
}

/// Delete database state for the session.
#[implement(Sessions)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn delete(&self, sess_id: &str) {
	let Ok(session) = self.get(sess_id).await else {
		return;
	};

	// Check the user_id still points to this sess_id before deleting. If not, the
	// association was updated to a newer session.
	if let Some(user_id) = session.user_id.as_deref() {
		if let Ok(assoc_id) = self.get_sess_id_by_user(user_id).await {
			if assoc_id == sess_id {
				self.db.userid_oauthid.remove(user_id);
			}
		}
	}

	// Check the unique identity still points to this sess_id before deleting. If
	// not, the association was updated to a newer session.
	if let Some(idp_id) = session.idp_id.as_ref() {
		if let Ok(provider) = self.providers.get(idp_id).await {
			if let Ok(unique_id) = unique_id((&provider, &session)) {
				if let Ok(assoc_id) = self.get_sess_id_by_unique_id(&unique_id).await {
					if assoc_id == sess_id {
						self.db.oauthuniqid_oauthid.remove(&unique_id);
					}
				}
			}
		}
	}

	self.db.oauthid_session.remove(sess_id);
}

/// Create or overwrite database state for the session.
#[implement(Sessions)]
#[tracing::instrument(level = "info", skip(self))]
pub async fn put(&self, sess_id: &str, session: &Session) {
	self.db
		.oauthid_session
		.raw_put(sess_id, Cbor(session));

	if let Some(user_id) = session.user_id.as_deref() {
		self.db.userid_oauthid.insert(user_id, sess_id);
	}

	if let Some(idp_id) = session.idp_id.as_ref() {
		if let Ok(provider) = self.providers.get(idp_id).await {
			if let Ok(unique_id) = unique_id((&provider, session)) {
				self.db
					.oauthuniqid_oauthid
					.insert(&unique_id, sess_id);
			}
		}
	}
}

/// Fetch database state for a session from its associated `(iss,sub)`, in case
/// `sess_id` is not known.
#[implement(Sessions)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "debug"))]
pub async fn get_by_unique_id(&self, unique_id: &str) -> Result<Session> {
	self.get_sess_id_by_unique_id(unique_id)
		.and_then(async |sess_id| self.get(&sess_id).await)
		.await
}

/// Fetch database state for a session from its associated `user_id`, in case
/// `sess_id` is not known.
#[implement(Sessions)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "debug"))]
pub async fn get_by_user(&self, user_id: &UserId) -> Result<Session> {
	self.get_sess_id_by_user(user_id)
		.and_then(async |sess_id| self.get(&sess_id).await)
		.await
}

/// Fetch database state for a session from its `sess_id`.
#[implement(Sessions)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "debug"))]
pub async fn get(&self, sess_id: &str) -> Result<Session> {
	self.db
		.oauthid_session
		.get(sess_id)
		.await
		.deserialized::<Cbor<_>>()
		.map(at!(0))
}

/// Resolve the `sess_id` from an associated `user_id`.
#[implement(Sessions)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "debug"))]
pub async fn get_sess_id_by_user(&self, user_id: &UserId) -> Result<String> {
	self.db
		.userid_oauthid
		.get(user_id)
		.await
		.deserialized()
}

/// Resolve the `sess_id` from an associated provider issuer and subject hash.
#[implement(Sessions)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "debug"))]
pub async fn get_sess_id_by_unique_id(&self, unique_id: &str) -> Result<String> {
	self.db
		.oauthuniqid_oauthid
		.get(unique_id)
		.await
		.deserialized()
}

#[implement(Sessions)]
pub async fn provider(&self, session: &Session) -> Result<Provider> {
	let Some(idp_id) = session.idp_id.as_deref() else {
		return Err!(Request(NotFound("No provider for this session")));
	};

	self.providers.get(idp_id).await
}
