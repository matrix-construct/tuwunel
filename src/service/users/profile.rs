use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::join3};
use ruma::{
	OwnedMxcUri, OwnedRoomId, UserId,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{
	Result, implement,
	matrix::PduBuilder,
	utils::{
		future::TryExtExt,
		stream::{IterStream, TryIgnore},
	},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json};

#[implement(super::Service)]
pub async fn update_displayname(
	&self,
	user_id: &UserId,
	displayname: Option<String>,
	rooms: &[OwnedRoomId],
) {
	let (current_avatar_url, current_blurhash, current_displayname) = join3(
		self.services.users.avatar_url(user_id).ok(),
		self.services.users.blurhash(user_id).ok(),
		self.services.users.displayname(user_id).ok(),
	)
	.await;

	if displayname == current_displayname {
		return;
	}

	self.services
		.users
		.set_displayname(user_id, displayname.clone());

	// Send a new join membership event into rooms
	let avatar_url = &current_avatar_url;
	let blurhash = &current_blurhash;
	let displayname = &displayname;
	let rooms: Vec<_> = rooms
		.iter()
		.try_stream()
		.and_then(async |room_id: &OwnedRoomId| {
			let pdu = PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				displayname: displayname.clone(),
				membership: MembershipState::Join,
				avatar_url: avatar_url.clone(),
				blurhash: blurhash.clone(),
				join_authorized_via_users_server: None,
				reason: None,
				is_direct: None,
				third_party_invite: None,
			});

			Ok((pdu, room_id))
		})
		.ignore_err()
		.collect()
		.await;

	self.update_all_rooms(user_id, rooms)
		.boxed()
		.await;
}

/// Sets a new displayname or removes it if displayname is None. You still
/// need to notify all rooms of this change.
#[implement(super::Service)]
pub fn set_displayname(&self, user_id: &UserId, displayname: Option<String>) {
	if let Some(displayname) = displayname {
		self.db
			.userid_displayname
			.insert(user_id, displayname);
	} else {
		self.db.userid_displayname.remove(user_id);
	}
}

/// Returns the displayname of a user on this homeserver.
#[implement(super::Service)]
pub async fn displayname(&self, user_id: &UserId) -> Result<String> {
	self.db
		.userid_displayname
		.get(user_id)
		.await
		.deserialized()
}

#[implement(super::Service)]
pub async fn update_avatar_url(
	&self,
	user_id: &UserId,
	avatar_url: Option<OwnedMxcUri>,
	blurhash: Option<String>,
	rooms: &[OwnedRoomId],
) {
	let (current_avatar_url, current_blurhash, current_displayname) = join3(
		self.services.users.avatar_url(user_id).ok(),
		self.services.users.blurhash(user_id).ok(),
		self.services.users.displayname(user_id).ok(),
	)
	.await;

	if current_avatar_url == avatar_url && current_blurhash == blurhash {
		return;
	}

	self.services
		.users
		.set_avatar_url(user_id, avatar_url.clone());
	self.services
		.users
		.set_blurhash(user_id, blurhash.clone());

	// Send a new join membership event into rooms
	let avatar_url = &avatar_url;
	let blurhash = &blurhash;
	let displayname = &current_displayname;
	let rooms: Vec<_> = rooms
		.iter()
		.try_stream()
		.and_then(async |room_id: &OwnedRoomId| {
			let pdu = PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				avatar_url: avatar_url.clone(),
				blurhash: blurhash.clone(),
				membership: MembershipState::Join,
				displayname: displayname.clone(),
				join_authorized_via_users_server: None,
				reason: None,
				is_direct: None,
				third_party_invite: None,
			});

			Ok((pdu, room_id))
		})
		.ignore_err()
		.collect()
		.await;

	self.update_all_rooms(user_id, rooms)
		.boxed()
		.await;
}

/// Sets a new avatar_url or removes it if avatar_url is None.
#[implement(super::Service)]
pub fn set_avatar_url(&self, user_id: &UserId, avatar_url: Option<OwnedMxcUri>) {
	match avatar_url {
		| Some(avatar_url) => {
			self.db
				.userid_avatarurl
				.insert(user_id, &avatar_url);
		},
		| _ => {
			self.db.userid_avatarurl.remove(user_id);
		},
	}
}

/// Get the `avatar_url` of a user.
#[implement(super::Service)]
pub async fn avatar_url(&self, user_id: &UserId) -> Result<OwnedMxcUri> {
	self.db
		.userid_avatarurl
		.get(user_id)
		.await
		.deserialized()
}

/// Sets a new avatar_url or removes it if avatar_url is None.
#[implement(super::Service)]
pub fn set_blurhash(&self, user_id: &UserId, blurhash: Option<String>) {
	if let Some(blurhash) = blurhash {
		self.db.userid_blurhash.insert(user_id, blurhash);
	} else {
		self.db.userid_blurhash.remove(user_id);
	}
}

/// Get the blurhash of a user.
#[implement(super::Service)]
pub async fn blurhash(&self, user_id: &UserId) -> Result<String> {
	self.db
		.userid_blurhash
		.get(user_id)
		.await
		.deserialized()
}

/// Sets a new timezone or removes it if timezone is None.
#[implement(super::Service)]
pub fn set_timezone(&self, user_id: &UserId, timezone: Option<String>) {
	// TODO: insert to the stable MSC4175 key when it's stable
	let key = (user_id, "us.cloke.msc4175.tz");

	if let Some(timezone) = timezone {
		self.db
			.useridprofilekey_value
			.put_raw(key, &timezone);
	} else {
		self.db.useridprofilekey_value.del(key);
	}
}

/// Get the timezone of a user.
#[implement(super::Service)]
pub async fn timezone(&self, user_id: &UserId) -> Result<String> {
	// TODO: transparently migrate unstable key usage to the stable key once MSC4133
	// and MSC4175 are stable, likely a remove/insert in this block.

	// first check the unstable prefix then check the stable prefix
	let unstable_key = (user_id, "us.cloke.msc4175.tz");
	let stable_key = (user_id, "m.tz");
	self.db
		.useridprofilekey_value
		.qry(&unstable_key)
		.or_else(|_| self.db.useridprofilekey_value.qry(&stable_key))
		.await
		.deserialized()
}

/// Gets all the user's profile keys and values in an iterator
#[implement(super::Service)]
pub fn all_profile_keys<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = (String, serde_json::Value)> + 'a + Send {
	type KeyVal = ((Ignore, String), serde_json::Value);

	let prefix = (user_id, Interfix);
	self.db
		.useridprofilekey_value
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((_, key), val): KeyVal| (key, val))
}

/// Sets a new profile key value, removes the key if value is None
#[implement(super::Service)]
pub fn set_profile_key(
	&self,
	user_id: &UserId,
	profile_key: &str,
	profile_key_value: Option<serde_json::Value>,
) {
	// TODO: insert to the stable MSC4175 key when it's stable
	let key = (user_id, profile_key);

	if let Some(value) = profile_key_value {
		self.db
			.useridprofilekey_value
			.put(key, Json(value));
	} else {
		self.db.useridprofilekey_value.del(key);
	}
}

/// Gets a specific user profile key
#[implement(super::Service)]
pub async fn profile_key(
	&self,
	user_id: &UserId,
	profile_key: &str,
) -> Result<serde_json::Value> {
	let key = (user_id, profile_key);
	self.db
		.useridprofilekey_value
		.qry(&key)
		.await
		.deserialized()
}
