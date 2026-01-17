use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::{FutureExt, StreamExt, TryFutureExt, future::join3, stream::FuturesUnordered};
use ruma::{
	OwnedServerName, RoomId, UserId,
	api::{
		client::room::get_summary,
		federation::space::{SpaceHierarchyParentSummary, get_hierarchy},
	},
	events::room::member::MembershipState,
	room::{JoinRuleSummary, RoomSummary},
};
use tuwunel_core::{
	Err, Result, debug_warn, trace,
	utils::{IterStream, future::TryExtExt, option::OptionExt},
};
use tuwunel_service::Services;

use crate::{Ruma, RumaResponse};

/// # `GET /_matrix/client/unstable/im.nheko.summary/rooms/{roomIdOrAlias}/summary`
///
/// Returns a short description of the state of a room.
///
/// This is the "wrong" endpoint that some implementations/clients may use
/// according to the MSC. Request and response bodies are the same as
/// `get_room_summary`.
///
/// An implementation of [MSC3266](https://github.com/matrix-org/matrix-spec-proposals/pull/3266)
pub(crate) async fn get_room_summary_legacy(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_summary::v1::Request>,
) -> Result<RumaResponse<get_summary::v1::Response>> {
	get_room_summary(State(services), InsecureClientIp(client), body)
		.boxed()
		.await
		.map(RumaResponse)
}

/// # `GET /_matrix/client/unstable/im.nheko.summary/summary/{roomIdOrAlias}`
///
/// Returns a short description of the state of a room.
///
/// An implementation of [MSC3266](https://github.com/matrix-org/matrix-spec-proposals/pull/3266)
#[tracing::instrument(skip_all, fields(%client), name = "room_summary")]
pub(crate) async fn get_room_summary(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_summary::v1::Request>,
) -> Result<get_summary::v1::Response> {
	let (room_id, servers) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, Some(&body.via))
		.await?;

	if services.metadata.is_banned(&room_id).await {
		return Err!(Request(Forbidden("This room is banned on this homeserver.")));
	}

	room_summary_response(&services, &room_id, &servers, body.sender_user.as_deref())
		.boxed()
		.await
}

async fn room_summary_response(
	services: &Services,
	room_id: &RoomId,
	servers: &[OwnedServerName],
	sender_user: Option<&UserId>,
) -> Result<get_summary::v1::Response> {
	if services
		.state_cache
		.server_in_room(services.globals.server_name(), room_id)
		.await
	{
		return local_room_summary_response(services, room_id, sender_user)
			.boxed()
			.await;
	}

	let summary = remote_room_summary_hierarchy_response(services, room_id, servers, sender_user)
		.await?
		.summary;

	Ok(get_summary::v1::Response {
		summary,
		membership: sender_user
			.is_some()
			.then_some(MembershipState::Leave),
	})
}

async fn local_room_summary_response(
	services: &Services,
	room_id: &RoomId,
	sender_user: Option<&UserId>,
) -> Result<get_summary::v1::Response> {
	trace!(?sender_user, "Sending local room summary response for {room_id:?}");
	let join_rule = services.state_accessor.get_join_rules(room_id);

	let world_readable = services.state_accessor.is_world_readable(room_id);

	let guest_can_join = services.state_accessor.guest_can_join(room_id);

	let (join_rule, world_readable, guest_can_join) =
		join3(join_rule, world_readable, guest_can_join).await;

	trace!("{join_rule:?}, {world_readable:?}, {guest_can_join:?}");
	user_can_see_summary(
		services,
		room_id,
		&join_rule.clone().into(),
		guest_can_join,
		world_readable,
		join_rule.allowed_room_ids(),
		sender_user,
	)
	.await?;

	let canonical_alias = services
		.state_accessor
		.get_canonical_alias(room_id)
		.ok();

	let name = services.state_accessor.get_name(room_id).ok();

	let topic = services
		.state_accessor
		.get_room_topic(room_id)
		.ok();

	let room_type = services
		.state_accessor
		.get_room_type(room_id)
		.ok();

	let avatar_url = services
		.state_accessor
		.get_avatar(room_id)
		.map_ok(|content| content.url)
		.ok()
		.map(Option::flatten);

	let room_version = services.state.get_room_version(room_id).ok();

	let encryption = services
		.state_accessor
		.get_room_encryption(room_id)
		.ok();

	let num_joined_members = services
		.state_cache
		.room_joined_count(room_id)
		.unwrap_or(0);

	let membership = sender_user.map_async(|sender_user| {
		services
			.state_accessor
			.get_member(room_id, sender_user)
			.map_ok_or(MembershipState::Leave, |content| content.membership)
	});

	let (
		canonical_alias,
		name,
		num_joined_members,
		topic,
		avatar_url,
		room_type,
		room_version,
		encryption,
		membership,
	) = futures::join!(
		canonical_alias,
		name,
		num_joined_members,
		topic,
		avatar_url,
		room_type,
		room_version,
		encryption,
		membership,
	);

	Ok(get_summary::v1::Response {
		summary: RoomSummary {
			room_id: room_id.to_owned(),
			canonical_alias,
			avatar_url,
			guest_can_join,
			name,
			num_joined_members: num_joined_members.try_into().unwrap_or_default(),
			topic,
			world_readable,
			room_type,
			room_version,
			encryption,
			join_rule: join_rule.into(),
		},
		membership,
	})
}

