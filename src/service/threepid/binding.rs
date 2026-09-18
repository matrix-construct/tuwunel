//! Persistent email-to-user bindings.
//!
//! Forward rows retain binding metadata for each user, while reverse rows
//! resolve one canonical email to its owner. Streamed listing and point lookup
//! expose the two index directions to account and invitation flows.

use futures::{Stream, StreamExt};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
	thirdparty::{Medium, ThirdPartyIdentifier, ThirdPartyIdentifierInit},
};
use tuwunel_core::{Result, implement, result::NotFound, utils::stream::TryIgnore};
use tuwunel_database::{Cbor, Deserialized, Ignore, Interfix};

use super::Binding;

/// Persists a canonical email binding in both index directions.
///
/// The forward row stores the medium and timestamps, while the reverse row
/// stores the owning user. These are separate map writes, so callers must not
/// treat the two rows as one atomic snapshot.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip(self),
	fields(
		%user_id,
	),
)]
pub async fn put_binding(
	&self,
	user_id: &UserId,
	email_canon: &str,
	medium: Medium,
	validated_at: MilliSecondsSinceUnixEpoch,
	added_at: MilliSecondsSinceUnixEpoch,
) {
	let binding = Binding { medium, validated_at, added_at };

	self.db
		.userid_email
		.put((user_id, email_canon), Cbor(binding));

	self.db.email_userid.insert(email_canon, user_id);
}

/// Streams all third-party identifiers bound to `user_id`.
///
/// Entries are decoded lazily from the user's forward-index prefix. Storage or
/// decoding failures are skipped, and each yielded identifier owns its data.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip(self),
	fields(
		%user_id,
	),
)]
pub fn get_bindings<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = ThirdPartyIdentifier> + Send + 'a {
	type KeyVal = ((Ignore, String), Cbor<Binding>);

	self.db
		.userid_email
		.stream_prefix(&(user_id, Interfix))
		.ignore_err()
		.map(|((_, address), Cbor(binding)): KeyVal| {
			ThirdPartyIdentifierInit {
				address,
				medium: binding.medium,
				validated_at: binding.validated_at,
				added_at: binding.added_at,
			}
			.into()
		})
}

/// Removes a canonical email binding from both index directions.
///
/// The forward row is deleted even when absent. The reverse row is removed
/// only when a successful lookup still names this user, so a read failure or a
/// different owner leaves that row untouched.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip(self),
	fields(
		%user_id,
	),
)]
pub async fn del_binding(&self, user_id: &UserId, email_canon: &str) {
	self.db.userid_email.del((user_id, email_canon));

	if self
		.user_id_for_email(email_canon)
		.await
		.ok()
		.flatten()
		.is_some_and(|bound| bound == user_id)
	{
		self.db.email_userid.remove(email_canon);
	}
}

/// Whether a canonical email address is bound to an account other than this
/// one.
///
/// No binding or a binding to `user_id` returns `false`. Storage and decoding
/// failures from the reverse lookup are propagated.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip(self),
	fields(
		%user_id,
	),
)]
pub async fn bound_elsewhere(&self, user_id: &UserId, email_canon: &str) -> Result<bool> {
	self.user_id_for_email(email_canon)
		.await
		.map(|bound| bound.is_some_and(|bound| bound != user_id))
}

/// Returns the user bound to a canonical email address.
///
/// An absent reverse row returns `None`. Storage and user-ID decoding failures
/// are propagated.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn user_id_for_email(&self, email_canon: &str) -> Result<Option<OwnedUserId>> {
	self.db
		.email_userid
		.get(email_canon)
		.await
		.optional()?
		.map(|handle| handle.deserialized())
		.transpose()
}

/// Tests whether a canonical email address has a readable reverse row.
///
/// Any successful raw lookup returns `true` without decoding the stored user.
/// Absence and all storage failures are both collapsed to `false`.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn address_in_use(&self, email_canon: &str) -> bool {
	self.db
		.email_userid
		.get(email_canon)
		.await
		.is_ok()
}
