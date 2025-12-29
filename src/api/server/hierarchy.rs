use axum::extract::State;
use futures::{FutureExt, StreamExt};
use ruma::api::federation::space::get_hierarchy;
use tuwunel_core::{
	Err, Result,
	utils::stream::{BroadbandExt, IterStream},
};
use tuwunel_service::rooms::spaces::{Accessibility, Identifier, get_parent_children_via};

use crate::Ruma;

/// # `GET /_matrix/federation/v1/hierarchy/{roomId}`
///
/// Gets the space tree in a depth-first manner to locate child rooms of a given
/// space.
pub(crate) async fn get_hierarchy_route(
	State(services): State<crate::State>,
	body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
	if !services.metadata.exists(&body.room_id).await {
		return Err!(Request(NotFound("Room does not exist.")));
	}

	let room_id = &body.room_id;
	let suggested_only = body.suggested_only;
	let identifier = Identifier::ServerName(body.origin());
	match services
		.spaces
		.get_summary_and_children_local(room_id, identifier)
		.await?
	{
		| Accessibility::Inaccessible =>
			Err!(Request(NotFound("The requested room is inaccessible"))),

		| Accessibility::Accessible(room) => {
			let (children, inaccessible_children) =
				get_parent_children_via(&room, suggested_only)
					.stream()
					.broad_filter_map(async |(child, _via)| {
						let identifier = Identifier::ServerName(body.origin());
						match services
							.spaces
							.get_summary_and_children_local(&child, identifier)
							.await
							.ok()?
						{
							| Accessibility::Inaccessible => Some((None, Some(child))),
							| Accessibility::Accessible(summary) => Some((Some(summary), None)),
						}
					})
					.unzip()
					.map(|(children, inaccessible_children): (Vec<_>, Vec<_>)| {
						let children = children
							.into_iter()
							.flatten()
							.map(|parent| parent.summary)
							.collect();

						let inaccessible_children = inaccessible_children
							.into_iter()
							.flatten()
							.collect();

						(children, inaccessible_children)
					})
					.await;

			Ok(get_hierarchy::v1::Response { room, children, inaccessible_children })
		},
	}
}