/// used by MSC3266 to fetch a room's info if we do not know about it
async fn remote_room_summary_hierarchy_response(
	services: &Services,
	room_id: &RoomId,
	servers: &[OwnedServerName],
	sender_user: Option<&UserId>,
) -> Result<SpaceHierarchyParentSummary> {
	trace!(?sender_user, ?servers, "Sending remote room summary response for {room_id:?}");
	if !services.config.allow_federation {
		return Err!(Request(Forbidden("Federation is disabled.")));
	}

	if services.metadata.is_disabled(room_id).await {
		return Err!(Request(Forbidden(
			"Federaton of room {room_id} is currently disabled on this server."
		)));
	}

	let request = get_hierarchy::v1::Request::new(room_id.to_owned());

	let mut requests: FuturesUnordered<_> = servers
		.iter()
		.map(|server| {
			services
				.federation
				.execute(server, request.clone())
		})
		.collect();

	while let Some(Ok(response)) = requests.next().await {
		trace!("{response:?}");
		let room = response.room.clone();
		let summary = &room.summary;
		if summary.room_id != room_id {
			debug_warn!(
				"Room ID {} returned does not belong to the requested room ID {}",
				summary.room_id,
				room_id
			);
			continue;
		}

		return user_can_see_summary(
			services,
			room_id,
			&summary.join_rule,
			summary.guest_can_join,
			summary.world_readable,
			summary.join_rule.allowed_room_ids(),
			sender_user,
		)
		.await
		.map(|()| room);
	}

	Err!(Request(NotFound(
		"Room is unknown to this server and was unable to fetch over federation with the \
		 provided servers available"
	)))
}

async fn user_can_see_summary<'a, I>(
	services: &Services,
	room_id: &RoomId,
	join_rule: &JoinRuleSummary,
	guest_can_join: bool,
	world_readable: bool,
	allowed_room_ids: I,
	sender_user: Option<&UserId>,
) -> Result
where
	I: Iterator<Item = &'a RoomId> + Send,
{
	let is_public_room = matches!(
		join_rule,
		JoinRuleSummary::Public | JoinRuleSummary::Knock | JoinRuleSummary::KnockRestricted(_)
	);

	match sender_user {
		| Some(sender_user) => {
			let user_can_see_state_events = services
				.state_accessor
				.user_can_see_state_events(sender_user, room_id);

			let is_guest = services
				.users
				.is_deactivated(sender_user)
				.unwrap_or(false);

			let user_in_allowed_restricted_room = allowed_room_ids
				.stream()
				.any(|room| services.state_cache.is_joined(sender_user, room));

			let (user_can_see_state_events, is_guest, user_in_allowed_restricted_room) =
				join3(user_can_see_state_events, is_guest, user_in_allowed_restricted_room)
					.boxed()
					.await;

			if user_can_see_state_events
				|| (is_guest && guest_can_join)
				|| is_public_room
				|| user_in_allowed_restricted_room
			{
				return Ok(());
			}

			Err!(Request(Forbidden(
				"Room is not world readable, not publicly accessible/joinable, restricted room \
				 conditions not met, and guest access is forbidden. Not allowed to see details \
				 of this room."
			)))
		},
		| None => {
			if is_public_room || world_readable {
				return Ok(());
			}

			Err!(Request(Forbidden(
				"Room is not world readable or publicly accessible/joinable, authentication is \
				 required"
			)))
		},
	}
}
