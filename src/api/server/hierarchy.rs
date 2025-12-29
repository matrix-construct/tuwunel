use Accessibility::{Accessible, Inaccessible};
use Identifier::ServerName;
use axum::extract::State;
use futures::{FutureExt, StreamExt};
use get_hierarchy::v1::{Request, Response};
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
	body: Ruma<Request>,
) -> Result<Response> {
	if !services.metadata.exists(&body.room_id).await {
		return Err!(Request(NotFound("Room does not exist.")));
	}

	match services
		.spaces
		.get_summary_and_children(&body.room_id, &ServerName(body.origin()), &[])
		.await?
	{
		| Inaccessible => Err!(Request(NotFound("The requested room is inaccessible"))),
		| Accessible(room) => {
			let (children, inaccessible_children) =
				get_parent_children_via(&room, body.suggested_only)
					.stream()
					.broad_filter_map(async |(child, _via)| {
						match services
							.spaces
							.get_summary_and_children(&child, &ServerName(body.origin()), &[])
							.await
							.ok()?
						{
							| Inaccessible => Some((None, Some(child))),
							| Accessible(summary) => Some((Some(summary), None)),
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

			Ok(Response { room, children, inaccessible_children })
		},
	}
}
