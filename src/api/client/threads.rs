use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::api::client::threads::get_threads;
use tuwunel_core::{
	Err, Result, at,
	result::{FlatOk, LogErr},
	utils::stream::TryWidebandExt,
};
use tuwunel_matrix::{
	Event,
	pdu::{PduCount, PduEvent},
};

use crate::Ruma;

/// # `GET /_matrix/client/r0/rooms/{roomId}/threads`
pub(crate) async fn get_threads_route(
	State(services): State<crate::State>,
	ref body: Ruma<get_threads::v1::Request>,
) -> Result<get_threads::v1::Response> {
	let sender_user = body.sender_user();
	let room_id = &body.room_id;

	if !services.metadata.exists(room_id).await {
		return Err!(Request(Forbidden("Room does not exist to this server")));
	}

	if !services
		.state_accessor
		.user_can_see_room(sender_user, room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this room.")));
	}

	// Use limit or else 10, with maximum 100
	let limit = body
		.limit
		.map(usize::try_from)
		.flat_ok()
		.unwrap_or(10)
		.min(100);

	let from: PduCount = body
		.from
		.as_deref()
		.map(str::parse)
		.transpose()?
		.unwrap_or_else(PduCount::max);

	// MSC3856: an ignored sender's root is served redacted.
	let ignored = services.users.ignored_users(sender_user).await;

	// One extra row probes whether the list continues past this page.
	let mut threads: Vec<(PduCount, PduEvent)> = services
		.threads
		.threads_until(sender_user, room_id, from, &body.include)
		.try_filter_map(async |(count, pdu)| {
			Ok(services
				.state_accessor
				.user_can_see_event(sender_user, &pdu)
				.await
				.then_some((count, pdu)))
		})
		.take(limit.saturating_add(1))
		.wide_and_then(async |(count, pdu)| {
			let root = services
				.pdu_metadata
				.redacted_root(ignored.as_ref(), &pdu)
				.await;

			let pdu = services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.await;

			Ok((count, apply_redacted_root(pdu, root)))
		})
		.try_collect()
		.await?;

	let more = threads.len() > limit;

	threads.truncate(limit);

	Ok(get_threads::v1::Response {
		next_batch: threads
			.last()
			.filter(|_| more)
			.map(at!(0))
			.as_ref()
			.map(ToString::to_string),

		chunk: threads
			.into_iter()
			.map(at!(1))
			.map(Event::into_format)
			.collect(),
	})
}

/// MSC3856: an ignored sender's root is served with its redacted content.
///
/// It keeps the `unsigned` the bundle pass served, minus any `m.replace`
/// bundle: a folded edit shares the root's sender, so it would re-serve the
/// ignored content.
fn apply_redacted_root(pdu: PduEvent, root: Option<PduEvent>) -> PduEvent {
	match root {
		| None => pdu,
		| Some(mut root) => {
			root.unsigned = pdu.unsigned;
			root.remove_replacement_bundle().log_err().ok();
			root
		},
	}
}
