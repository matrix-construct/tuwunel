use axum::extract::State;
use futures::{FutureExt, StreamExt};
use ruma::api::federation::space::get_hierarchy;
use tuwunel_core::{
	Err, Result,
	utils::stream::{BroadbandExt, IterStream},
};
use tuwunel_service::rooms::spaces::{Identifier, SummaryAccessibility, get_parent_children_via};

use crate::Ruma;

/// # `GET /_matrix/federation/v1/hierarchy/{roomId}`
///
/// Gets the space tree in a depth-first manner to locate child rooms of a given
/// space.
pub(crate) async fn get_hierarchy_route(
	State(services): State<crate::State>,
	body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
	if !services
		.rooms
		.metadata
		.exists(&body.room_id)
		.await
	{
		return Err!(Request(NotFound("Room does not exist.")));
	}

	let room_id = &body.room_id;
	let suggested_only = body.suggested_only;
	let ref identifier = Identifier::ServerName(body.origin());
	match services
		.rooms
		.spaces
		.get_summary_and_children_local(room_id, identifier)
		.await?
	{
		| None => Err!(Request(NotFound("The requested room was not found"))),

		| Some(SummaryAccessibility::Inaccessible) => {
			Err!(Request(NotFound("The requested room is inaccessible")))
		},

		| Some(SummaryAccessibility::Accessible(room)) => {
			let (children, inaccessible_children) =
				get_parent_children_via(&room, suggested_only)
					.stream()
					.broad_filter_map(|(child, _via)| async move {
						match services
							.rooms
							.spaces
							.get_summary_and_children_local(&child, identifier)
							.await
							.ok()?
						{
							| None => None,

							| Some(SummaryAccessibility::Inaccessible) => {
								Some((None, Some(child)))
							},

							| Some(SummaryAccessibility::Accessible(summary)) => {
								Some((Some(summary), None))
							},
						}
					})
					.unzip()
					.map(|(children, inaccessible_children): (Vec<_>, Vec<_>)| {
						(
							children
								.into_iter()
								.flatten()
								.map(Into::into)
								.collect(),
							inaccessible_children
								.into_iter()
								.flatten()
								.collect(),
						)
					})
					.await;

			Ok(get_hierarchy::v1::Response { room, children, inaccessible_children })
		},
	}
}
