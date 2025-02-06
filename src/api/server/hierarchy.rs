use axum::extract::State;
use conduwuit::{Err, Result};
use ruma::{api::federation::space::get_hierarchy, RoomId, ServerName};
use service::{
	rooms::spaces::{get_parent_children_via, Identifier, SummaryAccessibility},
	Services,
};

use crate::Ruma;

/// # `GET /_matrix/federation/v1/hierarchy/{roomId}`
///
/// Gets the space tree in a depth-first manner to locate child rooms of a given
/// space.
pub(crate) async fn get_hierarchy_route(
	State(services): State<crate::State>,
	body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
	if !services.rooms.metadata.exists(&body.room_id).await {
		return Err!(Request(NotFound("Room does not exist.")));
	}

	get_hierarchy(&services, &body.room_id, body.origin(), body.suggested_only).await
}

/// Gets the response for the space hierarchy over federation request
///
/// Errors if the room does not exist, so a check if the room exists should
/// be done
async fn get_hierarchy(
	services: &Services,
	room_id: &RoomId,
	server_name: &ServerName,
	suggested_only: bool,
) -> Result<get_hierarchy::v1::Response> {
	match services
		.rooms
		.spaces
		.get_summary_and_children_local(&room_id.to_owned(), Identifier::ServerName(server_name))
		.await?
	{
		| Some(SummaryAccessibility::Accessible(room)) => {
			let mut children = Vec::new();
			let mut inaccessible_children = Vec::new();

			for (child, _via) in get_parent_children_via(&room, suggested_only) {
				match services
					.rooms
					.spaces
					.get_summary_and_children_local(&child, Identifier::ServerName(server_name))
					.await?
				{
					| Some(SummaryAccessibility::Accessible(summary)) => {
						children.push((*summary).into());
					},
					| Some(SummaryAccessibility::Inaccessible) => {
						inaccessible_children.push(child);
					},
					| None => (),
				}
			}

			Ok(get_hierarchy::v1::Response {
				room: *room,
				children,
				inaccessible_children,
			})
		},
		| Some(SummaryAccessibility::Inaccessible) =>
			Err!(Request(NotFound("The requested room is inaccessible"))),
		| None => Err!(Request(NotFound("The requested room was not found"))),
	}
}
