use futures::{Stream, StreamExt};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
	thirdparty::{Medium, ThirdPartyIdentifier, ThirdPartyIdentifierInit},
};
use tuwunel_core::{Result, implement, result::NotFound, utils::stream::TryIgnore};
use tuwunel_database::{Cbor, Deserialized, Ignore, Interfix};

use super::Binding;

/// Persist a binding in both directions: the forward `(user, email)` row with
/// its metadata, and the reverse `email -> user` lookup.
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

/// All third-party identifiers bound to `user_id`, lazily decoded from the
/// `(user, email)` prefix scan.
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

/// Remove a binding in both directions; blind-delete, tolerant of an absent
/// row. The reverse lookup is removed only when it still maps to this user, so
/// one user's delete cannot wipe another's reverse row.
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
/// Separates rebinding an address a user already holds, which is permitted,
/// from claiming one another account owns, which is not. The lookup's own
/// error handling decides what an unreadable row does.
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

/// The user bound to a canonical email address, if any.
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

/// Whether a canonical email address is already bound to some user.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn address_in_use(&self, email_canon: &str) -> bool {
	self.db
		.email_userid
		.get(email_canon)
		.await
		.is_ok()
}
